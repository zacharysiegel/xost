use std::collections::BTreeMap;
use std::os::unix::ffi::OsStrExt;
use std::path::Path;
use std::path::PathBuf;

use jiff::Timestamp;
use jiff::ZonedDateTime;
use jiff::tz::TimeZone;
use rusqlite::Connection;
use rusqlite::OpenFlags;
use rusqlite::OptionalExtension;
use rusqlite::params;

use crate::browser::LoginState;
use crate::browser::Session;
use crate::clock;
use crate::config;
use crate::config::Config;
use crate::config::NamedSection;
use crate::config::Section;
use crate::cron;
use crate::cron::CronSchedule;
use crate::cron::NextRun;
use crate::error::AppError;
use crate::queue;
use crate::queue::QueuedTweet;
use crate::queue::StuckRecord;
use crate::schedule;
use crate::schedule::JobLoading;

const CHROME_PROFILE_DIRECTORY: &str = "Default";
const CHROME_COOKIES_FILE: &str = "Cookies";
const CHROME_SINGLETON_LOCK_FILE: &str = "SingletonLock";
const SESSION_COOKIE_HOST: &str = ".x.com";
const SESSION_COOKIE_NAME: &str = "auth_token";
// Chrome stores cookie times as microseconds since 1601-01-01T00:00:00Z.
const MICROSECONDS_FROM_1601_TO_UNIX_EPOCH: i64 = 11_644_473_600_000_000;
const SECONDS_PER_DAY: i64 = 86_400;
const MAX_PREVIEW_CHARACTERS: usize = 160;
const PREVIEW_ELLIPSIS: &str = "...";
const DISPLAYED_TIME_FORMAT: &str = "%Y-%m-%d %H:%M %Z";
const FIELD_INDENT: &str = "  ";
const APP_ERROR_MESSAGE_PREFIX: &str = "Error: ";
const STUCK_RECORD_INSTRUCTION: &str = "Confirm on X whether each stuck record posted, then clear its posting_started_at.";

pub enum LoginCheckRequest {
    Skip,
    Perform,
}

pub struct AccountStatus {
    pub account: String,
    pub config_validity: ConfigValidity,
    pub schedule_status: ScheduleStatus,
    pub queue_status: QueueStatus,
    pub session_cookie_status: SessionCookieStatus,
    pub login_status: LoginStatus,
}

pub enum ConfigValidity {
    Valid,
    Invalid(String),
}

pub struct ScheduleStatus {
    pub schedule_expression: ScheduleExpression,
    pub plist_installation: PlistInstallation,
    pub job_loading: JobLoading,
}

pub enum ScheduleExpression {
    Absent,
    Invalid { expression: String },
    Valid { expression: String, next_run: NextRun },
}

pub enum PlistInstallation {
    Installed(String),
    NotInstalled,
}

pub enum QueueStatus {
    NotConfigured,
    Configured { db_path: PathBuf, queue_state: QueueState },
}

pub enum QueueState {
    Missing,
    Unreadable(String),
    MissingColumns(Vec<String>),
    Summarized(QueueSummary),
}

#[derive(Debug, PartialEq)]
pub struct QueueSummary {
    pub record_counts: RecordCounts,
    pub next_record: Option<NextRecord>,
    pub most_recent_posted_at: Option<String>,
    pub stuck_records: Vec<StuckRecord>,
    pub failed_records: Vec<FailedRecord>,
}

#[derive(Debug, PartialEq)]
pub struct RecordCounts {
    pub total: i64,
    pub enabled: i64,
    pub disabled: i64,
    pub posted: i64,
    pub never_posted: i64,
    pub failed: i64,
}

#[derive(Debug, PartialEq)]
pub struct NextRecord {
    pub id: i64,
    pub queue_order: i64,
    pub heading: Option<String>,
}

#[derive(Debug, PartialEq)]
pub struct FailedRecord {
    pub id: i64,
    pub heading: Option<String>,
    pub last_error: Option<String>,
}

pub enum SessionCookieStatus {
    ProfileNotConfigured,
    CookiesFileMissing,
    Absent,
    EndsWithBrowserSession,
    ExpiresAt(Timestamp),
    Unreadable(String),
}

pub enum LoginStatus {
    NotChecked,
    LoggedIn,
    LoggedOut,
    NotCheckable(String),
    CheckFailed(String),
}

