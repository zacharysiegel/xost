use std::fs::OpenOptions;
use std::fs::Permissions;
use std::io::BufRead;
use std::io::Write;
use std::os::unix::fs::OpenOptionsExt;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::path::PathBuf;

use crate::config;
use crate::config::Section;
use crate::cron::CronSchedule;
use crate::error::AppError;
use crate::secrets;

const DEFAULT_SECRETS_STORE: &str = "~/.config/xost/secrets.yaml";
const DATA_ROOT: &str = "~/.local/share/xost";
const OWNER_ONLY_FILE_MODE: u32 = 0o600;

pub struct AccountSettings {
    pub handle: String,
    pub username: String,
    pub chrome_path: Option<String>,
    pub schedule: Option<String>,
}

pub struct Credentials {
    pub password: String,
    pub totp_seed: Option<String>,
}

#[derive(Debug, PartialEq)]
pub struct SecretsLocation {
    pub store: String,
    pub key_base64: String,
}

#[derive(Debug, PartialEq)]
pub enum SectionChange {
    Append(String),
    KeepExisting,
}

pub struct SecretsPlan {
    pub location: SecretsLocation,
    pub section: SectionChange,
}

pub struct ConfigPlan {
    pub secrets: SecretsPlan,
    pub account_section: SectionChange,
}

impl ConfigPlan {
    fn appended_sections(&self) -> Vec<String> {
        [&self.secrets.section, &self.account_section]
            .into_iter()
            .filter_map(|section_change| match section_change {
                SectionChange::Append(text) => Some(text.clone()),
                SectionChange::KeepExisting => None,
            })
            .collect()
    }
}

#[derive(Debug, PartialEq)]
pub enum StoreContents {
    MissingOrBlank,
    HasContent,
}

pub struct PendingSecret {
    pub name: String,
    pub plaintext: String,
}

pub struct SecretPartition {
    pub to_append: Vec<PendingSecret>,
    pub already_stored: Vec<String>,
}

pub fn run(config_path: &Path, explicit_config_path: Option<&Path>, account: &str) -> Result<(), AppError> {
    config::validate_account_name(account)?;

    let existing_config: String = read_existing_config(config_path)?;
    let sections: Vec<Section> = config::parse(&existing_config)?;

    let secrets_plan: SecretsPlan = plan_secrets(&sections, &secrets::generate_key_base64())?;
    let key: Vec<u8> = secrets::decode_key(&secrets_plan.location.key_base64)?;
    let store_path: PathBuf = config::expand_user_path(&secrets_plan.location.store);
    let store_contents: StoreContents = read_store_contents(&store_path)?;
    require_store_usable_with_key(&secrets_plan.section, &store_contents, &store_path)?;

    let settings: AccountSettings = prompt_account_settings()?;
    let credentials: Credentials = prompt_credentials(account)?;

    let plan: ConfigPlan = ConfigPlan {
        secrets: secrets_plan,
        account_section: plan_account_section(&sections, account, &settings),
    };

    append_to_private_file(config_path, &config_text_to_append(&existing_config, &plan))?;
    report_kept_section(&plan.secrets.section, &config::secrets_section_header(), config_path);
    report_kept_section(&plan.account_section, &config::account_section_header(account), config_path);

    store_secrets(&store_path, &key, pending_secrets(account, credentials))?;

    std::fs::create_dir_all(config::expand_user_path(&data_directory(account)))?;

    print_next_steps(account, config_path, explicit_config_path);

    Ok(())
}

pub fn plan_secrets(sections: &[Section], new_key_base64: &str) -> Result<SecretsPlan, AppError> {
    let existing_secrets_location: Option<SecretsLocation> = find_secrets_location(sections)?;

    let secrets_plan: SecretsPlan = match existing_secrets_location {
        Some(location) => SecretsPlan {
            location,
            section: SectionChange::KeepExisting,
        },
        None => {
            let location: SecretsLocation = SecretsLocation {
                store: DEFAULT_SECRETS_STORE.to_string(),
                key_base64: new_key_base64.to_string(),
            };
            let section_text: String = render_secrets_section(&location);

            SecretsPlan {
                location,
                section: SectionChange::Append(section_text),
            }
        }
    };

    Ok(secrets_plan)
}

