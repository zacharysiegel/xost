use std::collections::BTreeMap;
use std::path::Path;

use jiff::Timestamp;
use jiff::ZonedDateTime;
use rusqlite::Connection;
use simple_alert::alert::AlertLevel;

use crate::alert;
use crate::alert::AlertDelivery;
use crate::alert_state;
use crate::alert_state::AlertRecord;
use crate::config;
use crate::config::NamedSection;
use crate::error;
use crate::error::AppError;
use crate::queue;
use crate::schedule;
use crate::schedule::JobKind;
use crate::schedule::JobLoading;
use crate::status;
use crate::status::AccountSelection;
use crate::status::AccountSelectionError;
use crate::status::AccountStatus;
use crate::status::ConfigValidity;
use crate::status::LoginCheckRequest;
use crate::status::LoginStatus;
use crate::status::PlistInstallation;
use crate::status::ScheduleExpression;
use crate::status::ScheduleStatus;
use crate::status::SessionCookieStatus;

const SESSION_COOKIE_EXPIRY_THRESHOLD_DAYS: [i64; 4] = [14, 7, 3, 1];
const SESSION_COOKIE_EXPIRY_ERROR_THRESHOLD_DAYS: i64 = 3;
const NONE_LISTED: &str = "none";

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum ConditionKind {
    ConfigInvalid,
    SessionCookieMissing,
    SessionCookieExpiring,
    LoggedOutEarly,
    PostingJobInactive,
}

impl ConditionKind {
    pub fn key(self) -> &'static str {
        match self {
            ConditionKind::ConfigInvalid => "config_invalid",
            ConditionKind::SessionCookieMissing => "session_cookie_missing",
            ConditionKind::SessionCookieExpiring => "session_cookie_expiring",
            ConditionKind::LoggedOutEarly => "logged_out_early",
            ConditionKind::PostingJobInactive => "posting_job_inactive",
        }
    }

    fn deduplication(self) -> Deduplication {
        match self {
            ConditionKind::SessionCookieExpiring => Deduplication::OncePerValue,
            ConditionKind::ConfigInvalid
            | ConditionKind::SessionCookieMissing
            | ConditionKind::LoggedOutEarly
            | ConditionKind::PostingJobInactive => Deduplication::OncePerAppearance,
        }
    }

    fn description(self) -> &'static str {
        match self {
            ConditionKind::ConfigInvalid => "config invalid",
            ConditionKind::SessionCookieMissing => "no session cookie",
            ConditionKind::SessionCookieExpiring => "session cookie expiring",
            ConditionKind::LoggedOutEarly => "logged out early",
            ConditionKind::PostingJobInactive => "posting job inactive",
        }
    }
}

enum Deduplication {
    OncePerAppearance,
    OncePerValue,
}

#[derive(Debug, PartialEq)]
pub struct ActiveCondition {
    pub level: AlertLevel,
    pub alerted_value: String,
    pub message: String,
}

#[derive(Debug, PartialEq)]
pub enum ConditionObservation {
    Active(ActiveCondition),
    Cleared,
    Unobserved,
}

pub struct EvaluatedCondition {
    pub condition_kind: ConditionKind,
    pub condition_observation: ConditionObservation,
}

#[derive(Debug, PartialEq)]
pub enum AlertDecision {
    Send,
    AlreadySent,
    ClearRecord,
    LeaveUnchanged,
}

enum AlertStateStore {
    Open(Connection),
    Unavailable(String),
}

struct CheckOutcome {
    sent_condition_kinds: Vec<ConditionKind>,
    undelivered_condition_kinds: Vec<ConditionKind>,
    already_alerted_condition_kinds: Vec<ConditionKind>,
    alert_state_problems: Vec<String>,
}

impl CheckOutcome {
    fn empty() -> CheckOutcome {
        CheckOutcome {
            sent_condition_kinds: Vec::new(),
            undelivered_condition_kinds: Vec::new(),
            already_alerted_condition_kinds: Vec::new(),
            alert_state_problems: Vec::new(),
        }
    }
}