pub async fn run(config_path: &Path, requested_account: Option<&str>, login_check_request: LoginCheckRequest) -> Result<(), AppError> {
    let config_text: String = config::read_required_config(config_path)?;
    let sections: Vec<Section> = config::parse(&config_text)?;
    let account_sections: Vec<NamedSection<'_>> = select_account_sections(&sections, requested_account)?;

    if account_sections.is_empty() {
        println!("the config has no {} sections", config::account_section_header("<name>"));

        return Ok(());
    }

    let now: ZonedDateTime = Timestamp::now().to_zoned_datetime(TimeZone::system());

    for (account_index, account_section) in account_sections.iter().enumerate() {
        let account_status: AccountStatus = observe_account(config_path, account_section, &now, &login_check_request).await;

        if account_index > 0 {
            println!();
        }

        print!("{}", render_account_status(&account_status, &now));
    }

    Ok(())
}

pub fn select_account_sections<'a>(sections: &'a [Section], requested_account: Option<&str>) -> Result<Vec<NamedSection<'a>>, AppError> {
    let account_sections: Vec<NamedSection<'a>> = config::find_named_sections(sections, config::ACCOUNT_SECTION);

    let Some(requested_account) = requested_account else {
        return Ok(account_sections);
    };

    let requested_section: Option<NamedSection<'a>> =
        account_sections.into_iter().find(|account_section| account_section.name == requested_account);

    match requested_section {
        Some(account_section) => Ok(vec![account_section]),
        None => Err(AppError::new(&format!(
            "no {} section in config; defined accounts: {}",
            config::account_section_header(requested_account),
            config::account_names(sections),
        ))),
    }
}

async fn observe_account(
    config_path: &Path,
    account_section: &NamedSection<'_>,
    now: &ZonedDateTime,
    login_check_request: &LoginCheckRequest,
) -> AccountStatus {
    let config_load: Result<Config, AppError> = Config::load(config_path, account_section.name);

    let config_validity: ConfigValidity = match &config_load {
        Ok(_) => ConfigValidity::Valid,
        Err(error) => ConfigValidity::Invalid(get_error_message(error)),
    };

    let schedule_status: ScheduleStatus = observe_schedule(account_section, now);
    let queue_status: QueueStatus = observe_queue(account_section.values);
    let session_cookie_status: SessionCookieStatus = observe_session_cookie(account_section.values);
    let login_status: LoginStatus = observe_login(&config_load, login_check_request).await;

    AccountStatus {
        account: account_section.name.to_string(),
        config_validity,
        schedule_status,
        queue_status,
        session_cookie_status,
        login_status,
    }
}

fn observe_schedule(account_section: &NamedSection<'_>, now: &ZonedDateTime) -> ScheduleStatus {
    let label: String = schedule::job_label(account_section.name);
    let installed_plist_path: String = schedule::installed_plist_path(&label);
    let plist_present: bool = Path::new(&installed_plist_path).exists();

    let plist_installation: PlistInstallation = match plist_present {
        true => PlistInstallation::Installed(installed_plist_path),
        false => PlistInstallation::NotInstalled,
    };

    ScheduleStatus {
        schedule_expression: get_schedule_expression(account_section.values, now),
        plist_installation,
        job_loading: schedule::read_job_loading(&label),
    }
}

fn get_schedule_expression(account_values: &BTreeMap<String, String>, now: &ZonedDateTime) -> ScheduleExpression {
    let Some(expression) = account_values.get(config::SCHEDULE_KEY) else {
        return ScheduleExpression::Absent;
    };

    let parse_result: Result<CronSchedule, AppError> = CronSchedule::parse(expression);

    match parse_result {
        Ok(cron_schedule) => ScheduleExpression::Valid {
            expression: expression.clone(),
            next_run: cron::get_next_run_after(&cron_schedule, now),
        },
        Err(_error) => ScheduleExpression::Invalid {
            expression: expression.clone(),
        },
    }
}

fn observe_queue(account_values: &BTreeMap<String, String>) -> QueueStatus {
    let Some(db_path) = config::get_configured_path(account_values, config::DB_KEY) else {
        return QueueStatus::NotConfigured;
    };

    let queue_state: QueueState = observe_queue_state(&db_path);

    QueueStatus::Configured { db_path, queue_state }
}