pub fn plan_account_section(sections: &[Section], account: &str, settings: &AccountSettings) -> SectionChange {
    match config::find_section(sections, config::ACCOUNT_SECTION, Some(account)) {
        Some(_) => SectionChange::KeepExisting,
        None => SectionChange::Append(render_account_section(account, settings)),
    }
}

fn read_store_contents(store_path: &Path) -> Result<StoreContents, AppError> {
    let existing_store: String = read_existing_text(store_path)?;

    if existing_store.trim().is_empty() {
        return Ok(StoreContents::MissingOrBlank);
    }

    Ok(StoreContents::HasContent)
}

pub fn require_store_usable_with_key(
    secrets_section: &SectionChange,
    store_contents: &StoreContents,
    store_path: &Path,
) -> Result<(), AppError> {
    match (secrets_section, store_contents) {
        (SectionChange::Append(_), StoreContents::HasContent) => Err(AppError::new(&format!(
            "config has no {} section, so register would generate a new secr key, but {} already holds secrets \
             encrypted with another key; add a {} section with {} = {} and that store's {}, or move {} aside",
            config::secrets_section_header(),
            store_path.display(),
            config::secrets_section_header(),
            config::SECRETS_STORE_KEY,
            store_path.display(),
            config::SECRETS_KEY_KEY,
            store_path.display(),
        ))),
        _ => Ok(()),
    }
}

fn find_secrets_location(sections: &[Section]) -> Result<Option<SecretsLocation>, AppError> {
    let Some(secrets_values) = config::find_section(sections, config::SECRETS_SECTION, None) else {
        return Ok(None);
    };

    let store: String = config::require(secrets_values, config::SECRETS_STORE_KEY, config::SECRETS_SECTION)?;
    let key_base64: String = config::require(secrets_values, config::SECRETS_KEY_KEY, config::SECRETS_SECTION)?;

    Ok(Some(SecretsLocation { store, key_base64 }))
}

pub fn render_secrets_section(location: &SecretsLocation) -> String {
    let mut section: String = format!("{}\n", config::secrets_section_header());

    push_entry(&mut section, config::SECRETS_STORE_KEY, &location.store);
    push_entry(&mut section, config::SECRETS_KEY_KEY, &location.key_base64);

    section
}

pub fn render_account_section(account: &str, settings: &AccountSettings) -> String {
    let data_directory: String = data_directory(account);
    let mut section: String = format!("{}\n", config::account_section_header(account));

    push_entry(&mut section, config::HANDLE_KEY, &settings.handle);
    push_entry(&mut section, config::USERNAME_KEY, &settings.username);
    push_entry(&mut section, config::DB_KEY, &format!("{}/queue.db", data_directory));
    push_entry(&mut section, config::PROFILE_KEY, &format!("{}/chrome-profile", data_directory));

    if let Some(chrome_path) = &settings.chrome_path {
        push_entry(&mut section, config::CHROME_KEY, chrome_path);
    }

    if let Some(schedule) = &settings.schedule {
        push_entry(&mut section, config::SCHEDULE_KEY, schedule);
    }

    section
}

fn push_entry(section: &mut String, key: &str, value: &str) {
    section.push_str(&format!("  {} = {}\n", key, value));
}

fn data_directory(account: &str) -> String {
    format!("{}/{}", DATA_ROOT, account)
}

pub fn config_text_to_append(existing_config: &str, plan: &ConfigPlan) -> String {
    text_to_append(existing_config, &plan.appended_sections())
}