pub async fn run(config_path: &Path, requested_account: Option<&str>) -> Result<(), AppError> {
    let account_selection_result: Result<AccountSelection, AccountSelectionError> =
        status::select_accounts(config_path, requested_account);

    let account_selection: AccountSelection = match account_selection_result {
        Ok(account_selection) => account_selection,
        Err(AccountSelectionError::ConfigUnusable(error)) => {
            alert::send_unusable_config_alert(requested_account, &error);

            return Err(error);
        }
        Err(AccountSelectionError::AccountAbsent(error)) => return Err(error),
    };

    let AccountSelection::Selected(selected_accounts) = account_selection
    else {
        return Ok(());
    };

    let now: &ZonedDateTime = &selected_accounts.now;

    for account_section in &selected_accounts.account_sections() {
        let account_status: AccountStatus =
            status::observe_account(config_path, account_section, now, &LoginCheckRequest::Perform).await;

        let summary: String = check_account(account_section, &account_status, now);

        println!("{}", summary);
    }

    Ok(())
}

fn check_account(account_section: &NamedSection<'_>, account_status: &AccountStatus, now: &ZonedDateTime) -> String {
    let evaluated_conditions: Vec<EvaluatedCondition> = evaluate_conditions(account_status, now);
    let alert_state_store: AlertStateStore = open_alert_state_store(account_section.values);
    let mut check_outcome: CheckOutcome = CheckOutcome::empty();

    let alert_records: Vec<AlertRecord> = read_stored_alert_records(&alert_state_store, &mut check_outcome);

    for evaluated_condition in &evaluated_conditions {
        let alert_record: Option<&AlertRecord> = find_alert_record(&alert_records, evaluated_condition.condition_kind);
        let alert_decision: AlertDecision = decide_alert(
            evaluated_condition.condition_kind,
            &evaluated_condition.condition_observation,
            alert_record,
        );

        apply_alert_decision(&account_status.account, evaluated_condition, &alert_decision, &alert_state_store, &mut check_outcome);
    }

    render_check_summary(&account_status.account, &check_outcome, &account_status.login_status)
}

pub fn evaluate_conditions(account_status: &AccountStatus, now: &ZonedDateTime) -> Vec<EvaluatedCondition> {
    let account: &str = &account_status.account;
    let condition_observations: [(ConditionKind, ConditionObservation); 5] = [
        (ConditionKind::ConfigInvalid, observe_config_invalid(&account_status.config_validity)),
        (
            ConditionKind::SessionCookieMissing,
            observe_session_cookie_missing(account, &account_status.session_cookie_status),
        ),
        (
            ConditionKind::SessionCookieExpiring,
            observe_session_cookie_expiring(account, &account_status.session_cookie_status, now),
        ),
        (
            ConditionKind::LoggedOutEarly,
            observe_logged_out_early(account, &account_status.login_status, &account_status.session_cookie_status, now),
        ),
        (
            ConditionKind::PostingJobInactive,
            observe_posting_job_inactive(account, &account_status.schedule_status),
        ),
    ];

    condition_observations
        .into_iter()
        .map(|(condition_kind, condition_observation)| EvaluatedCondition {
            condition_kind,
            condition_observation,
        })
        .collect()
}

fn observe_config_invalid(config_validity: &ConfigValidity) -> ConditionObservation {
    match config_validity {
        ConfigValidity::Valid => ConditionObservation::Cleared,
        ConfigValidity::Invalid(error) => ConditionObservation::Active(ActiveCondition {
            level: AlertLevel::Error,
            alerted_value: error.clone(),
            message: format!("the config section is invalid: {}", error),
        }),
    }
}

fn observe_session_cookie_missing(account: &str, session_cookie_status: &SessionCookieStatus) -> ConditionObservation {
    match session_cookie_status {
        SessionCookieStatus::Absent | SessionCookieStatus::CookiesFileMissing => {
            ConditionObservation::Active(ActiveCondition {
                level: AlertLevel::Error,
                alerted_value: "missing".to_string(),
                message: format!("no session cookie; run `xost --account {} login` to sign in", account),
            })
        }
        SessionCookieStatus::EndsWithBrowserSession | SessionCookieStatus::ExpiresAt(_) => ConditionObservation::Cleared,
        SessionCookieStatus::ProfileNotConfigured | SessionCookieStatus::Unreadable(_) => ConditionObservation::Unobserved,
    }
}