fn observe_queue_state(db_path: &Path) -> QueueState {
    let db_present: bool = db_path.exists();

    if !db_present {
        return QueueState::Missing;
    }

    let open_result: rusqlite::Result<Connection> = Connection::open_with_flags(db_path, OpenFlags::SQLITE_OPEN_READ_ONLY);

    let connection: Connection = match open_result {
        Ok(connection) => connection,
        Err(error) => return QueueState::Unreadable(error.to_string()),
    };

    let queue_state_result: Result<QueueState, AppError> = read_queue_state(&connection);

    queue_state_result.unwrap_or_else(|error| QueueState::Unreadable(get_error_message(&error)))
}

fn read_queue_state(connection: &Connection) -> Result<QueueState, AppError> {
    let missing_columns: Vec<String> = queue::read_missing_columns(connection)?;

    if !missing_columns.is_empty() {
        return Ok(QueueState::MissingColumns(missing_columns));
    }

    let queue_summary: QueueSummary = summarize_queue(connection)?;

    Ok(QueueState::Summarized(queue_summary))
}

pub fn summarize_queue(connection: &Connection) -> Result<QueueSummary, AppError> {
    let record_counts: RecordCounts = read_record_counts(connection)?;
    let next_record: Option<NextRecord> = read_next_record(connection)?;
    let most_recent_posted_at: Option<String> =
        connection.query_row("select max(last_posted_at) from tweet_queue", [], |record| record.get(0))?;
    let stuck_records: Vec<StuckRecord> = queue::read_stuck_records(connection)?;
    let failed_records: Vec<FailedRecord> = read_failed_records(connection)?;

    Ok(QueueSummary {
        record_counts,
        next_record,
        most_recent_posted_at,
        stuck_records,
        failed_records,
    })
}

fn read_record_counts(connection: &Connection) -> Result<RecordCounts, AppError> {
    let record_counts: RecordCounts = connection.query_row(
        "select count(*),
                coalesce(sum(enabled = 1), 0),
                coalesce(sum(enabled != 1), 0),
                coalesce(sum(last_posted_at is not null), 0),
                coalesce(sum(last_posted_at is null), 0),
                coalesce(sum(failed_at is not null), 0)
         from tweet_queue",
        [],
        |record| {
            Ok(RecordCounts {
                total: record.get(0)?,
                enabled: record.get(1)?,
                disabled: record.get(2)?,
                posted: record.get(3)?,
                never_posted: record.get(4)?,
                failed: record.get(5)?,
            })
        },
    )?;

    Ok(record_counts)
}

fn read_next_record(connection: &Connection) -> Result<Option<NextRecord>, AppError> {
    let next_tweet_lookup: Option<QueuedTweet> = queue::select_next(connection)?;

    let Some(next_tweet) = next_tweet_lookup else {
        return Ok(None);
    };

    let queue_order: i64 = connection.query_row(
        "select queue_order from tweet_queue where id = ?1",
        params![next_tweet.id],
        |record| record.get(0),
    )?;

    Ok(Some(NextRecord {
        id: next_tweet.id,
        queue_order,
        heading: next_tweet.heading,
    }))
}

fn read_failed_records(connection: &Connection) -> Result<Vec<FailedRecord>, AppError> {
    let mut statement: rusqlite::Statement<'_> =
        connection.prepare("select id, heading, last_error from tweet_queue where failed_at is not null order by id asc")?;

    let failed_records: Vec<FailedRecord> = statement
        .query_map([], |record| {
            Ok(FailedRecord {
                id: record.get(0)?,
                heading: record.get(1)?,
                last_error: record.get(2)?,
            })
        })?
        .collect::<rusqlite::Result<Vec<FailedRecord>>>()?;

    Ok(failed_records)
}

fn observe_session_cookie(account_values: &BTreeMap<String, String>) -> SessionCookieStatus {
    let Some(profile_dir) = config::get_configured_path(account_values, config::PROFILE_KEY) else {
        return SessionCookieStatus::ProfileNotConfigured;
    };

    let cookies_path: PathBuf = profile_dir.join(CHROME_PROFILE_DIRECTORY).join(CHROME_COOKIES_FILE);
    let cookies_present: bool = cookies_path.exists();

    if !cookies_present {
        return SessionCookieStatus::CookiesFileMissing;
    }

    let read_result: Result<SessionCookieStatus, AppError> = read_session_cookie(&cookies_path);

    read_result.unwrap_or_else(|error| SessionCookieStatus::Unreadable(get_error_message(&error)))
}