pub fn text_to_append(existing_text: &str, blocks: &[String]) -> String {
    if blocks.is_empty() {
        return String::new();
    }

    separated_append(existing_text, &blocks.join("\n"))
}

pub fn separated_append(existing_text: &str, block: &str) -> String {
    if existing_text.is_empty() {
        return block.to_string();
    }

    let line_terminator: &str = if existing_text.ends_with('\n') { "" } else { "\n" };

    format!("{}\n{}", line_terminator, block)
}

pub fn pending_secrets(account: &str, credentials: Credentials) -> Vec<PendingSecret> {
    let mut pending: Vec<PendingSecret> = vec![PendingSecret {
        name: secrets::password_secret_name(account),
        plaintext: credentials.password,
    }];

    if let Some(totp_seed) = credentials.totp_seed {
        pending.push(PendingSecret {
            name: secrets::totp_seed_secret_name(account),
            plaintext: totp_seed,
        });
    }

    pending
}

pub fn partition_pending_secrets(stored_secret_names: &[String], pending: Vec<PendingSecret>) -> SecretPartition {
    let (already_stored_secrets, to_append): (Vec<PendingSecret>, Vec<PendingSecret>) = pending
        .into_iter()
        .partition(|pending_secret| stored_secret_names.contains(&pending_secret.name));

    SecretPartition {
        to_append,
        already_stored: already_stored_secrets.into_iter().map(|pending_secret| pending_secret.name).collect(),
    }
}

fn store_secrets(store_path: &Path, key: &Vec<u8>, pending: Vec<PendingSecret>) -> Result<(), AppError> {
    let stored_secret_names: Vec<String> = secrets::read_stored_secret_names(store_path)?;
    let partition: SecretPartition = partition_pending_secrets(&stored_secret_names, pending);

    let mut entries: Vec<String> = Vec::new();

    for pending_secret in &partition.to_append {
        entries.push(secrets::render_encrypted_entry(key, &pending_secret.name, &pending_secret.plaintext)?);
    }

    let existing_store: String = read_existing_text(store_path)?;
    append_to_private_file(store_path, &text_to_append(&existing_store, &entries))?;

    for name in &partition.already_stored {
        println!("secret '{}' is already in {}; leaving it unchanged", name, store_path.display());
    }

    Ok(())
}

fn read_existing_config(config_path: &Path) -> Result<String, AppError> {
    read_existing_text(config_path)
        .map_err(|error| AppError::new(&format!("could not read config at {}: {}", config_path.display(), error)))
}

fn read_existing_text(path: &Path) -> Result<String, AppError> {
    match std::fs::read_to_string(path) {
        Ok(contents) => Ok(contents),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(String::new()),
        Err(error) => Err(error.into()),
    }
}

pub fn append_to_private_file(path: &Path, text: &str) -> Result<(), AppError> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }

    let mut file: std::fs::File = OpenOptions::new()
        .create(true)
        .append(true)
        .mode(OWNER_ONLY_FILE_MODE)
        .open(path)?;
    file.write_all(text.as_bytes())?;

    std::fs::set_permissions(path, Permissions::from_mode(OWNER_ONLY_FILE_MODE))?;

    Ok(())
}

fn report_kept_section(section_change: &SectionChange, header: &str, config_path: &Path) {
    if matches!(section_change, SectionChange::KeepExisting) {
        println!("{} already exists in {}; leaving it unchanged", header, config_path.display());
    }
}

fn print_next_steps(account: &str, config_path: &Path, explicit_config_path: Option<&Path>) {
    let xost: String = xost_command_prefix(explicit_config_path);

    println!();
    println!("Registered account '{}' in {}.", account, config_path.display());
    println!("Next steps:");
    println!(
        "  1. Sign in: {} --account {} import-session --from <file>, or {} --account {} login",
        xost, account, xost, account,
    );
    println!("  2. Queue a post: {} --account {} enqueue --body \"first post\"", xost, account);
    println!(
        "  3. Schedule posting (needs the account's {} key): {} --account {} schedule",
        config::SCHEDULE_KEY,
        xost,
        account,
    );
}

