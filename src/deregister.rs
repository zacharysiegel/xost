use std::collections::BTreeMap;
use std::path::Path;
use std::path::PathBuf;

use crate::config;
use crate::config::Section;
use crate::error::AppError;
use crate::private_file;
use crate::prompt;
use crate::schedule;
use crate::secrets;

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum DataHandling {
    Delete,
    Keep,
}

#[derive(Debug, PartialEq)]
pub enum Confirmation {
    Confirmed,
    Declined,
}

pub struct ObservedAccount {
    pub installed_plist_path: Option<String>,
    pub secrets_store: Option<ObservedSecretsStore>,
    pub account_section: Option<ObservedAccountSection>,
    pub other_account_data_paths: Vec<OtherAccountDataPath>,
}

pub struct OtherAccountDataPath {
    pub account: String,
    pub data_path: PathBuf,
}

pub struct ObservedSecretsStore {
    pub store_path: PathBuf,
    pub stored_secret_names: Vec<String>,
}

pub struct ObservedAccountSection {
    pub existing_db_path: Option<PathBuf>,
    pub existing_profile_dir: Option<PathBuf>,
}

impl ObservedAccountSection {
    fn existing_data_paths(&self) -> Vec<PathBuf> {
        [&self.existing_db_path, &self.existing_profile_dir]
            .into_iter()
            .flatten()
            .cloned()
            .collect()
    }
}

#[derive(Debug, PartialEq)]
pub enum DeregisterStep {
    RemoveScheduledJob {
        account: String,
        installed_plist_path: String,
    },
    RemoveSecrets {
        store_path: PathBuf,
        secret_names: Vec<String>,
    },
    RemoveAccountSection {
        account: String,
        config_path: PathBuf,
    },
    DeleteDatabaseFile {
        db_path: PathBuf,
    },
    DeleteProfileDirectory {
        profile_dir: PathBuf,
    },
    DeleteDirectoryIfEmpty {
        directory: PathBuf,
    },
}

impl DeregisterStep {
    fn deleted_data_path(&self) -> Option<&Path> {
        match self {
            DeregisterStep::DeleteDatabaseFile { db_path } => Some(db_path),
            DeregisterStep::DeleteProfileDirectory { profile_dir } => Some(profile_dir),
            DeregisterStep::DeleteDirectoryIfEmpty { directory } => Some(directory),
            DeregisterStep::RemoveScheduledJob { .. }
            | DeregisterStep::RemoveSecrets { .. }
            | DeregisterStep::RemoveAccountSection { .. } => None,
        }
    }

    fn description(&self) -> String {
        match self {
            DeregisterStep::RemoveScheduledJob {
                installed_plist_path, ..
            } => format!("the scheduled job {}", installed_plist_path),
            DeregisterStep::RemoveSecrets {
                store_path,
                secret_names,
            } => format!("the secrets {} in {}", secret_names.join(" and "), store_path.display()),
            DeregisterStep::RemoveAccountSection { account, config_path } => {
                format!("the {} section of {}", config::account_section_header(account), config_path.display())
            }
            DeregisterStep::DeleteDatabaseFile { db_path } => format!("the queue database {}", db_path.display()),
            DeregisterStep::DeleteProfileDirectory { profile_dir } => {
                format!("the Chrome profile directory {}", profile_dir.display())
            }
            DeregisterStep::DeleteDirectoryIfEmpty { directory } => {
                format!("the directory {} if it is then empty", directory.display())
            }
        }
    }
}