// immutable=1 reads without locking; a running Chrome holds this database's lock exclusively.
fn read_session_cookie(cookies_path: &Path) -> Result<SessionCookieStatus, AppError> {
    let cookies_uri: String = format!("file:{}?immutable=1", get_percent_encoded_path(cookies_path));
    let connection: Connection =
        Connection::open_with_flags(&cookies_uri, OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_URI)?;

    let session_cookie: Option<(i64, i64)> = connection
        .query_row(
            "select has_expires, expires_utc from cookies where host_key = ?1 and name = ?2 order by expires_utc desc limit 1",
            params![SESSION_COOKIE_HOST, SESSION_COOKIE_NAME],
            |record| Ok((record.get(0)?, record.get(1)?)),
        )
        .optional()?;

    let session_cookie_status: SessionCookieStatus = match session_cookie {
        None => SessionCookieStatus::Absent,
        Some((0, _)) => SessionCookieStatus::EndsWithBrowserSession,
        Some((_, expires_utc)) => SessionCookieStatus::ExpiresAt(get_chrome_cookie_expiry(expires_utc)?),
    };

    Ok(session_cookie_status)
}

pub fn get_chrome_cookie_expiry(expires_utc_microseconds: i64) -> Result<Timestamp, AppError> {
    let unix_microseconds: i64 = expires_utc_microseconds - MICROSECONDS_FROM_1601_TO_UNIX_EPOCH;
    let expiry: Timestamp = Timestamp::from_microsecond(unix_microseconds)?;

    Ok(expiry)
}

pub fn get_percent_encoded_path(path: &Path) -> String {
    let mut encoded_path: String = String::new();

    for byte in path.as_os_str().as_bytes() {
        let unreserved: bool = byte.is_ascii_alphanumeric() || b"-._~/".contains(byte);

        match unreserved {
            true => encoded_path.push(char::from(*byte)),
            false => encoded_path.push_str(&format!("%{:02X}", byte)),
        }
    }

    encoded_path
}

async fn observe_login(config_load: &Result<Config, AppError>, login_check_request: &LoginCheckRequest) -> LoginStatus {
    if let LoginCheckRequest::Skip = login_check_request {
        return LoginStatus::NotChecked;
    }

    match config_load {
        Ok(config) => check_login(config).await,
        Err(_) => LoginStatus::NotCheckable("the account's config section is invalid".to_string()),
    }
}

async fn check_login(config: &Config) -> LoginStatus {
    let profile_present: bool = config.profile_dir.is_dir();

    if !profile_present {
        return LoginStatus::NotCheckable("the Chrome profile directory does not exist".to_string());
    }

    let singleton_lock_metadata: std::io::Result<std::fs::Metadata> =
        std::fs::symlink_metadata(config.profile_dir.join(CHROME_SINGLETON_LOCK_FILE));

    if singleton_lock_metadata.is_ok() {
        return LoginStatus::NotCheckable("the Chrome profile is in use by another Chrome process".to_string());
    }

    let launch_result: Result<Session, AppError> = Session::launch(config).await;

    let session: Session = match launch_result {
        Ok(session) => session,
        Err(error) => return LoginStatus::CheckFailed(format!("could not launch Chrome: {}", get_error_message(&error))),
    };

    let login_state_result: Result<LoginState, AppError> = session.observe_login_state().await;

    session.close().await.ok();

    match login_state_result {
        Ok(LoginState::LoggedIn) => LoginStatus::LoggedIn,
        Ok(LoginState::LoggedOut) => LoginStatus::LoggedOut,
        Err(error) => LoginStatus::CheckFailed(get_error_message(&error)),
    }
}

pub fn render_account_status(account_status: &AccountStatus, now: &ZonedDateTime) -> String {
    let time_zone: &TimeZone = now.time_zone();
    let mut lines: Vec<String> = vec![account_status.account.clone()];

    push_field(&mut lines, "Config", &render_config_validity(&account_status.config_validity));
    push_schedule_fields(&mut lines, &account_status.schedule_status, time_zone);
    push_queue_fields(&mut lines, &account_status.queue_status, time_zone);
    push_field(&mut lines, "Session cookie", &render_session_cookie_status(&account_status.session_cookie_status, now));

    if let Some(login) = render_login_status(&account_status.login_status) {
        push_field(&mut lines, "Login", &login);
    }

    let mut rendered: String = lines.join("\n");
    rendered.push('\n');

    rendered
}