pub fn xost_command_prefix(explicit_config_path: Option<&Path>) -> String {
    match explicit_config_path {
        Some(path) => format!("xost --config {}", path.display()),
        None => "xost".to_string(),
    }
}

fn prompt_account_settings() -> Result<AccountSettings, AppError> {
    let handle: String = prompt_until_non_empty(
        || prompt_line("X handle, without the @: ").map(|answer| normalize_handle(&answer)),
        "the handle is required",
    )?;

    let username_answer: String = prompt_line(&format!("Login identifier (email, phone or handle) [{}]: ", handle))?;
    let chrome_answer: String = prompt_line("Chrome binary path (blank to auto-detect): ")?;
    let schedule: Option<String> = prompt_schedule()?;

    Ok(AccountSettings {
        username: username_or_handle(&username_answer, &handle),
        handle,
        chrome_path: optional_answer(&chrome_answer),
        schedule,
    })
}

fn prompt_credentials(account: &str) -> Result<Credentials, AppError> {
    let password: String = prompt_until_non_empty(
        || prompt_hidden(&format!("X password for '{}': ", account)),
        "the password is required",
    )?;

    let totp_seed_answer: String = prompt_hidden("TOTP base32 seed (blank if the account has no authenticator app): ")?;

    Ok(Credentials {
        password,
        totp_seed: optional_answer(&totp_seed_answer),
    })
}

fn prompt_schedule() -> Result<Option<String>, AppError> {
    loop {
        let answer: String =
            prompt_line("Schedule as a cron expression: minute hour day-of-month month day-of-week (blank for none): ")?;
        let parsed: Result<Option<String>, AppError> = parse_schedule_answer(&answer);

        match parsed {
            Ok(schedule) => return Ok(schedule),
            Err(error) => println!("{}", error.message),
        }
    }
}

fn prompt_until_non_empty(
    read_answer: impl Fn() -> Result<String, AppError>,
    missing_message: &str,
) -> Result<String, AppError> {
    loop {
        let answer: String = read_answer()?;

        if !answer.is_empty() {
            return Ok(answer);
        }

        println!("{}", missing_message);
    }
}

fn prompt_line(prompt: &str) -> Result<String, AppError> {
    print!("{}", prompt);
    std::io::stdout().flush()?;

    let mut answer: String = String::new();
    let bytes_read: usize = std::io::stdin().lock().read_line(&mut answer)?;

    if bytes_read == 0 {
        return Err(AppError::new("standard input closed before registration finished"));
    }

    Ok(answer.trim().to_string())
}

fn prompt_hidden(prompt: &str) -> Result<String, AppError> {
    let answer: String = rpassword::prompt_password(prompt)?;

    Ok(answer)
}

pub fn parse_schedule_answer(answer: &str) -> Result<Option<String>, AppError> {
    let expression: &str = answer.trim();

    if expression.is_empty() {
        return Ok(None);
    }

    CronSchedule::parse(expression)?;

    Ok(Some(expression.to_string()))
}

pub fn normalize_handle(answer: &str) -> String {
    let trimmed: &str = answer.trim();

    trimmed.strip_prefix('@').unwrap_or(trimmed).to_string()
}

pub fn username_or_handle(answer: &str, handle: &str) -> String {
    match optional_answer(answer) {
        Some(username) => username,
        None => handle.to_string(),
    }
}