enum StepOutcome {
    Removed,
    KeptNonEmptyDirectory(PathBuf),
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum LinePosition {
    InsideRemovedSection,
    OutsideRemovedSection,
}

#[derive(Debug, PartialEq)]
pub enum KeptReason {
    DeleteDataNotRequested,
    SharedWithAccount(String),
}

#[derive(Debug, PartialEq)]
pub struct KeptDataPath {
    pub path: PathBuf,
    pub reason: KeptReason,
}

pub struct DeregisterPlan {
    pub steps: Vec<DeregisterStep>,
    pub kept_data_paths: Vec<KeptDataPath>,
}

pub fn run(config_path: &Path, account: &str, data_handling: DataHandling) -> Result<(), AppError> {
    config::validate_account_name(account)?;

    let config_text: String = config::read_existing_config(config_path)?;
    let sections: Vec<Section> = config::parse(&config_text)?;
    let observed_account: ObservedAccount = observe_account(account, &sections)?;
    let plan: DeregisterPlan = build_plan(account, config_path, observed_account, data_handling);

    if plan.steps.is_empty() {
        println!("nothing was found for account '{}'; nothing to remove", account);

        return Ok(());
    }

    print_plan(account, &plan);

    let answer: String = prompt::read_line_answer("Delete these? [y/N] ")?;
    let confirmation: Confirmation = parse_confirmation(&answer);

    if confirmation == Confirmation::Declined {
        println!("aborted; nothing was removed");

        return Ok(());
    }

    execute_plan(&plan.steps)?;

    println!("deregistered account '{}'", account);

    Ok(())
}

fn observe_account(account: &str, sections: &[Section]) -> Result<ObservedAccount, AppError> {
    let installed_plist_path: String = schedule::installed_plist_path(&schedule::job_label(account));
    let installed_plist_present: bool = Path::new(&installed_plist_path).exists();

    let secrets_store: Option<ObservedSecretsStore> = observe_secrets_store(sections)?;
    let account_section: Option<ObservedAccountSection> =
        config::find_section(sections, config::ACCOUNT_SECTION, Some(account)).map(observe_account_section);
    let other_account_data_paths: Vec<OtherAccountDataPath> = observe_other_account_data_paths(account, sections);

    Ok(ObservedAccount {
        installed_plist_path: installed_plist_present.then_some(installed_plist_path),
        secrets_store,
        account_section,
        other_account_data_paths,
    })
}

fn observe_other_account_data_paths(account: &str, sections: &[Section]) -> Vec<OtherAccountDataPath> {
    let mut other_account_data_paths: Vec<OtherAccountDataPath> = Vec::new();

    for account_section in config::find_named_sections(sections, config::ACCOUNT_SECTION) {
        if account_section.name == account {
            continue;
        }

        for key in [config::DB_KEY, config::PROFILE_KEY] {
            let Some(raw_path) = account_section.values.get(key).filter(|raw_path| !raw_path.is_empty())
            else {
                continue;
            };

            other_account_data_paths.push(OtherAccountDataPath {
                account: account_section.name.to_string(),
                data_path: config::expand_user_path(raw_path),
            });
        }
    }

    other_account_data_paths
}

fn observe_secrets_store(sections: &[Section]) -> Result<Option<ObservedSecretsStore>, AppError> {
    let configured_store: Option<&String> = config::find_section(sections, config::SECRETS_SECTION, None)
        .and_then(|secrets_values| secrets_values.get(config::SECRETS_STORE_KEY))
        .filter(|store| !store.is_empty());

    let Some(store) = configured_store else {
        return Ok(None);
    };

    let store_path: PathBuf = config::expand_user_path(store);
    let stored_secret_names: Vec<String> = secrets::read_stored_secret_names(&store_path)?;

    Ok(Some(ObservedSecretsStore {
        store_path,
        stored_secret_names,
    }))
}

fn observe_account_section(account_values: &BTreeMap<String, String>) -> ObservedAccountSection {
    ObservedAccountSection {
        existing_db_path: get_existing_configured_path(account_values, config::DB_KEY),
        existing_profile_dir: get_existing_configured_path(account_values, config::PROFILE_KEY),
    }
}

fn get_existing_configured_path(values: &BTreeMap<String, String>, key: &str) -> Option<PathBuf> {
    let raw: &String = values.get(key).filter(|raw| !raw.is_empty())?;
    let path: PathBuf = config::expand_user_path(raw);
    let path_exists: bool = path.exists();

    path_exists.then_some(path)
}

pub fn build_plan(
    account: &str,
    config_path: &Path,
    observed_account: ObservedAccount,
    data_handling: DataHandling,
) -> DeregisterPlan {
    let mut steps: Vec<DeregisterStep> = Vec::new();
    let mut kept_data_paths: Vec<KeptDataPath> = Vec::new();

    if let Some(installed_plist_path) = observed_account.installed_plist_path {
        steps.push(DeregisterStep::RemoveScheduledJob {
            account: account.to_string(),
            installed_plist_path,
        });
    }

    if let Some(secrets_store) = observed_account.secrets_store {
        steps.extend(get_secrets_step(account, secrets_store));
    }

    if let Some(account_section) = observed_account.account_section {
        steps.push(DeregisterStep::RemoveAccountSection {
            account: account.to_string(),
            config_path: config_path.to_path_buf(),
        });

        let data_plan: DeregisterPlan = match data_handling {
            DataHandling::Delete => plan_data_deletion(&account_section, &observed_account.other_account_data_paths),
            DataHandling::Keep => plan_data_keeping(&account_section),
        };

        steps.extend(data_plan.steps);
        kept_data_paths = data_plan.kept_data_paths;
    }

    DeregisterPlan {
        steps,
        kept_data_paths,
    }
}

fn get_secrets_step(account: &str, secrets_store: ObservedSecretsStore) -> Option<DeregisterStep> {
    let secret_names: Vec<String> = [secrets::password_secret_name(account), secrets::totp_seed_secret_name(account)]
        .into_iter()
        .filter(|secret_name| secrets_store.stored_secret_names.contains(secret_name))
        .collect();

    if secret_names.is_empty() {
        return None;
    }

    Some(DeregisterStep::RemoveSecrets {
        store_path: secrets_store.store_path,
        secret_names,
    })
}

fn plan_data_keeping(account_section: &ObservedAccountSection) -> DeregisterPlan {
    let kept_data_paths: Vec<KeptDataPath> = account_section
        .existing_data_paths()
        .into_iter()
        .map(|path| KeptDataPath {
            path,
            reason: KeptReason::DeleteDataNotRequested,
        })
        .collect();

    DeregisterPlan {
        steps: Vec::new(),
        kept_data_paths,
    }
}

fn plan_data_deletion(
    account_section: &ObservedAccountSection,
    other_account_data_paths: &[OtherAccountDataPath],
) -> DeregisterPlan {
    let mut steps: Vec<DeregisterStep> = Vec::new();
    let mut kept_data_paths: Vec<KeptDataPath> = Vec::new();

    for step in get_data_deletion_steps(account_section) {
        let shared_data_path: Option<KeptDataPath> = step
            .deleted_data_path()
            .and_then(|deleted_path| get_shared_data_path(deleted_path, other_account_data_paths));

        match shared_data_path {
            Some(kept_data_path) => kept_data_paths.push(kept_data_path),
            None => steps.push(step),
        }
    }

    DeregisterPlan {
        steps,
        kept_data_paths,
    }
}

/// A path is shared when it equals, contains or lies inside another account's data path.
fn get_shared_data_path(path: &Path, other_account_data_paths: &[OtherAccountDataPath]) -> Option<KeptDataPath> {
    let comparable_path: PathBuf = get_comparable_path(path);

    let sharing_account_data_path: &OtherAccountDataPath = other_account_data_paths.iter().find(|other_account_data_path| {
        let other_comparable_path: PathBuf = get_comparable_path(&other_account_data_path.data_path);

        comparable_path.starts_with(&other_comparable_path) || other_comparable_path.starts_with(&comparable_path)
    })?;

    Some(KeptDataPath {
        path: path.to_path_buf(),
        reason: KeptReason::SharedWithAccount(sharing_account_data_path.account.clone()),
    })
}

// Symlinks and "." or ".." segments can make two spellings name the same directory.
fn get_comparable_path(path: &Path) -> PathBuf {
    std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}

fn get_data_deletion_steps(account_section: &ObservedAccountSection) -> Vec<DeregisterStep> {
    let mut steps: Vec<DeregisterStep> = Vec::new();

    if let Some(db_path) = &account_section.existing_db_path {
        steps.push(DeregisterStep::DeleteDatabaseFile { db_path: db_path.clone() });
    }

    if let Some(profile_dir) = &account_section.existing_profile_dir {
        steps.push(DeregisterStep::DeleteProfileDirectory {
            profile_dir: profile_dir.clone(),
        });
    }

    for directory in get_parent_directories(account_section) {
        steps.push(DeregisterStep::DeleteDirectoryIfEmpty { directory });
    }

    steps
}

fn get_parent_directories(account_section: &ObservedAccountSection) -> Vec<PathBuf> {
    let mut directories: Vec<PathBuf> = Vec::new();

    for data_path in account_section.existing_data_paths() {
        let Some(parent) = data_path.parent().filter(|parent| !parent.as_os_str().is_empty())
        else {
            continue;
        };

        let inside_deleted_profile: bool = account_section
            .existing_profile_dir
            .as_ref()
            .is_some_and(|profile_dir| parent.starts_with(profile_dir));

        if !inside_deleted_profile && !directories.iter().any(|directory| directory == parent) {
            directories.push(parent.to_path_buf());
        }
    }

    directories
}

fn print_plan(account: &str, plan: &DeregisterPlan) {
    println!("Deregistering account '{}' removes:", account);

    for step in &plan.steps {
        println!("  - {}", step.description());
    }

    let unrequested_paths: Vec<String> = plan
        .kept_data_paths
        .iter()
        .filter(|kept_data_path| kept_data_path.reason == KeptReason::DeleteDataNotRequested)
        .map(|kept_data_path| kept_data_path.path.display().to_string())
        .collect();

    if !unrequested_paths.is_empty() {
        println!("Kept data (pass --delete-data to delete it): {}", unrequested_paths.join(", "));
    }

    for kept_data_path in &plan.kept_data_paths {
        if let KeptReason::SharedWithAccount(other_account) = &kept_data_path.reason {
            println!("Kept {}: account '{}' also uses it", kept_data_path.path.display(), other_account);
        }
    }
}

pub fn parse_confirmation(answer: &str) -> Confirmation {
    let normalized_answer: String = answer.trim().to_lowercase();

    match normalized_answer.as_str() {
        "y" | "yes" => Confirmation::Confirmed,
        _ => Confirmation::Declined,
    }
}

fn execute_plan(steps: &[DeregisterStep]) -> Result<(), AppError> {
    for (step_index, step) in steps.iter().enumerate() {
        let step_result: Result<StepOutcome, AppError> = execute_step(step);

        match step_result {
            Ok(StepOutcome::Removed) => println!("removed {}", step.description()),
            Ok(StepOutcome::KeptNonEmptyDirectory(directory)) => {
                println!("kept the directory {}: it is not empty", directory.display())
            }
            Err(error) => return Err(AppError::new(&describe_partial_failure(steps, step_index, &error))),
        }
    }

    Ok(())
}

fn execute_step(step: &DeregisterStep) -> Result<StepOutcome, AppError> {
    match step {
        DeregisterStep::RemoveScheduledJob { account, .. } => schedule::remove(account)?,
        DeregisterStep::RemoveSecrets {
            store_path,
            secret_names,
        } => remove_secrets_from_store(store_path, secret_names)?,
        DeregisterStep::RemoveAccountSection { account, config_path } => remove_account_section_from_file(config_path, account)?,
        DeregisterStep::DeleteDatabaseFile { db_path } => std::fs::remove_file(db_path)?,
        DeregisterStep::DeleteProfileDirectory { profile_dir } => std::fs::remove_dir_all(profile_dir)?,
        DeregisterStep::DeleteDirectoryIfEmpty { directory } => return delete_directory_if_empty(directory),
    }

    Ok(StepOutcome::Removed)
}

fn remove_secrets_from_store(store_path: &Path, secret_names: &[String]) -> Result<(), AppError> {
    let store_text: String = private_file::read_existing_text(store_path)?;
    let remaining_secret_names: Vec<String> = secrets::read_stored_secret_names(store_path)?
        .into_iter()
        .filter(|stored_secret_name| !secret_names.contains(stored_secret_name))
        .collect();

    let remaining_store_text: String = secrets::render_store_without_secrets(&store_text, secret_names);

    private_file::replace_checked_private_file(store_path, &remaining_store_text, |written_path| {
        secrets::verify_store_file(written_path, &remaining_secret_names)
    })
}

fn remove_account_section_from_file(config_path: &Path, account: &str) -> Result<(), AppError> {
    let config_text: String = config::read_existing_config(config_path)?;

    private_file::replace_private_file(config_path, &remove_account_section(&config_text, account))
}

fn delete_directory_if_empty(directory: &Path) -> Result<StepOutcome, AppError> {
    let first_entry: Option<Result<std::fs::DirEntry, std::io::Error>> = std::fs::read_dir(directory)?.next();

    if first_entry.is_some() {
        return Ok(StepOutcome::KeptNonEmptyDirectory(directory.to_path_buf()));
    }

    std::fs::remove_dir(directory)?;

    Ok(StepOutcome::Removed)
}

pub fn describe_partial_failure(steps: &[DeregisterStep], failed_step_index: usize, error: &AppError) -> String {
    let removed_descriptions: Vec<String> = steps[..failed_step_index].iter().map(DeregisterStep::description).collect();
    let not_removed_descriptions: Vec<String> = steps[failed_step_index..].iter().map(DeregisterStep::description).collect();

    let removed_summary: String = match removed_descriptions.is_empty() {
        true => "nothing".to_string(),
        false => removed_descriptions.join(", "),
    };

    format!(
        "could not remove {}: {}\nRemoved: {}\nNot removed: {}",
        steps[failed_step_index].description(),
        error.message,
        removed_summary,
        not_removed_descriptions.join(", "),
    )
}

/// Removes every `[account "<account>"]` section from the blank lines above its header through its last key line.
pub fn remove_account_section(config_text: &str, account: &str) -> String {
    let config_lines: Vec<&str> = config_text.split_inclusive('\n').collect();
    let mut line_positions: Vec<LinePosition> = vec![LinePosition::OutsideRemovedSection; config_lines.len()];

    for (header_index, line) in config_lines.iter().enumerate() {
        if !config::is_account_section_header_line(line, account) {
            continue;
        }

        let first_removed_index: usize = get_first_preceding_blank_line_index(&config_lines, header_index);
        let last_key_line_index: usize = get_last_key_line_index(&config_lines, header_index);

        // With nothing kept above it, the blank lines after the section would open the file.
        let last_removed_index: usize = match has_kept_content_before(&config_lines, &line_positions, first_removed_index) {
            true => last_key_line_index,
            false => get_last_following_blank_line_index(&config_lines, last_key_line_index),
        };

        for line_position in &mut line_positions[first_removed_index..=last_removed_index] {
            *line_position = LinePosition::InsideRemovedSection;
        }
    }

    let kept_lines: Vec<&str> = config_lines
        .iter()
        .zip(&line_positions)
        .filter(|(_, line_position)| **line_position == LinePosition::OutsideRemovedSection)
        .map(|(line, _)| *line)
        .collect();

    kept_lines.concat()
}

fn get_first_preceding_blank_line_index(config_lines: &[&str], header_index: usize) -> usize {
    let mut first_index: usize = header_index;

    while first_index > 0 && config_lines[first_index - 1].trim().is_empty() {
        first_index -= 1;
    }

    first_index
}

fn get_last_following_blank_line_index(config_lines: &[&str], line_index: usize) -> usize {
    let mut last_index: usize = line_index;

    while last_index + 1 < config_lines.len() && config_lines[last_index + 1].trim().is_empty() {
        last_index += 1;
    }

    last_index
}

fn has_kept_content_before(config_lines: &[&str], line_positions: &[LinePosition], line_index: usize) -> bool {
    config_lines[..line_index]
        .iter()
        .zip(line_positions)
        .any(|(line, line_position)| *line_position == LinePosition::OutsideRemovedSection && !line.trim().is_empty())
}

fn get_last_key_line_index(config_lines: &[&str], header_index: usize) -> usize {
    let mut last_key_line_index: usize = header_index;

    for (line_index, line) in config_lines.iter().enumerate().skip(header_index + 1) {
        if config::is_section_header_line(line) {
            break;
        }

        let trimmed_line: &str = line.trim();
        let is_key_line: bool = !trimmed_line.is_empty() && !trimmed_line.starts_with('#');

        if is_key_line {
            last_key_line_index = line_index;
        }
    }

    last_key_line_index
}

#[cfg(test)]
mod tests {
    use super::*;