fn push_field(lines: &mut Vec<String>, label: &str, value: &str) {
    lines.push(format!("{}{}: {}", FIELD_INDENT, label, value));
}

fn render_config_validity(config_validity: &ConfigValidity) -> String {
    match config_validity {
        ConfigValidity::Valid => "valid".to_string(),
        ConfigValidity::Invalid(error) => format!("invalid: {}", get_single_line_preview(error)),
    }
}

fn push_schedule_fields(lines: &mut Vec<String>, schedule_status: &ScheduleStatus, time_zone: &TimeZone) {
    match &schedule_status.schedule_expression {
        ScheduleExpression::Absent => push_field(lines, "Schedule", "none"),
        ScheduleExpression::Invalid { expression } => {
            push_field(lines, "Schedule", &format!("{} (invalid; see Config)", expression));
        }
        ScheduleExpression::Valid { expression, next_run } => {
            push_field(lines, "Schedule", expression);
            push_field(lines, "Next run", &render_next_run(next_run, time_zone));
        }
    }

    let plist_installation: String = match &schedule_status.plist_installation {
        PlistInstallation::Installed(installed_plist_path) => format!("installed at {}", installed_plist_path),
        PlistInstallation::NotInstalled => "not installed".to_string(),
    };

    push_field(lines, "Plist", &plist_installation);

    let job_loading: &str = match schedule_status.job_loading {
        JobLoading::Loaded => "loaded",
        JobLoading::NotLoaded => "not loaded",
        JobLoading::Unknown => "unknown",
    };

    push_field(lines, "Job", job_loading);
}

fn render_next_run(next_run: &NextRun, time_zone: &TimeZone) -> String {
    match next_run {
        NextRun::At(next_run_time) => render_time(next_run_time.timestamp(), time_zone),
        NextRun::NoneWithinHorizon => "none within a year".to_string(),
    }
}

fn push_queue_fields(lines: &mut Vec<String>, queue_status: &QueueStatus, time_zone: &TimeZone) {
    let QueueStatus::Configured { db_path, queue_state } = queue_status else {
        push_field(lines, "Queue", "no db path configured");

        return;
    };

    let db_path_display: std::path::Display<'_> = db_path.display();

    match queue_state {
        QueueState::Missing => push_field(lines, "Queue", &format!("{} (missing)", db_path_display)),
        QueueState::Unreadable(error) => {
            push_field(lines, "Queue", &format!("{} (unreadable: {})", db_path_display, get_single_line_preview(error)));
        }
        QueueState::MissingColumns(missing_columns) => {
            push_field(lines, "Queue", &format!("{} (missing columns: {})", db_path_display, missing_columns.join(", ")));
        }
        QueueState::Summarized(queue_summary) => {
            push_field(lines, "Queue", &db_path_display.to_string());
            push_queue_summary_fields(lines, queue_summary, time_zone);
        }
    }
}

fn push_queue_summary_fields(lines: &mut Vec<String>, queue_summary: &QueueSummary, time_zone: &TimeZone) {
    let record_counts: &RecordCounts = &queue_summary.record_counts;

    push_field(
        lines,
        "Records",
        &format!(
            "{} total, {} enabled, {} disabled, {} posted, {} never posted, {} failed",
            record_counts.total,
            record_counts.enabled,
            record_counts.disabled,
            record_counts.posted,
            record_counts.never_posted,
            record_counts.failed,
        ),
    );

    let next_record: String = match &queue_summary.next_record {
        Some(next_record) => format!(
            "id {}, order {}{}",
            next_record.id,
            next_record.queue_order,
            render_heading_suffix(next_record.heading.as_deref()),
        ),
        None => "none".to_string(),
    };

    push_field(lines, "Next record", &next_record);

    let last_posted: String = match &queue_summary.most_recent_posted_at {
        Some(posted_at) => render_stored_time(posted_at, time_zone),
        None => "never".to_string(),
    };

    push_field(lines, "Last posted", &last_posted);

    for stuck_record in &queue_summary.stuck_records {
        push_field(
            lines,
            "STUCK MID-POST",
            &format!(
                "id {}{}, since {}",
                stuck_record.id,
                render_heading_suffix(stuck_record.heading.as_deref()),
                render_stored_time(&stuck_record.posting_started_at, time_zone),
            ),
        );
    }

    if !queue_summary.stuck_records.is_empty() {
        lines.push(format!("{}{}", FIELD_INDENT, STUCK_RECORD_INSTRUCTION));
    }

    for failed_record in &queue_summary.failed_records {
        let last_error: String = match &failed_record.last_error {
            Some(last_error) => get_single_line_preview(last_error),
            None => "none recorded".to_string(),
        };

        push_field(
            lines,
            "Failed record",
            &format!(
                "id {}{}, error: {}",
                failed_record.id,
                render_heading_suffix(failed_record.heading.as_deref()),
                last_error,
            ),
        );
    }
}