fn optional_answer(answer: &str) -> Option<String> {
    let trimmed: &str = answer.trim();

    if trimmed.is_empty() {
        return None;
    }

    Some(trimmed.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    const EXISTING_CONFIG: &str = "\
[secrets]
  store = /tmp/existing-secrets.yaml
  key = EXISTINGKEY

[account \"daily\"]
  handle = dailybot
  db = /tmp/daily/queue.db
  profile = /tmp/daily/profile
";

    fn sample_settings() -> AccountSettings {
        AccountSettings {
            handle: "funnybot".to_string(),
            username: "funnybot@example.com".to_string(),
            chrome_path: None,
            schedule: None,
        }
    }

    fn plan_against(existing_config: &str, account: &str) -> ConfigPlan {
        let sections: Vec<Section> = config::parse(existing_config).unwrap();

        ConfigPlan {
            secrets: plan_secrets(&sections, "NEWKEY").unwrap(),
            account_section: plan_account_section(&sections, account, &sample_settings()),
        }
    }

    #[test]
    fn config_text_to_append_writes_a_secrets_section_and_the_account_section_into_a_new_file() {
        let plan: ConfigPlan = plan_against("", "gimmick");

        let expected: &str = "\
[secrets]
  store = ~/.config/xost/secrets.yaml
  key = NEWKEY

[account \"gimmick\"]
  handle = funnybot
  username = funnybot@example.com
  db = ~/.local/share/xost/gimmick/queue.db
  profile = ~/.local/share/xost/gimmick/chrome-profile
";
        assert_eq!(config_text_to_append("", &plan), expected);
    }

    #[test]
    fn plan_secrets_reuses_an_existing_secrets_section() {
        let plan: ConfigPlan = plan_against(EXISTING_CONFIG, "gimmick");

        let expected_location: SecretsLocation = SecretsLocation {
            store: "/tmp/existing-secrets.yaml".to_string(),
            key_base64: "EXISTINGKEY".to_string(),
        };
        assert_eq!(plan.secrets.location, expected_location);
        assert_eq!(plan.secrets.section, SectionChange::KeepExisting);
    }

    #[test]
    fn plan_account_section_leaves_an_existing_account_section_unchanged() {
        let plan: ConfigPlan = plan_against(EXISTING_CONFIG, "daily");

        assert_eq!(plan.account_section, SectionChange::KeepExisting);
    }

    #[test]
    fn plan_secrets_rejects_a_secrets_section_without_a_key() {
        let sections: Vec<Section> = config::parse("[secrets]\n  store = /tmp/secrets.yaml\n").unwrap();

        assert!(plan_secrets(&sections, "NEWKEY").is_err());
    }

    #[test]
    fn require_store_usable_with_key_rejects_a_new_key_for_a_store_with_content() {
        let new_secrets_section: SectionChange = SectionChange::Append("[secrets]\n".to_string());

        let result: Result<(), AppError> =
            require_store_usable_with_key(&new_secrets_section, &StoreContents::HasContent, Path::new("/tmp/secrets.yaml"));

        assert!(result.is_err());
    }

    #[test]
    fn require_store_usable_with_key_accepts_a_new_key_for_a_missing_or_blank_store() {
        let new_secrets_section: SectionChange = SectionChange::Append("[secrets]\n".to_string());

        let result: Result<(), AppError> =
            require_store_usable_with_key(&new_secrets_section, &StoreContents::MissingOrBlank, Path::new("/tmp/secrets.yaml"));

        assert!(result.is_ok());
    }

    #[test]
    fn xost_command_prefix_includes_an_explicit_config_path() {
        assert_eq!(xost_command_prefix(Some(Path::new("/tmp/xost.config"))), "xost --config /tmp/xost.config");
    }

    #[test]
    fn text_to_append_is_empty_without_blocks() {
        assert_eq!(text_to_append("a = 1\n", &[]), "");
    }

    #[test]
    fn render_account_section_includes_chrome_and_schedule_when_given() {
        let settings: AccountSettings = AccountSettings {
            handle: "funnybot".to_string(),
            username: "funnybot".to_string(),
            chrome_path: Some("/Applications/Google Chrome.app/Contents/MacOS/Google Chrome".to_string()),
            schedule: Some("0 9 * * *".to_string()),
        };

        let section: String = render_account_section("gimmick", &settings);

        assert!(section.ends_with(
            "  chrome = /Applications/Google Chrome.app/Contents/MacOS/Google Chrome\n  schedule = 0 9 * * *\n"
        ));
    }

    #[test]
    fn render_account_section_is_read_back_by_config_load() {
        let config_path: PathBuf = std::env::temp_dir().join(format!("xost-register-test-{}.config", std::process::id()));
        let location: SecretsLocation = SecretsLocation {
            store: "/tmp/secrets.yaml".to_string(),
            key_base64: "KEY".to_string(),
        };
        let contents: String = format!(
            "{}\n{}",
            render_secrets_section(&location),
            render_account_section("gimmick", &sample_settings()),
        );
        std::fs::write(&config_path, contents).unwrap();

        let loaded: Result<config::Config, AppError> = config::Config::load(&config_path, "gimmick");
        std::fs::remove_file(&config_path).ok();

        let loaded: config::Config = loaded.unwrap();
        assert_eq!(loaded.handle, "funnybot");
        assert_eq!(loaded.username, "funnybot@example.com");
    }

    #[test]
    fn separated_append_separates_the_block_with_a_blank_line() {
        assert_eq!(separated_append("a = 1\n", "[b]\n"), "\n[b]\n");
    }

    #[test]
    fn separated_append_terminates_an_unterminated_last_line() {
        assert_eq!(separated_append("a = 1", "[b]\n"), "\n\n[b]\n");
    }

    #[test]
    fn pending_secrets_omits_the_totp_seed_when_absent() {
        let credentials: Credentials = Credentials {
            password: "hunter2".to_string(),
            totp_seed: None,
        };

        let names: Vec<String> = pending_secrets("gimmick", credentials)
            .into_iter()
            .map(|pending_secret| pending_secret.name)
            .collect();

        assert_eq!(names, vec!["gimmick_password".to_string()]);
    }

    #[test]
    fn partition_pending_secrets_keeps_a_secret_already_in_the_store() {
        let pending: Vec<PendingSecret> = vec![
            PendingSecret {
                name: "gimmick_password".to_string(),
                plaintext: "hunter2".to_string(),
            },
            PendingSecret {
                name: "gimmick_totp_seed".to_string(),
                plaintext: "SEED".to_string(),
            },
        ];

        let partition: SecretPartition = partition_pending_secrets(&["gimmick_password".to_string()], pending);

        let appended_names: Vec<String> = partition
            .to_append
            .into_iter()
            .map(|pending_secret| pending_secret.name)
            .collect();
        assert_eq!(appended_names, vec!["gimmick_totp_seed".to_string()]);
        assert_eq!(partition.already_stored, vec!["gimmick_password".to_string()]);
    }

    #[test]
    fn append_to_private_file_creates_the_file_readable_only_by_its_owner() {
        let directory: PathBuf = std::env::temp_dir().join(format!("xost-register-test-{}", std::process::id()));
        let path: PathBuf = directory.join("nested").join("config");

        append_to_private_file(&path, "[secrets]\n").unwrap();
        let mode: u32 = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        std::fs::remove_dir_all(&directory).ok();

        assert_eq!(mode, OWNER_ONLY_FILE_MODE);
    }

    #[test]
    fn parse_schedule_answer_treats_a_blank_answer_as_no_schedule() {
        assert_eq!(parse_schedule_answer("  ").unwrap(), None);
    }

    #[test]
    fn parse_schedule_answer_rejects_an_invalid_expression() {
        assert!(parse_schedule_answer("0 9 * * MON").is_err());
    }

    #[test]
    fn normalize_handle_strips_a_leading_at_sign() {
        assert_eq!(normalize_handle(" @funnybot "), "funnybot");
    }

    #[test]
    fn username_or_handle_defaults_to_the_handle_when_blank() {
        assert_eq!(username_or_handle("", "funnybot"), "funnybot");
    }
}
