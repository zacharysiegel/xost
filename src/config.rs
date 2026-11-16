use std::collections::BTreeMap;
use std::path::PathBuf;

use crate::cron::CronSchedule;
use crate::error;
use crate::error::AppError;
use crate::private_file;
use crate::secrets;
use crate::thread;
use crate::thread::ThreadNumbering;
use crate::thread::ThreadSettings;

const DEFAULT_HEADING_SEPARATOR: &str = " ";
const DEFAULT_THREAD_POST_LIMIT: usize = 280;
const DEFAULT_THREAD_MAX_POSTS: usize = 9;

pub const SECRETS_SECTION: &str = "secrets";
pub const ACCOUNT_SECTION: &str = "account";
pub const SECRETS_STORE_KEY: &str = "store";
pub const SECRETS_KEY_KEY: &str = "key";
pub const HANDLE_KEY: &str = "handle";
pub const USERNAME_KEY: &str = "username";
pub const DB_KEY: &str = "db";
pub const PROFILE_KEY: &str = "profile";
pub const CHROME_KEY: &str = "chrome";
pub const SCHEDULE_KEY: &str = "schedule";
pub const CHECK_SCHEDULE_KEY: &str = "check_schedule";
pub const HEADING_SEPARATOR_KEY: &str = "heading_separator";
pub const THREAD_POST_LIMIT_KEY: &str = "thread_post_limit";
pub const THREAD_MAX_POSTS_KEY: &str = "thread_max_posts";
pub const THREAD_NUMBERING_KEY: &str = "thread_numbering";
pub const THREAD_OVERFLOW_LABEL_KEY: &str = "thread_overflow_label";
pub const THREAD_SOURCE_LABEL_KEY: &str = "thread_source_label";

pub struct Config {
    pub account: String,
    pub db_path: PathBuf,
    pub profile_dir: PathBuf,
    pub chrome_path: Option<PathBuf>,
    pub username: String,
    pub handle: String,
    pub secr_store_path: PathBuf,
    pub secr_key_base64: String,
    pub password_secret: String,
    pub totp_seed_secret: String,
    pub heading_separator: String,
    pub thread_settings: ThreadSettings,
    pub schedule: Option<CronSchedule>,
    pub check_schedule: Option<CronSchedule>,
}

impl Config {
    pub fn load(config_path: &std::path::Path, account: &str) -> Result<Self, AppError> {
        let sections: Vec<Section> = read_sections(config_path)?;

        Config::from_sections(&sections, account)
    }

    pub fn from_sections(sections: &[Section], account: &str) -> Result<Self, AppError> {
        validate_account_name(account)?;

        let account_values: &BTreeMap<String, String> = find_section(sections, ACCOUNT_SECTION, Some(account))
            .ok_or_else(|| get_absent_account_error(account, sections))?;

        let secrets_values: &BTreeMap<String, String> = find_section(sections, SECRETS_SECTION, None).ok_or_else(|| {
            AppError::new(&format!(
                "config has no {} section (expected keys: {}, {})",
                secrets_section_header(),
                SECRETS_STORE_KEY,
                SECRETS_KEY_KEY,
            ))
        })?;

        let handle: String = require(account_values, HANDLE_KEY, account)?;
        let username: String = account_values.get(USERNAME_KEY).cloned().unwrap_or_else(|| handle.clone());
        let db_path: PathBuf = require_path(account_values, DB_KEY, account)?;
        let profile_dir: PathBuf = require_path(account_values, PROFILE_KEY, account)?;
        let chrome_path: Option<PathBuf> = get_configured_path(account_values, CHROME_KEY);
        let secr_store_path: PathBuf = require_path(secrets_values, SECRETS_STORE_KEY, SECRETS_SECTION)?;
        let secr_key_base64: String = require(secrets_values, SECRETS_KEY_KEY, SECRETS_SECTION)?;
        let heading_separator: String = match account_values.get(HEADING_SEPARATOR_KEY) {
            Some(raw) => parse_string_value(raw)?,
            None => DEFAULT_HEADING_SEPARATOR.to_string(),
        };
        let thread_settings: ThreadSettings = parse_thread_settings(account_values)?;
        let schedule: Option<CronSchedule> = parse_schedule(account_values, SCHEDULE_KEY)?;
        let check_schedule: Option<CronSchedule> = parse_schedule(account_values, CHECK_SCHEDULE_KEY)?;

        Ok(Config {
            account: account.to_string(),
            db_path,
            profile_dir,
            chrome_path,
            username,
            handle,
            secr_store_path,
            secr_key_base64,
            password_secret: secrets::password_secret_name(account),
            totp_seed_secret: secrets::totp_seed_secret_name(account),
            heading_separator,
            thread_settings,
            schedule,
            check_schedule,
        })
    }
}