fn render_heading_suffix(heading: Option<&str>) -> String {
    match heading {
        Some(heading) => format!(", heading \"{}\"", get_single_line_preview(heading)),
        None => String::new(),
    }
}

fn render_session_cookie_status(session_cookie_status: &SessionCookieStatus, now: &ZonedDateTime) -> String {
    match session_cookie_status {
        SessionCookieStatus::ProfileNotConfigured => "unknown (no profile configured)".to_string(),
        SessionCookieStatus::CookiesFileMissing => "no session cookie (the profile has no Cookies file)".to_string(),
        SessionCookieStatus::Absent => "no session cookie".to_string(),
        SessionCookieStatus::EndsWithBrowserSession => "present, expires with the browser session".to_string(),
        SessionCookieStatus::ExpiresAt(expiry) => render_cookie_expiry(*expiry, now),
        SessionCookieStatus::Unreadable(error) => format!("unreadable: {}", get_single_line_preview(error)),
    }
}

fn render_cookie_expiry(expiry: Timestamp, now: &ZonedDateTime) -> String {
    let expiry_display: String = render_time(expiry, now.time_zone());
    let seconds_until_expiry: i64 = expiry.as_second() - now.timestamp().as_second();

    match seconds_until_expiry > 0 {
        true => format!("expires {}, {} days left", expiry_display, seconds_until_expiry / SECONDS_PER_DAY),
        false => format!("expired {}, {} days ago", expiry_display, -seconds_until_expiry / SECONDS_PER_DAY),
    }
}

fn render_login_status(login_status: &LoginStatus) -> Option<String> {
    match login_status {
        LoginStatus::NotChecked => None,
        LoginStatus::LoggedIn => Some("logged in".to_string()),
        LoginStatus::LoggedOut => Some("logged out".to_string()),
        LoginStatus::NotCheckable(reason) => Some(format!("not checked; {}", reason)),
        LoginStatus::CheckFailed(error) => Some(format!("check failed: {}", get_single_line_preview(error))),
    }
}

fn render_time(timestamp: Timestamp, time_zone: &TimeZone) -> String {
    timestamp.to_zoned_datetime(time_zone.clone()).strftime(DISPLAYED_TIME_FORMAT).to_string()
}

fn render_stored_time(stored_time: &str, time_zone: &TimeZone) -> String {
    let parse_result: Result<Timestamp, AppError> = clock::parse_rfc3339(stored_time);

    match parse_result {
        Ok(timestamp) => render_time(timestamp, time_zone),
        Err(_) => stored_time.to_string(),
    }
}

fn get_error_message(error: &AppError) -> String {
    let message: &str = error.message.strip_prefix(APP_ERROR_MESSAGE_PREFIX).unwrap_or(&error.message);

    message.to_string()
}