pub fn observe_session_cookie_expiring(
    account: &str,
    session_cookie_status: &SessionCookieStatus,
    now: &ZonedDateTime,
) -> ConditionObservation {
    let SessionCookieStatus::ExpiresAt(expiry) = session_cookie_status else {
        return ConditionObservation::Unobserved;
    };

    let seconds_until_expiry: i64 = status::get_seconds_until_expiry(*expiry, now);
    let crossed_threshold_days: Option<i64> = get_crossed_expiry_threshold_days(seconds_until_expiry);

    let Some(threshold_days) = crossed_threshold_days else {
        return ConditionObservation::Cleared;
    };

    ConditionObservation::Active(ActiveCondition {
        level: get_expiry_alert_level(threshold_days),
        alerted_value: format!("{} within {} days", expiry, threshold_days),
        message: render_expiry_message(account, *expiry, now),
    })
}

pub fn get_crossed_expiry_threshold_days(seconds_until_expiry: i64) -> Option<i64> {
    SESSION_COOKIE_EXPIRY_THRESHOLD_DAYS
        .into_iter()
        .filter(|threshold_days| seconds_until_expiry <= threshold_days * status::SECONDS_PER_DAY)
        .min()
}

pub fn get_expiry_alert_level(threshold_days: i64) -> AlertLevel {
    match threshold_days <= SESSION_COOKIE_EXPIRY_ERROR_THRESHOLD_DAYS {
        true => AlertLevel::Error,
        false => AlertLevel::Warning,
    }
}

pub fn render_expiry_message(account: &str, expiry: Timestamp, now: &ZonedDateTime) -> String {
    format!(
        "the X session cookie {}; run `xost --account {} login` to renew it",
        status::render_cookie_expiry(expiry, now),
        account,
    )
}

pub fn observe_logged_out_early(
    account: &str,
    login_status: &LoginStatus,
    session_cookie_status: &SessionCookieStatus,
    now: &ZonedDateTime,
) -> ConditionObservation {
    match login_status {
        LoginStatus::LoggedIn => ConditionObservation::Cleared,
        LoginStatus::LoggedOut => observe_logout_against_cookie(account, session_cookie_status, now),
        LoginStatus::NotChecked | LoginStatus::NotCheckable(_) | LoginStatus::CheckFailed(_) => ConditionObservation::Unobserved,
    }
}

fn observe_logout_against_cookie(
    account: &str,
    session_cookie_status: &SessionCookieStatus,
    now: &ZonedDateTime,
) -> ConditionObservation {
    let cookie_unexpired: bool = match session_cookie_status {
        SessionCookieStatus::EndsWithBrowserSession => true,
        SessionCookieStatus::ExpiresAt(expiry) => status::get_seconds_until_expiry(*expiry, now) > 0,
        SessionCookieStatus::ProfileNotConfigured
        | SessionCookieStatus::CookiesFileMissing
        | SessionCookieStatus::Absent
        | SessionCookieStatus::Unreadable(_) => false,
    };

    if !cookie_unexpired {
        return ConditionObservation::Cleared;
    }

    ConditionObservation::Active(ActiveCondition {
        level: AlertLevel::Error,
        alerted_value: "logged out".to_string(),
        message: format!(
            "X shows the account logged out although its session cookie has not expired; run `xost --account {} login` to sign in again",
            account,
        ),
    })
}