fn parse_schedule(account_values: &BTreeMap<String, String>, key: &str) -> Result<Option<CronSchedule>, AppError> {
    let Some(raw) = account_values.get(key) else {
        return Ok(None);
    };

    let schedule: CronSchedule =
        CronSchedule::parse(raw).map_err(|error| AppError::new(&format!("{}: {}", key, error::get_error_message(&error))))?;

    Ok(Some(schedule))
}

fn parse_thread_settings(account_values: &BTreeMap<String, String>) -> Result<ThreadSettings, AppError> {
    let post_limit: usize = parse_count_value(account_values, THREAD_POST_LIMIT_KEY, DEFAULT_THREAD_POST_LIMIT)?;
    let max_posts: usize = parse_count_value(account_values, THREAD_MAX_POSTS_KEY, DEFAULT_THREAD_MAX_POSTS)?;

    let numbering: ThreadNumbering = match account_values.get(THREAD_NUMBERING_KEY).map(String::as_str) {
        None | Some("all_posts") => ThreadNumbering::AllPosts,
        Some("first_post") => ThreadNumbering::FirstPost,
        Some(other) => {
            return Err(AppError::new(&format!(
                "{} must be all_posts or first_post, not '{}'",
                THREAD_NUMBERING_KEY, other,
            )));
        }
    };

    let overflow_label: Option<String> = parse_optional_string_value(account_values, THREAD_OVERFLOW_LABEL_KEY)?;
    let source_label: Option<String> = parse_optional_string_value(account_values, THREAD_SOURCE_LABEL_KEY)?;

    let thread_settings: ThreadSettings = ThreadSettings {
        post_limit,
        max_posts,
        numbering,
        overflow_label,
        source_label,
    };

    validate_thread_settings(&thread_settings)?;

    Ok(thread_settings)
}

fn validate_thread_settings(thread_settings: &ThreadSettings) -> Result<(), AppError> {
    if thread_settings.max_posts == 0 {
        return Err(AppError::new(&format!("{} must be at least 1", THREAD_MAX_POSTS_KEY)));
    }

    validate_link_label(THREAD_OVERFLOW_LABEL_KEY, thread_settings.overflow_label.as_deref(), thread_settings.post_limit)?;
    validate_link_label(THREAD_SOURCE_LABEL_KEY, thread_settings.source_label.as_deref(), thread_settings.post_limit)?;

    Ok(())
}

fn validate_link_label(key: &str, label: Option<&str>, post_limit: usize) -> Result<(), AppError> {
    let Some(label) = label else {
        return Ok(());
    };

    let link_post_weight: usize = thread::link_post_weight(label);

    if link_post_weight > post_limit {
        return Err(AppError::new(&format!(
            "{} makes its link post weigh {}, more than {} {}",
            key, link_post_weight, THREAD_POST_LIMIT_KEY, post_limit,
        )));
    }

    Ok(())
}