pub fn get_single_line_preview(text: &str) -> String {
    let single_line: String = text.split_whitespace().collect::<Vec<&str>>().join(" ");
    let character_count: usize = single_line.chars().count();

    if character_count <= MAX_PREVIEW_CHARACTERS {
        return single_line;
    }

    let kept_characters: String = single_line.chars().take(MAX_PREVIEW_CHARACTERS - PREVIEW_ELLIPSIS.len()).collect();

    format!("{}{}", kept_characters, PREVIEW_ELLIPSIS)
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE_CONFIG: &str = "\
[secrets]
  store = /tmp/secrets.yaml
  key = AAAABBBB

[account \"first\"]
  handle = first_handle

[account \"second\"]
  handle = second_handle
";

    fn create_fresh_queue() -> Connection {
        let connection: Connection = Connection::open_in_memory().unwrap();
        queue::init_schema(&connection).unwrap();

        connection
    }

    fn get_selected_account_names(requested_account: Option<&str>) -> Result<Vec<String>, AppError> {
        let sections: Vec<Section> = config::parse(SAMPLE_CONFIG).unwrap();
        let account_sections: Vec<NamedSection<'_>> = select_account_sections(&sections, requested_account)?;

        Ok(account_sections.into_iter().map(|account_section| account_section.name.to_string()).collect())
    }

    #[test]
    fn select_account_sections_returns_every_account_in_file_order_without_a_requested_account() {
        assert_eq!(get_selected_account_names(None).unwrap(), vec!["first", "second"]);
    }

    #[test]
    fn select_account_sections_returns_only_the_requested_account() {
        assert_eq!(get_selected_account_names(Some("second")).unwrap(), vec!["second"]);
    }

    #[test]
    fn select_account_sections_rejects_an_absent_account() {
        assert!(get_selected_account_names(Some("missing")).is_err());
    }

    #[test]
    fn get_chrome_cookie_expiry_converts_microseconds_since_1601_to_a_timestamp() {
        let expiry: Timestamp = get_chrome_cookie_expiry(13_470_473_600_000_000).unwrap();

        assert_eq!(expiry, "2027-11-12T06:13:20Z".parse::<Timestamp>().unwrap());
    }

    #[test]
    fn get_percent_encoded_path_escapes_spaces_and_uri_delimiters() {
        let encoded_path: String = get_percent_encoded_path(Path::new("/Users/a b/c?d#e%f/Cookies"));

        assert_eq!(encoded_path, "/Users/a%20b/c%3Fd%23e%25f/Cookies");
    }

    #[test]
    fn get_single_line_preview_joins_lines_with_single_spaces() {
        assert_eq!(get_single_line_preview("editor\n  lost text"), "editor lost text");
    }

    #[test]
    fn get_single_line_preview_truncates_long_text_with_an_ellipsis() {
        let long_text: String = "x".repeat(MAX_PREVIEW_CHARACTERS + 1);

        let preview: String = get_single_line_preview(&long_text);

        assert_eq!(preview, format!("{}{}", "x".repeat(MAX_PREVIEW_CHARACTERS - PREVIEW_ELLIPSIS.len()), PREVIEW_ELLIPSIS));
    }

    #[test]
    fn summarize_queue_counts_records_by_state() {
        let connection: Connection = create_fresh_queue();
        let posted_id: i64 = queue::enqueue(&connection, None, "posted", None, None).unwrap();
        let disabled_id: i64 = queue::enqueue(&connection, None, "disabled", None, None).unwrap();
        let failed_id: i64 = queue::enqueue(&connection, None, "failed", None, None).unwrap();
        queue::enqueue(&connection, None, "waiting", None, None).unwrap();
        queue::mark_posted(&connection, posted_id).unwrap();
        connection.execute("update tweet_queue set enabled = 0 where id = ?1", params![disabled_id]).unwrap();
        queue::mark_failed(&connection, failed_id, "boom").unwrap();

        let queue_summary: QueueSummary = summarize_queue(&connection).unwrap();

        let expected_record_counts: RecordCounts = RecordCounts {
            total: 4,
            enabled: 3,
            disabled: 1,
            posted: 1,
            never_posted: 3,
            failed: 1,
        };
        assert_eq!(queue_summary.record_counts, expected_record_counts);
    }

    #[test]
    fn summarize_queue_reports_the_record_select_next_picks() {
        let connection: Connection = create_fresh_queue();
        let posted_id: i64 = queue::enqueue(&connection, Some("1."), "a", None, Some(1)).unwrap();
        let next_id: i64 = queue::enqueue(&connection, Some("2."), "b", None, Some(5)).unwrap();
        queue::mark_posted(&connection, posted_id).unwrap();

        let queue_summary: QueueSummary = summarize_queue(&connection).unwrap();

        let expected_next_record: NextRecord = NextRecord {
            id: next_id,
            queue_order: 5,
            heading: Some("2.".to_string()),
        };
        assert_eq!(queue_summary.next_record, Some(expected_next_record));
    }

    #[test]
    fn summarize_queue_reports_the_most_recent_post_time() {
        let connection: Connection = create_fresh_queue();
        queue::enqueue(&connection, None, "a", None, None).unwrap();
        queue::enqueue(&connection, None, "b", None, None).unwrap();
        connection
            .execute_batch(
                "update tweet_queue set last_posted_at = '2026-11-01T09:00:00Z' where body = 'a';
                 update tweet_queue set last_posted_at = '2026-11-02T09:00:00Z' where body = 'b';",
            )
            .unwrap();

        let queue_summary: QueueSummary = summarize_queue(&connection).unwrap();

        assert_eq!(queue_summary.most_recent_posted_at.as_deref(), Some("2026-11-02T09:00:00Z"));
    }

    #[test]
    fn summarize_queue_lists_failed_records_with_their_errors() {
        let connection: Connection = create_fresh_queue();
        let failed_id: i64 = queue::enqueue(&connection, Some("1."), "a", None, None).unwrap();
        queue::enqueue(&connection, None, "b", None, None).unwrap();
        queue::mark_failed(&connection, failed_id, "boom").unwrap();

        let queue_summary: QueueSummary = summarize_queue(&connection).unwrap();

        let expected_failed_records: Vec<FailedRecord> = vec![FailedRecord {
            id: failed_id,
            heading: Some("1.".to_string()),
            last_error: Some("boom".to_string()),
        }];
        assert_eq!(queue_summary.failed_records, expected_failed_records);
    }

    #[test]
    fn render_account_status_renders_one_account_block() {
        let now: ZonedDateTime = clock::get_utc_time("2026-11-15T10:00:00Z");
        let account_status: AccountStatus = AccountStatus {
            account: "daily".to_string(),
            config_validity: ConfigValidity::Valid,
            schedule_status: ScheduleStatus {
                schedule_expression: ScheduleExpression::Valid {
                    expression: "0 9 * * *".to_string(),
                    next_run: NextRun::At(clock::get_utc_time("2026-11-16T09:00:00Z")),
                },
                plist_installation: PlistInstallation::Installed("/Library/LaunchDaemons/ro.zach.xost.daily.plist".to_string()),
                job_loading: JobLoading::Loaded,
            },
            queue_status: QueueStatus::Configured {
                db_path: PathBuf::from("/tmp/daily/queue.db"),
                queue_state: QueueState::Summarized(QueueSummary {
                    record_counts: RecordCounts {
                        total: 3,
                        enabled: 3,
                        disabled: 0,
                        posted: 1,
                        never_posted: 2,
                        failed: 1,
                    },
                    next_record: Some(NextRecord {
                        id: 2,
                        queue_order: 2,
                        heading: Some("2.".to_string()),
                    }),
                    most_recent_posted_at: Some("2026-11-15T09:00:04Z".to_string()),
                    stuck_records: vec![StuckRecord {
                        id: 1,
                        heading: None,
                        posting_started_at: "2026-11-15T09:00:00Z".to_string(),
                    }],
                    failed_records: vec![FailedRecord {
                        id: 3,
                        heading: Some("3.".to_string()),
                        last_error: Some("editor lost text".to_string()),
                    }],
                }),
            },
            session_cookie_status: SessionCookieStatus::ExpiresAt(
                "2027-11-12T06:13:20Z".parse::<Timestamp>().unwrap(),
            ),
            login_status: LoginStatus::LoggedIn,
        };

        let expected_block: &str = "\
daily
  Config: valid
  Schedule: 0 9 * * *
  Next run: 2026-11-16 09:00 UTC
  Plist: installed at /Library/LaunchDaemons/ro.zach.xost.daily.plist
  Job: loaded
  Queue: /tmp/daily/queue.db
  Records: 3 total, 3 enabled, 0 disabled, 1 posted, 2 never posted, 1 failed
  Next record: id 2, order 2, heading \"2.\"
  Last posted: 2026-11-15 09:00 UTC
  STUCK MID-POST: id 1, since 2026-11-15 09:00 UTC
  Confirm on X whether each stuck record posted, then clear its posting_started_at.
  Failed record: id 3, heading \"3.\", error: editor lost text
  Session cookie: expires 2027-11-12 06:13 UTC, 361 days left
  Login: logged in
";

        assert_eq!(render_account_status(&account_status, &now), expected_block);
    }
}