pub fn observe_posting_job_inactive(account: &str, schedule_status: &ScheduleStatus) -> ConditionObservation {
    if let ScheduleExpression::Absent = schedule_status.schedule_expression {
        return ConditionObservation::Cleared;
    }

    let inactive_state: &str = match (&schedule_status.plist_installation, &schedule_status.job_loading) {
        (PlistInstallation::NotInstalled, _) => "not installed",
        (PlistInstallation::Installed(_), JobLoading::NotLoaded) => "installed but not loaded",
        (PlistInstallation::Installed(_), JobLoading::Loaded) => return ConditionObservation::Cleared,
        (PlistInstallation::Installed(_), JobLoading::Unknown) => return ConditionObservation::Unobserved,
    };

    ConditionObservation::Active(ActiveCondition {
        level: AlertLevel::Error,
        alerted_value: inactive_state.to_string(),
        message: format!(
            "the posting job {} is {}; run `xost --account {} schedule`",
            schedule::job_label(account, JobKind::Post),
            inactive_state,
            account,
        ),
    })
}

pub fn decide_alert(
    condition_kind: ConditionKind,
    condition_observation: &ConditionObservation,
    alert_record: Option<&AlertRecord>,
) -> AlertDecision {
    match (condition_observation, alert_record) {
        (ConditionObservation::Unobserved, _) => AlertDecision::LeaveUnchanged,
        (ConditionObservation::Cleared, Some(_)) => AlertDecision::ClearRecord,
        (ConditionObservation::Cleared, None) => AlertDecision::LeaveUnchanged,
        (ConditionObservation::Active(_), None) => AlertDecision::Send,
        (ConditionObservation::Active(active_condition), Some(alert_record)) => {
            decide_for_recorded_condition(condition_kind, active_condition, alert_record)
        }
    }
}

fn decide_for_recorded_condition(
    condition_kind: ConditionKind,
    active_condition: &ActiveCondition,
    alert_record: &AlertRecord,
) -> AlertDecision {
    match condition_kind.deduplication() {
        Deduplication::OncePerAppearance => AlertDecision::AlreadySent,
        Deduplication::OncePerValue if alert_record.alerted_value == active_condition.alerted_value => AlertDecision::AlreadySent,
        Deduplication::OncePerValue => AlertDecision::Send,
    }
}

fn find_alert_record(alert_records: &[AlertRecord], condition_kind: ConditionKind) -> Option<&AlertRecord> {
    alert_records
        .iter()
        .find(|alert_record| alert_record.condition_key == condition_kind.key())
}

fn open_alert_state_store(account_values: &BTreeMap<String, String>) -> AlertStateStore {
    let Some(db_path) = config::get_configured_path(account_values, config::DB_KEY) else {
        return AlertStateStore::Unavailable("no db path configured".to_string());
    };

    let open_result: Result<Connection, AppError> = open_queue_with_schema(&db_path);

    match open_result {
        Ok(connection) => AlertStateStore::Open(connection),
        Err(error) => AlertStateStore::Unavailable(format!("{}: {}", db_path.display(), error::get_error_message(&error))),
    }
}

fn open_queue_with_schema(db_path: &Path) -> Result<Connection, AppError> {
    let connection: Connection = queue::open(db_path)?;

    queue::init_schema(&connection)?;

    Ok(connection)
}

fn read_stored_alert_records(alert_state_store: &AlertStateStore, check_outcome: &mut CheckOutcome) -> Vec<AlertRecord> {
    let connection: &Connection = match alert_state_store {
        AlertStateStore::Open(connection) => connection,
        AlertStateStore::Unavailable(reason) => {
            check_outcome.alert_state_problems.push(reason.clone());

            return Vec::new();
        }
    };

    let read_result: Result<Vec<AlertRecord>, AppError> = alert_state::read_alert_records(connection);

    match read_result {
        Ok(alert_records) => alert_records,
        Err(error) => {
            check_outcome.alert_state_problems.push(error::get_error_message(&error));

            Vec::new()
        }
    }
}

fn apply_alert_decision(
    account: &str,
    evaluated_condition: &EvaluatedCondition,
    alert_decision: &AlertDecision,
    alert_state_store: &AlertStateStore,
    check_outcome: &mut CheckOutcome,
) {
    let condition_kind: ConditionKind = evaluated_condition.condition_kind;

    match (alert_decision, &evaluated_condition.condition_observation) {
        (AlertDecision::Send, ConditionObservation::Active(active_condition)) => {
            send_and_record(account, condition_kind, active_condition, alert_state_store, check_outcome);
        }
        (AlertDecision::AlreadySent, _) => check_outcome.already_alerted_condition_kinds.push(condition_kind),
        (AlertDecision::ClearRecord, _) => {
            let clear_result: Result<(), AppError> =
                update_alert_state(alert_state_store, |connection| alert_state::clear_alert(connection, condition_kind.key()));

            record_alert_state_problem(clear_result, check_outcome);
        }
        (AlertDecision::Send, _) | (AlertDecision::LeaveUnchanged, _) => {}
    }
}