fn parse_count_value(values: &BTreeMap<String, String>, key: &str, default: usize) -> Result<usize, AppError> {
    let Some(raw) = values.get(key) else {
        return Ok(default);
    };

    raw.parse::<usize>()
        .map_err(|_| AppError::new(&format!("config key '{}' must be a non-negative integer, not '{}'", key, raw)))
}

fn parse_optional_string_value(values: &BTreeMap<String, String>, key: &str) -> Result<Option<String>, AppError> {
    let Some(raw) = values.get(key) else {
        return Ok(None);
    };

    let parsed: String = parse_string_value(raw)?;

    Ok(Some(parsed))
}

/// A value wrapped in double quotes keeps its whitespace and supports the escapes `\n`, `\t`, `\"`, and `\\`.
fn parse_string_value(raw: &str) -> Result<String, AppError> {
    let Some(inner) = raw.strip_prefix('"').and_then(|rest| rest.strip_suffix('"')) else {
        return Ok(raw.to_string());
    };

    let mut parsed: String = String::new();
    let mut characters: std::str::Chars<'_> = inner.chars();

    while let Some(character) = characters.next() {
        if character != '\\' {
            parsed.push(character);
            continue;
        }

        let escaped: char = match characters.next() {
            Some('n') => '\n',
            Some('t') => '\t',
            Some('"') => '"',
            Some('\\') => '\\',
            Some(other) => return Err(AppError::new(&format!("unsupported escape '\\{}' in config value {}", other, raw))),
            None => return Err(AppError::new(&format!("config value ends with a lone backslash: {}", raw))),
        };
        parsed.push(escaped);
    }

    Ok(parsed)
}

pub fn default_config_path() -> Result<PathBuf, AppError> {
    let home: String = std::env::var("HOME").map_err(|_| AppError::new("HOME is not set; pass --config explicitly"))?;

    Ok(PathBuf::from(home).join(".config").join("xost").join("config"))
}

pub fn read_sections(config_path: &std::path::Path) -> Result<Vec<Section>, AppError> {
    let contents: String = read_required_config(config_path)?;

    parse(&contents)
}

fn read_required_config(config_path: &std::path::Path) -> Result<String, AppError> {
    std::fs::read_to_string(config_path).map_err(|error| get_config_read_error(config_path, &error))
}

pub fn read_existing_config(config_path: &std::path::Path) -> Result<String, AppError> {
    private_file::read_existing_text(config_path).map_err(|error| get_config_read_error(config_path, &error))
}

fn get_config_read_error(config_path: &std::path::Path, error: &impl std::fmt::Display) -> AppError {
    AppError::new(&format!("could not read config at {}: {}", config_path.display(), error))
}

pub fn secrets_section_header() -> String {
    format!("[{}]", SECRETS_SECTION)
}

pub fn account_section_header(account: &str) -> String {
    format!("[{} \"{}\"]", ACCOUNT_SECTION, account)
}

pub struct Section {
    kind: String,
    name: Option<String>,
    values: BTreeMap<String, String>,
}

pub fn parse(contents: &str) -> Result<Vec<Section>, AppError> {
    let mut sections: Vec<Section> = Vec::new();

    for raw_line in contents.lines() {
        let line: &str = raw_line.trim();

        if line.is_empty() || line.starts_with('#') {
            continue;
        }

        if let Some(header_inner) = get_section_header_inner(line) {
            sections.push(parse_header(header_inner));

            continue;
        }

        let Some((key, value)) = line.split_once('=') else {
            return Err(AppError::new(&format!("config line is neither a section nor a key = value pair: {}", line)));
        };

        let current: &mut Section = sections
            .last_mut()
            .ok_or_else(|| AppError::new(&format!("config key appears before any section: {}", line)))?;

        current.values.insert(key.trim().to_string(), value.trim().to_string());
    }

    Ok(sections)
}

fn get_section_header_inner(trimmed_line: &str) -> Option<&str> {
    trimmed_line.strip_prefix('[')?.strip_suffix(']')
}