    const CONFIG_WITH_THREE_ACCOUNTS: &str = "\
[secrets]
  store = /tmp/secrets.yaml
  key = KEY

[account \"daily\"]
  handle = dailybot
  db = /tmp/daily/queue.db

[account \"gimmick\"]
  handle = funnybot
  db = /tmp/gimmick/queue.db

[account \"weekly\"]
  handle = weeklybot
";

    fn get_observed_section(db_path: Option<&str>, profile_dir: Option<&str>) -> ObservedAccountSection {
        ObservedAccountSection {
            existing_db_path: db_path.map(PathBuf::from),
            existing_profile_dir: profile_dir.map(PathBuf::from),
        }
    }

    fn get_plan(observed_account: ObservedAccount, data_handling: DataHandling) -> DeregisterPlan {
        build_plan("gimmick", Path::new("/tmp/config"), observed_account, data_handling)
    }

    fn get_observed_account_with_data() -> ObservedAccount {
        ObservedAccount {
            installed_plist_path: None,
            secrets_store: None,
            account_section: Some(get_observed_section(
                Some("/tmp/xost/gimmick/queue.db"),
                Some("/tmp/xost/gimmick/chrome-profile"),
            )),
            other_account_data_paths: Vec::new(),
        }
    }

    #[test]
    fn remove_account_section_removes_a_middle_section() {
        let expected: &str = "\
[secrets]
  store = /tmp/secrets.yaml
  key = KEY

[account \"daily\"]
  handle = dailybot
  db = /tmp/daily/queue.db

[account \"weekly\"]
  handle = weeklybot
";

        assert_eq!(remove_account_section(CONFIG_WITH_THREE_ACCOUNTS, "gimmick"), expected);
    }

    #[test]
    fn remove_account_section_removes_the_last_section_with_its_preceding_blank_line() {
        let expected: &str = "\
[secrets]
  store = /tmp/secrets.yaml
  key = KEY

[account \"daily\"]
  handle = dailybot
  db = /tmp/daily/queue.db

[account \"gimmick\"]
  handle = funnybot
  db = /tmp/gimmick/queue.db
";

        assert_eq!(remove_account_section(CONFIG_WITH_THREE_ACCOUNTS, "weekly"), expected);
    }

    #[test]
    fn remove_account_section_leaves_text_without_the_section_unchanged() {
        assert_eq!(remove_account_section(CONFIG_WITH_THREE_ACCOUNTS, "missing"), CONFIG_WITH_THREE_ACCOUNTS);
    }

    #[test]
    fn remove_account_section_keeps_the_comments_above_the_next_section_header() {
        let config_text: &str = "\
[account \"daily\"]
  handle = dailybot

[account \"gimmick\"]
  handle = funnybot

# posts once a week
[account \"weekly\"]
  handle = weeklybot
";
        let expected: &str = "\
[account \"daily\"]
  handle = dailybot

# posts once a week
[account \"weekly\"]
  handle = weeklybot
";

        assert_eq!(remove_account_section(config_text, "gimmick"), expected);
    }

    #[test]
    fn remove_account_section_keeps_the_comments_after_the_last_section_at_the_end_of_the_file() {
        let config_text: &str = "\
[account \"daily\"]
  handle = dailybot

[account \"gimmick\"]
  handle = funnybot

# retired accounts
";
        let expected: &str = "\
[account \"daily\"]
  handle = dailybot

# retired accounts
";

        assert_eq!(remove_account_section(config_text, "gimmick"), expected);
    }

    #[test]
    fn remove_account_section_removes_every_duplicate_section() {
        let config_text: &str = "\
[account \"gimmick\"]
  handle = funnybot

[account \"daily\"]
  handle = dailybot

[account \"gimmick\"]
  handle = otherbot
";
        let expected: &str = "\
[account \"daily\"]
  handle = dailybot
";

        assert_eq!(remove_account_section(config_text, "gimmick"), expected);
    }

    #[test]
    fn build_plan_lists_only_what_exists() {
        let observed_account: ObservedAccount = ObservedAccount {
            installed_plist_path: None,
            secrets_store: Some(ObservedSecretsStore {
                store_path: PathBuf::from("/tmp/secrets.yaml"),
                stored_secret_names: vec!["daily_password".to_string(), "gimmick_password".to_string()],
            }),
            account_section: Some(get_observed_section(Some("/tmp/gimmick/queue.db"), None)),
            other_account_data_paths: Vec::new(),
        };

        let plan: DeregisterPlan = get_plan(observed_account, DataHandling::Keep);

        let expected_steps: Vec<DeregisterStep> = vec![
            DeregisterStep::RemoveSecrets {
                store_path: PathBuf::from("/tmp/secrets.yaml"),
                secret_names: vec!["gimmick_password".to_string()],
            },
            DeregisterStep::RemoveAccountSection {
                account: "gimmick".to_string(),
                config_path: PathBuf::from("/tmp/config"),
            },
        ];
        assert_eq!(plan.steps, expected_steps);
    }

    #[test]
    fn build_plan_reports_the_existing_data_paths_as_kept_without_delete_data() {
        let plan: DeregisterPlan = get_plan(get_observed_account_with_data(), DataHandling::Keep);

        let expected_kept_data_paths: Vec<KeptDataPath> = vec![
            KeptDataPath {
                path: PathBuf::from("/tmp/xost/gimmick/queue.db"),
                reason: KeptReason::DeleteDataNotRequested,
            },
            KeptDataPath {
                path: PathBuf::from("/tmp/xost/gimmick/chrome-profile"),
                reason: KeptReason::DeleteDataNotRequested,
            },
        ];
        assert_eq!(plan.kept_data_paths, expected_kept_data_paths);
    }

    #[test]
    fn build_plan_deletes_the_data_and_then_its_shared_parent_directory_with_delete_data() {
        let plan: DeregisterPlan = get_plan(get_observed_account_with_data(), DataHandling::Delete);

        let expected_data_steps: [DeregisterStep; 3] = [
            DeregisterStep::DeleteDatabaseFile {
                db_path: PathBuf::from("/tmp/xost/gimmick/queue.db"),
            },
            DeregisterStep::DeleteProfileDirectory {
                profile_dir: PathBuf::from("/tmp/xost/gimmick/chrome-profile"),
            },
            DeregisterStep::DeleteDirectoryIfEmpty {
                directory: PathBuf::from("/tmp/xost/gimmick"),
            },
        ];
        assert_eq!(plan.steps[1..], expected_data_steps);
    }

    #[test]
    fn build_plan_removes_the_scheduled_job_when_its_plist_exists() {
        let observed_account: ObservedAccount = ObservedAccount {
            installed_plist_path: Some("/Library/LaunchDaemons/xost.gimmick.plist".to_string()),
            secrets_store: None,
            account_section: None,
            other_account_data_paths: Vec::new(),
        };

        let plan: DeregisterPlan = get_plan(observed_account, DataHandling::Keep);

        let expected_steps: Vec<DeregisterStep> = vec![DeregisterStep::RemoveScheduledJob {
            account: "gimmick".to_string(),
            installed_plist_path: "/Library/LaunchDaemons/xost.gimmick.plist".to_string(),
        }];
        assert_eq!(plan.steps, expected_steps);
    }

    #[test]
    fn build_plan_skips_a_parent_directory_inside_the_deleted_profile_directory() {
        let observed_account: ObservedAccount = ObservedAccount {
            installed_plist_path: None,
            secrets_store: None,
            account_section: Some(get_observed_section(
                Some("/tmp/xost/gimmick/chrome-profile/queue.db"),
                Some("/tmp/xost/gimmick/chrome-profile"),
            )),
            other_account_data_paths: Vec::new(),
        };

        let plan: DeregisterPlan = get_plan(observed_account, DataHandling::Delete);

        let directories_deleted_if_empty: Vec<&DeregisterStep> = plan
            .steps
            .iter()
            .filter(|step| matches!(step, DeregisterStep::DeleteDirectoryIfEmpty { .. }))
            .collect();
        let expected_step: DeregisterStep = DeregisterStep::DeleteDirectoryIfEmpty {
            directory: PathBuf::from("/tmp/xost/gimmick"),
        };
        assert_eq!(directories_deleted_if_empty, vec![&expected_step]);
    }

    #[test]
    fn build_plan_keeps_data_paths_shared_with_another_account() {
        let mut observed_account: ObservedAccount = get_observed_account_with_data();
        observed_account.other_account_data_paths = vec![OtherAccountDataPath {
            account: "daily".to_string(),
            data_path: PathBuf::from("/tmp/xost/gimmick/chrome-profile"),
        }];

        let plan: DeregisterPlan = get_plan(observed_account, DataHandling::Delete);

        let expected_data_steps: [DeregisterStep; 1] = [DeregisterStep::DeleteDatabaseFile {
            db_path: PathBuf::from("/tmp/xost/gimmick/queue.db"),
        }];
        let expected_kept_data_paths: Vec<KeptDataPath> = vec![
            KeptDataPath {
                path: PathBuf::from("/tmp/xost/gimmick/chrome-profile"),
                reason: KeptReason::SharedWithAccount("daily".to_string()),
            },
            KeptDataPath {
                path: PathBuf::from("/tmp/xost/gimmick"),
                reason: KeptReason::SharedWithAccount("daily".to_string()),
            },
        ];
        assert_eq!(plan.steps[1..], expected_data_steps);
        assert_eq!(plan.kept_data_paths, expected_kept_data_paths);
    }

    #[test]
    fn build_plan_is_empty_when_nothing_of_the_account_exists() {
        let observed_account: ObservedAccount = ObservedAccount {
            installed_plist_path: None,
            secrets_store: Some(ObservedSecretsStore {
                store_path: PathBuf::from("/tmp/secrets.yaml"),
                stored_secret_names: vec!["daily_password".to_string()],
            }),
            account_section: None,
            other_account_data_paths: Vec::new(),
        };

        let plan: DeregisterPlan = get_plan(observed_account, DataHandling::Delete);

        assert!(plan.steps.is_empty());
    }

    #[test]
    fn parse_confirmation_accepts_y_or_yes_in_any_case() {
        assert_eq!(parse_confirmation("Y"), Confirmation::Confirmed);
        assert_eq!(parse_confirmation(" yEs "), Confirmation::Confirmed);
    }

    #[test]
    fn parse_confirmation_declines_any_other_answer() {
        assert_eq!(parse_confirmation(""), Confirmation::Declined);
        assert_eq!(parse_confirmation("yess"), Confirmation::Declined);
    }

    #[test]
    fn describe_partial_failure_lists_what_was_and_was_not_removed() {
        let steps: Vec<DeregisterStep> = vec![
            DeregisterStep::DeleteDatabaseFile {
                db_path: PathBuf::from("/tmp/queue.db"),
            },
            DeregisterStep::DeleteProfileDirectory {
                profile_dir: PathBuf::from("/tmp/profile"),
            },
            DeregisterStep::DeleteDirectoryIfEmpty {
                directory: PathBuf::from("/tmp"),
            },
        ];

        let message: String = describe_partial_failure(&steps, 1, &AppError::new("permission denied"));

        let expected: &str = "\
could not remove the Chrome profile directory /tmp/profile: Error: permission denied
Removed: the queue database /tmp/queue.db
Not removed: the Chrome profile directory /tmp/profile, the directory /tmp if it is then empty";
        assert_eq!(message, expected);
    }

    #[test]
    fn get_shared_data_path_detects_a_directory_shared_through_a_symlink() {
        let root: PathBuf = std::env::temp_dir().join(format!("xost-deregister-symlink-{}", std::process::id()));
        let real_profile: PathBuf = root.join("real-profile");
        let linked_profile: PathBuf = root.join("linked-profile");
        std::fs::create_dir_all(&real_profile).unwrap();
        std::os::unix::fs::symlink(&real_profile, &linked_profile).unwrap();

        let other_account_data_paths: Vec<OtherAccountDataPath> = vec![OtherAccountDataPath {
            account: "other".to_string(),
            data_path: linked_profile.clone(),
        }];
        let shared_data_path: Option<KeptDataPath> = get_shared_data_path(&real_profile, &other_account_data_paths);
        std::fs::remove_dir_all(&root).ok();

        assert!(shared_data_path.is_some());
    }
}