fn send_and_record(
    account: &str,
    condition_kind: ConditionKind,
    active_condition: &ActiveCondition,
    alert_state_store: &AlertStateStore,
    check_outcome: &mut CheckOutcome,
) {
    let alert_delivery: AlertDelivery = alert::send(account, active_condition.level, &active_condition.message);

    if alert_delivery == AlertDelivery::Failed {
        check_outcome.undelivered_condition_kinds.push(condition_kind);

        return;
    }

    check_outcome.sent_condition_kinds.push(condition_kind);

    let record_result: Result<(), AppError> = update_alert_state(alert_state_store, |connection| {
        alert_state::record_alert(connection, condition_kind.key(), &active_condition.alerted_value)
    });

    record_alert_state_problem(record_result, check_outcome);
}

fn update_alert_state(
    alert_state_store: &AlertStateStore,
    update: impl FnOnce(&Connection) -> Result<(), AppError>,
) -> Result<(), AppError> {
    match alert_state_store {
        AlertStateStore::Open(connection) => update(connection),
        AlertStateStore::Unavailable(_) => Ok(()),
    }
}

fn record_alert_state_problem(update_result: Result<(), AppError>, check_outcome: &mut CheckOutcome) {
    if let Err(error) = update_result {
        check_outcome.alert_state_problems.push(error::get_error_message(&error));
    }
}

fn render_check_summary(account: &str, check_outcome: &CheckOutcome, login_status: &LoginStatus) -> String {
    let mut summary_parts: Vec<String> = vec![
        format!("alerts sent: {}", render_condition_list(&check_outcome.sent_condition_kinds)),
        format!("already alerted: {}", render_condition_list(&check_outcome.already_alerted_condition_kinds)),
    ];

    if !check_outcome.undelivered_condition_kinds.is_empty() {
        summary_parts.push(format!(
            "alerts not delivered: {}",
            render_condition_list(&check_outcome.undelivered_condition_kinds),
        ));
    }

    summary_parts.push(render_login_summary(login_status));

    if !check_outcome.alert_state_problems.is_empty() {
        summary_parts.push(format!(
            "alert state not recorded: {}",
            status::get_single_line_preview(&check_outcome.alert_state_problems.join("; ")),
        ));
    }

    format!("{}: {}", account, summary_parts.join("; "))
}

fn render_condition_list(condition_kinds: &[ConditionKind]) -> String {
    if condition_kinds.is_empty() {
        return NONE_LISTED.to_string();
    }

    let descriptions: Vec<&str> = condition_kinds.iter().map(|condition_kind| condition_kind.description()).collect();

    descriptions.join(", ")
}