fn parse_header(inner: &str) -> Section {
    let inner: &str = inner.trim();

    match inner.split_once(char::is_whitespace) {
        Some((kind, rest)) => {
            let name: &str = rest.trim().trim_matches('"');

            Section {
                kind: kind.to_string(),
                name: Some(name.to_string()),
                values: BTreeMap::new(),
            }
        }
        None => Section {
            kind: inner.to_string(),
            name: None,
            values: BTreeMap::new(),
        },
    }
}

pub fn is_section_header_line(raw_line: &str) -> bool {
    get_section_header_inner(raw_line.trim()).is_some()
}

pub fn is_account_section_header_line(raw_line: &str, account: &str) -> bool {
    let Some(header_inner) = get_section_header_inner(raw_line.trim()) else {
        return false;
    };

    let header: Section = parse_header(header_inner);

    header.kind == ACCOUNT_SECTION && header.name.as_deref() == Some(account)
}

pub fn find_section<'a>(sections: &'a [Section], kind: &str, name: Option<&str>) -> Option<&'a BTreeMap<String, String>> {
    sections
        .iter()
        .find(|section| section.kind == kind && section.name.as_deref() == name)
        .map(|section| &section.values)
}

pub struct NamedSection<'a> {
    pub name: &'a str,
    pub values: &'a BTreeMap<String, String>,
}

pub fn find_named_sections<'a>(sections: &'a [Section], kind: &str) -> Vec<NamedSection<'a>> {
    sections
        .iter()
        .filter(|section| section.kind == kind)
        .filter_map(|section| {
            section.name.as_deref().map(|name| NamedSection {
                name,
                values: &section.values,
            })
        })
        .collect()
}

pub fn select_account_sections(sections: Vec<Section>, requested_account: Option<&str>) -> Result<Vec<Section>, AppError> {
    if let Some(account) = requested_account {
        let requested_section_present: bool = find_section(&sections, ACCOUNT_SECTION, Some(account)).is_some();

        if !requested_section_present {
            return Err(get_absent_account_error(account, &sections));
        }
    }

    let account_sections: Vec<Section> = sections
        .into_iter()
        .filter(|section| section.kind == ACCOUNT_SECTION && section.name.is_some())
        .filter(|section| requested_account.is_none() || section.name.as_deref() == requested_account)
        .collect();

    Ok(account_sections)
}

fn get_absent_account_error(account: &str, sections: &[Section]) -> AppError {
    AppError::new(&format!(
        "no {} section in config; defined accounts: {}",
        account_section_header(account),
        account_names(sections),
    ))
}

pub fn account_names(sections: &[Section]) -> String {
    let names: Vec<&str> = find_named_sections(sections, ACCOUNT_SECTION)
        .into_iter()
        .map(|account_section| account_section.name)
        .collect();

    if names.is_empty() {
        return "(none)".to_string();
    }

    names.join(", ")
}

pub fn require(values: &BTreeMap<String, String>, key: &str, context: &str) -> Result<String, AppError> {
    values
        .get(key)
        .filter(|value| !value.is_empty())
        .cloned()
        .ok_or_else(|| get_missing_key_error(key, context))
}

fn require_path(values: &BTreeMap<String, String>, key: &str, context: &str) -> Result<PathBuf, AppError> {
    get_configured_path(values, key).ok_or_else(|| get_missing_key_error(key, context))
}

fn get_missing_key_error(key: &str, context: &str) -> AppError {
    AppError::new(&format!("config section '{}' is missing required key '{}'", context, key))
}

pub fn get_configured_path(values: &BTreeMap<String, String>, key: &str) -> Option<PathBuf> {
    values.get(key).filter(|raw| !raw.is_empty()).map(|raw| expand_user_path(raw))
}

pub fn validate_account_name(account: &str) -> Result<(), AppError> {
    let valid: bool = !account.is_empty()
        && account.chars().all(|character| character.is_ascii_alphanumeric() || character == '-' || character == '_');

    if !valid {
        return Err(AppError::new(&format!(
            "account name '{}' must be non-empty and contain only letters, digits, '-', or '_'; \
             it is used as a file path, a secret name, and a launchd label",
            account,
        )));
    }

    Ok(())
}

pub fn expand_user_path(raw: &str) -> PathBuf {
    let Some(remainder) = raw.strip_prefix("~/") else {
        return PathBuf::from(raw);
    };

    match std::env::var("HOME") {
        Ok(home) => PathBuf::from(home).join(remainder),
        Err(_) => PathBuf::from(raw),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = "\
# comment
[secrets]
  store = /tmp/secrets.yaml
  key = AAAABBBB

[account \"gimmick1\"]
  handle = funnybot
  username = funnybot@example.com
  db = /tmp/gimmick1/queue.db
  profile = /tmp/gimmick1/profile

[account \"gimmick2\"]
  handle = otherbot
  db = /tmp/gimmick2/queue.db
  profile = /tmp/gimmick2/profile
";

    fn get_config_from(contents: &str, account: &str) -> Result<Config, AppError> {
        static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

        let unique: u64 = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let path: PathBuf = std::env::temp_dir().join(format!("xost-test-{}-{}.config", std::process::id(), unique));
        std::fs::write(&path, contents).unwrap();

        let result: Result<Config, AppError> = Config::load(&path, account);
        std::fs::remove_file(&path).ok();

        result
    }

    #[test]
    fn load_reads_the_requested_account_section() {
        let config: Config = get_config_from(SAMPLE, "gimmick1").unwrap();

        assert_eq!(config.handle, "funnybot");
        assert_eq!(config.username, "funnybot@example.com");
        assert_eq!(config.db_path, PathBuf::from("/tmp/gimmick1/queue.db"));
    }

    #[test]
    fn username_defaults_to_handle_when_absent() {
        let config: Config = get_config_from(SAMPLE, "gimmick2").unwrap();

        assert_eq!(config.username, "otherbot");
    }

    #[test]
    fn secret_names_are_derived_from_the_account() {
        let config: Config = get_config_from(SAMPLE, "gimmick1").unwrap();

        assert_eq!(config.password_secret, "gimmick1_password");
        assert_eq!(config.totp_seed_secret, "gimmick1_totp_seed");
    }

    #[test]
    fn secrets_section_is_shared_across_accounts() {
        let config: Config = get_config_from(SAMPLE, "gimmick2").unwrap();

        assert_eq!(config.secr_store_path, PathBuf::from("/tmp/secrets.yaml"));
        assert_eq!(config.secr_key_base64, "AAAABBBB");
    }

    #[test]
    fn unknown_account_is_an_error() {
        assert!(get_config_from(SAMPLE, "missing").is_err());
    }

    #[test]
    fn account_name_with_spaces_is_rejected() {
        assert!(get_config_from(SAMPLE, "Nietzsche Dawn Daily").is_err());
    }

    #[test]
    fn heading_separator_defaults_to_a_single_space() {
        let config: Config = get_config_from(SAMPLE, "gimmick1").unwrap();

        assert_eq!(config.heading_separator, " ");
    }

    #[test]
    fn load_uses_the_thread_defaults_when_absent() {
        let config: Config = get_config_from(SAMPLE, "gimmick1").unwrap();

        assert_eq!(config.thread_settings.post_limit, 280);
        assert_eq!(config.thread_settings.max_posts, 9);
        assert_eq!(config.thread_settings.numbering, ThreadNumbering::AllPosts);
        assert_eq!(config.thread_settings.overflow_label, None);
        assert_eq!(config.thread_settings.source_label, None);
    }

    #[test]
    fn load_rejects_a_thread_numbering_other_than_all_posts_or_first_post() {
        let contents: String = format!("{}  thread_numbering = every\n", SAMPLE);

        assert!(get_config_from(&contents, "gimmick2").is_err());
    }

    #[test]
    fn load_rejects_a_thread_max_posts_of_zero() {
        let contents: String = format!("{}  thread_max_posts = 0\n", SAMPLE);

        assert!(get_config_from(&contents, "gimmick2").is_err());
    }

    #[test]
    fn load_rejects_an_overflow_label_whose_post_exceeds_thread_post_limit() {
        let contents: String = format!("{}  thread_post_limit = 30\n  thread_overflow_label = Full text\n", SAMPLE);

        assert!(get_config_from(&contents, "gimmick2").is_err());
    }

    #[test]
    fn load_rejects_a_source_label_whose_post_exceeds_thread_post_limit() {
        let contents: String = format!("{}  thread_post_limit = 30\n  thread_source_label = Source\n", SAMPLE);

        assert!(get_config_from(&contents, "gimmick2").is_err());
    }

    #[test]
    fn load_leaves_the_schedule_unset_when_absent() {
        let config: Config = get_config_from(SAMPLE, "gimmick1").unwrap();

        assert_eq!(config.schedule, None);
    }

    #[test]
    fn load_parses_the_schedule() {
        let contents: String = format!("{}  schedule = 0 9 * * *\n", SAMPLE);

        let config: Config = get_config_from(&contents, "gimmick2").unwrap();

        assert_eq!(config.schedule, Some(CronSchedule::parse("0 9 * * *").unwrap()));
    }

    #[test]
    fn load_rejects_an_invalid_schedule() {
        let contents: String = format!("{}  schedule = 0 9 * * MON\n", SAMPLE);

        assert!(get_config_from(&contents, "gimmick2").is_err());
    }

    #[test]
    fn load_parses_the_check_schedule() {
        let contents: String = format!("{}  check_schedule = 30 8 * * *\n", SAMPLE);

        let config: Config = get_config_from(&contents, "gimmick2").unwrap();

        assert_eq!(config.check_schedule, Some(CronSchedule::parse("30 8 * * *").unwrap()));
    }

    #[test]
    fn load_rejects_an_invalid_check_schedule_naming_its_key() {
        let contents: String = format!("{}  check_schedule = 0 9 * * MON\n", SAMPLE);

        let error: AppError = get_config_from(&contents, "gimmick2").err().unwrap();

        assert!(error.message.contains(CHECK_SCHEDULE_KEY));
    }

    fn get_selected_account_names(requested_account: Option<&str>) -> Result<Vec<String>, AppError> {
        let sections: Vec<Section> = parse(SAMPLE).unwrap();
        let account_sections: Vec<Section> = select_account_sections(sections, requested_account)?;

        Ok(account_sections.into_iter().filter_map(|account_section| account_section.name).collect())
    }

    #[test]
    fn select_account_sections_returns_every_account_in_file_order_without_a_requested_account() {
        assert_eq!(get_selected_account_names(None).unwrap(), vec!["gimmick1", "gimmick2"]);
    }

    #[test]
    fn select_account_sections_returns_only_the_requested_account() {
        assert_eq!(get_selected_account_names(Some("gimmick2")).unwrap(), vec!["gimmick2"]);
    }

    #[test]
    fn select_account_sections_rejects_an_absent_account() {
        assert!(get_selected_account_names(Some("missing")).is_err());
    }

    #[test]
    fn parse_string_value_keeps_quoted_whitespace_and_applies_escapes() {
        assert_eq!(parse_string_value("\"\\n\\n\"").unwrap(), "\n\n");
        assert_eq!(parse_string_value("\"a \\\"b\\\" \\\\\"").unwrap(), "a \"b\" \\");
    }

    #[test]
    fn parse_string_value_rejects_an_unknown_escape() {
        assert!(parse_string_value("\"\\x\"").is_err());
    }
}