fn render_login_summary(login_status: &LoginStatus) -> String {
    match login_status {
        LoginStatus::LoggedIn => "login: logged in".to_string(),
        LoginStatus::LoggedOut => "login: logged out".to_string(),
        LoginStatus::NotChecked => "login check skipped".to_string(),
        LoginStatus::NotCheckable(reason) => format!("login check skipped: {}", reason),
        LoginStatus::CheckFailed(error) => format!("login check skipped: {}", status::get_single_line_preview(error)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use jiff::tz::TimeZone;

    use crate::clock;

    const SAMPLE_EXPIRY: &str = "2026-11-20T09:00:00Z";

    fn get_expiry() -> Timestamp {
        clock::parse_rfc3339(SAMPLE_EXPIRY).unwrap()
    }

    fn get_days_before(expiry: Timestamp, days: i64) -> ZonedDateTime {
        Timestamp::from_second(expiry.as_second() - days * status::SECONDS_PER_DAY)
            .unwrap()
            .to_zoned_datetime(TimeZone::UTC)
    }

    fn get_alert_record(condition_kind: ConditionKind, alerted_value: &str) -> AlertRecord {
        AlertRecord {
            condition_key: condition_kind.key().to_string(),
            alerted_value: alerted_value.to_string(),
        }
    }

    fn get_active_config_invalid(error: &str) -> ConditionObservation {
        observe_config_invalid(&ConfigValidity::Invalid(error.to_string()))
    }

    fn get_alerted_value(condition_observation: &ConditionObservation) -> String {
        let ConditionObservation::Active(active_condition) = condition_observation else {
            panic!("the condition is not active");
        };

        active_condition.alerted_value.clone()
    }

    fn create_alert_state_connection() -> Connection {
        let connection: Connection = Connection::open_in_memory().unwrap();

        alert_state::create_table_if_missing(&connection).unwrap();

        connection
    }

    #[test]
    fn get_crossed_expiry_threshold_days_is_none_beyond_fourteen_days() {
        assert_eq!(get_crossed_expiry_threshold_days(15 * status::SECONDS_PER_DAY), None);
    }

    #[test]
    fn get_crossed_expiry_threshold_days_crosses_fourteen_days_within_fourteen_days() {
        assert_eq!(get_crossed_expiry_threshold_days(13 * status::SECONDS_PER_DAY), Some(14));
    }

    #[test]
    fn get_crossed_expiry_threshold_days_picks_the_nearest_of_several_crossed_thresholds() {
        assert_eq!(get_crossed_expiry_threshold_days(2 * status::SECONDS_PER_DAY), Some(3));
    }

    #[test]
    fn decide_alert_does_not_repeat_an_already_alerted_expiry_threshold() {
        let status_at_alert: SessionCookieStatus = SessionCookieStatus::ExpiresAt(get_expiry());
        let alerted_observation: ConditionObservation =
            observe_session_cookie_expiring("daily", &status_at_alert, &get_days_before(get_expiry(), 3));
        let alert_record: AlertRecord =
            get_alert_record(ConditionKind::SessionCookieExpiring, &get_alerted_value(&alerted_observation));

        let later_observation: ConditionObservation =
            observe_session_cookie_expiring("daily", &status_at_alert, &get_days_before(get_expiry(), 2));
        let alert_decision: AlertDecision =
            decide_alert(ConditionKind::SessionCookieExpiring, &later_observation, Some(&alert_record));

        assert_eq!(alert_decision, AlertDecision::AlreadySent);
    }

    #[test]
    fn decide_alert_alerts_again_for_a_later_cookie_expiry() {
        let earlier_expiry_observation: ConditionObservation = observe_session_cookie_expiring(
            "daily",
            &SessionCookieStatus::ExpiresAt(get_expiry()),
            &get_days_before(get_expiry(), 1),
        );
        let alert_record: AlertRecord =
            get_alert_record(ConditionKind::SessionCookieExpiring, &get_alerted_value(&earlier_expiry_observation));

        let later_expiry: Timestamp = Timestamp::from_second(get_expiry().as_second() + 5 * status::SECONDS_PER_DAY).unwrap();
        let later_expiry_observation: ConditionObservation = observe_session_cookie_expiring(
            "daily",
            &SessionCookieStatus::ExpiresAt(later_expiry),
            &get_days_before(get_expiry(), 1),
        );
        let alert_decision: AlertDecision =
            decide_alert(ConditionKind::SessionCookieExpiring, &later_expiry_observation, Some(&alert_record));

        assert_eq!(alert_decision, AlertDecision::Send);
    }

    #[test]
    fn decide_alert_does_not_repeat_a_persisting_condition_whose_detail_changed() {
        let alert_record: AlertRecord = get_alert_record(ConditionKind::ConfigInvalid, "first error");

        let alert_decision: AlertDecision = decide_alert(
            ConditionKind::ConfigInvalid,
            &get_active_config_invalid("second error"),
            Some(&alert_record),
        );

        assert_eq!(alert_decision, AlertDecision::AlreadySent);
    }

    #[test]
    fn decide_alert_alerts_again_after_the_condition_cleared_and_reappeared() {
        let connection: Connection = create_alert_state_connection();
        let condition_key: &str = ConditionKind::ConfigInvalid.key();

        alert_state::record_alert(&connection, condition_key, "boom").unwrap();

        let records_while_alerted: Vec<AlertRecord> = alert_state::read_alert_records(&connection).unwrap();
        let cleared_decision: AlertDecision = decide_alert(
            ConditionKind::ConfigInvalid,
            &ConditionObservation::Cleared,
            find_alert_record(&records_while_alerted, ConditionKind::ConfigInvalid),
        );

        alert_state::clear_alert(&connection, condition_key).unwrap();

        let records_after_clearing: Vec<AlertRecord> = alert_state::read_alert_records(&connection).unwrap();
        let reappeared_decision: AlertDecision = decide_alert(
            ConditionKind::ConfigInvalid,
            &get_active_config_invalid("boom"),
            find_alert_record(&records_after_clearing, ConditionKind::ConfigInvalid),
        );

        assert_eq!(vec![cleared_decision, reappeared_decision], vec![AlertDecision::ClearRecord, AlertDecision::Send]);
    }

    #[test]
    fn get_expiry_alert_level_warns_at_fourteen_and_seven_days_and_errors_at_three_and_one() {
        let alert_levels: Vec<AlertLevel> =
            SESSION_COOKIE_EXPIRY_THRESHOLD_DAYS.into_iter().map(get_expiry_alert_level).collect();

        assert_eq!(
            alert_levels,
            vec![AlertLevel::Warning, AlertLevel::Warning, AlertLevel::Error, AlertLevel::Error],
        );
    }

    #[test]
    fn render_expiry_message_names_the_expiry_and_the_days_left() {
        let message: String = render_expiry_message("daily", get_expiry(), &get_days_before(get_expiry(), 3));

        assert_eq!(
            message,
            "the X session cookie expires 2026-11-20 09:00 UTC, 3 days left; run `xost --account daily login` to renew it",
        );
    }

    #[test]
    fn observe_logged_out_early_ignores_a_logout_after_the_cookie_expired() {
        let one_day_after_expiry: ZonedDateTime = get_days_before(get_expiry(), -1);

        let condition_observation: ConditionObservation = observe_logged_out_early(
            "daily",
            &LoginStatus::LoggedOut,
            &SessionCookieStatus::ExpiresAt(get_expiry()),
            &one_day_after_expiry,
        );

        assert_eq!(condition_observation, ConditionObservation::Cleared);
    }

    #[test]
    fn observe_posting_job_inactive_reports_an_installed_job_that_is_not_loaded() {
        let schedule_status: ScheduleStatus = ScheduleStatus {
            job_kind: JobKind::Post,
            schedule_expression: ScheduleExpression::Invalid {
                expression: "0 9 * * MON".to_string(),
            },
            plist_installation: PlistInstallation::Installed("/Library/LaunchDaemons/ro.zach.xost.daily.plist".to_string()),
            job_loading: JobLoading::NotLoaded,
        };

        let condition_observation: ConditionObservation = observe_posting_job_inactive("daily", &schedule_status);

        assert_eq!(get_alerted_value(&condition_observation), "installed but not loaded");
    }

    #[test]
    fn render_check_summary_lists_sent_and_already_alerted_conditions_and_the_login_result() {
        let check_outcome: CheckOutcome = CheckOutcome {
            sent_condition_kinds: vec![ConditionKind::SessionCookieExpiring],
            undelivered_condition_kinds: Vec::new(),
            already_alerted_condition_kinds: vec![ConditionKind::PostingJobInactive, ConditionKind::ConfigInvalid],
            alert_state_problems: Vec::new(),
        };

        let summary: String = render_check_summary(
            "daily",
            &check_outcome,
            &LoginStatus::NotCheckable("the Chrome profile is in use by another Chrome process".to_string()),
        );

        assert_eq!(
            summary,
            "daily: alerts sent: session cookie expiring; already alerted: posting job inactive, config invalid; \
             login check skipped: the Chrome profile is in use by another Chrome process",
        );
    }
}
