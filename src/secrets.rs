use std::path::Path;

use base64::Engine;

use crate::config::Config;
use crate::error::AppError;
use crate::private_file;

const SECR_KEY_LENGTH_BYTES: usize = 32;

pub fn password_secret_name(account: &str) -> String {
    format!("{}_password", account)
}

pub fn totp_seed_secret_name(account: &str) -> String {
    format!("{}_totp_seed", account)
}

pub fn get_required(config: &Config, name: &str) -> Result<String, AppError> {
    get_optional(config, name)?.ok_or_else(|| AppError::new(&format!("required secret '{}' not found in secr store", name)))
}

pub fn get_optional(config: &Config, name: &str) -> Result<Option<String>, AppError> {
    let store: secr::store::SecretStore = secr::load::load_secrets_from_file(&config.secr_store_path)?;

    let present: bool = secr::store::list_secret_names(&store).iter().any(|candidate| candidate == name);
    if !present {
        return Ok(None);
    }

    let key: Vec<u8> = decode_key(&config.secr_key_base64)?;

    let plaintext: Vec<u8> = secr::cryptography::decrypt(&store, &key, name)?;
    let value: String = String::from_utf8(plaintext)?;

    Ok(Some(value))
}

pub fn decode_key(key_base64: &str) -> Result<Vec<u8>, AppError> {
    let key: Vec<u8> = secr::BASE64
        .decode(key_base64.as_bytes())
        .map_err(|error| AppError::new(&format!("the secr key in config is not valid base64: {}", error)))?;

    // secr panics on a key of any other length.
    if key.len() != SECR_KEY_LENGTH_BYTES {
        return Err(AppError::new(&format!(
            "the secr key in config decodes to {} bytes, not {}",
            key.len(),
            SECR_KEY_LENGTH_BYTES,
        )));
    }

    Ok(key)
}

pub fn generate_key_base64() -> String {
    secr::BASE64.encode(secr::cryptography::generate_key())
}

#[derive(Debug, PartialEq)]
pub enum StoreContents {
    MissingOrBlank,
    HasContent,
}

#[derive(Debug, PartialEq)]
enum EntryPosition {
    InsideRemovedEntry,
    OutsideRemovedEntry,
}

pub fn read_store_contents(store_path: &Path) -> Result<StoreContents, AppError> {
    let existing_store: String = private_file::read_existing_text(store_path)?;

    if existing_store.trim().is_empty() {
        return Ok(StoreContents::MissingOrBlank);
    }

    Ok(StoreContents::HasContent)
}

pub fn read_store(store_path: &Path) -> Result<secr::store::SecretStore, AppError> {
    let store_contents: StoreContents = read_store_contents(store_path)?;

    if store_contents == StoreContents::MissingOrBlank {
        return Ok(secr::store::SecretStore::new());
    }

    let store: secr::store::SecretStore = secr::load::load_secrets_from_file(store_path)?;

    Ok(store)
}

pub fn read_stored_secret_names(store_path: &Path) -> Result<Vec<String>, AppError> {
    let store: secr::store::SecretStore = read_store(store_path)?;

    Ok(secr::store::list_secret_names(&store))
}

pub fn render_store_without_secrets(store_text: &str, removed_secret_names: &[String]) -> String {
    let mut kept_lines: Vec<&str> = Vec::new();
    let mut entry_position: EntryPosition = EntryPosition::OutsideRemovedEntry;

    for line in store_text.split_inclusive('\n') {
        let top_level_entry_name: Option<&str> = get_top_level_entry_name(line);

        if let Some(entry_name) = top_level_entry_name {
            entry_position = match removed_secret_names.iter().any(|removed_name| removed_name == entry_name) {
                true => EntryPosition::InsideRemovedEntry,
                false => EntryPosition::OutsideRemovedEntry,
            };
        }

        let line_is_part_of_an_entry: bool = top_level_entry_name.is_some() || is_indented_content_line(line);

        if entry_position == EntryPosition::OutsideRemovedEntry || !line_is_part_of_an_entry {
            push_without_repeated_blank_lines(&mut kept_lines, line);
        }
    }

    kept_lines.concat()
}

// Removing an entry leaves the blank lines that separated it from its neighbours.
fn push_without_repeated_blank_lines<'a>(kept_lines: &mut Vec<&'a str>, line: &'a str) {
    let line_is_blank: bool = line.trim().is_empty();
    let previous_line_is_blank: bool = kept_lines.last().map_or(true, |previous_line| previous_line.trim().is_empty());

    if line_is_blank && previous_line_is_blank {
        return;
    }

    kept_lines.push(line);
}

fn get_top_level_entry_name(line: &str) -> Option<&str> {
    let starts_an_entry: bool = !line.starts_with([' ', '\t', '#']) && !line.trim().is_empty();

    if !starts_an_entry {
        return None;
    }

    let (raw_name, _) = line.split_once(':')?;
    let name: &str = raw_name.trim().trim_matches(['"', '\'']);

    Some(name)
}

fn is_indented_content_line(line: &str) -> bool {
    line.starts_with([' ', '\t']) && !line.trim().is_empty()
}

/// Fails unless the store file loads with secr and holds exactly `expected_secret_names`.
pub fn verify_store_file(store_path: &Path, expected_secret_names: &[String]) -> Result<(), AppError> {
    let store: secr::store::SecretStore = secr::load::load_secrets_from_file(store_path)?;
    let stored_secret_names: Vec<String> = secr::store::list_secret_names(&store);

    let mut sorted_expected_secret_names: Vec<String> = expected_secret_names.to_vec();
    sorted_expected_secret_names.sort();

    if stored_secret_names != sorted_expected_secret_names {
        return Err(AppError::new(&format!(
            "the rewritten secr store would hold [{}] instead of [{}]",
            stored_secret_names.join(", "),
            sorted_expected_secret_names.join(", "),
        )));
    }

    Ok(())
}

pub fn render_encrypted_entry(key: &Vec<u8>, name: &str, plaintext: &str) -> Result<String, AppError> {
    let secret: secr::secret::SecretBase64 = secr::cryptography::encrypt(key, plaintext.as_bytes())?;

    Ok(format!("{}\n", secret.to_yaml(name)))
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::path::PathBuf;

    fn get_temp_store_path(label: &str) -> PathBuf {
        std::env::temp_dir().join(format!("xost-secrets-test-{}-{}.yaml", std::process::id(), label))
    }

    #[test]
    fn decode_key_rejects_a_key_of_the_wrong_length() {
        let short_key_base64: String = secr::BASE64.encode([0u8; 16]);

        assert!(decode_key(&short_key_base64).is_err());
    }

    #[test]
    fn read_stored_secret_names_treats_a_missing_store_as_empty() {
        let store_path: PathBuf = get_temp_store_path("missing");

        assert_eq!(read_stored_secret_names(&store_path).unwrap(), Vec::<String>::new());
    }

    #[test]
    fn read_stored_secret_names_treats_a_blank_store_as_empty() {
        let store_path: PathBuf = get_temp_store_path("blank");
        std::fs::write(&store_path, "\n").unwrap();

        let names: Result<Vec<String>, AppError> = read_stored_secret_names(&store_path);
        std::fs::remove_file(&store_path).ok();

        assert_eq!(names.unwrap(), Vec::<String>::new());
    }

    #[test]
    fn render_store_without_secrets_keeps_the_other_entries_decryptable_with_the_original_key() {
        let key: Vec<u8> = decode_key(&generate_key_base64()).unwrap();
        let rewritten_store_path: PathBuf = get_temp_store_path("without-rewritten");
        let entries: Vec<String> = vec![
            render_encrypted_entry(&key, "daily_password", "one").unwrap(),
            render_encrypted_entry(&key, "gimmick_password", "two").unwrap(),
            render_encrypted_entry(&key, "gimmick_totp_seed", "three").unwrap(),
        ];
        let removed_secret_names: Vec<String> = vec!["gimmick_password".to_string(), "gimmick_totp_seed".to_string()];

        std::fs::write(&rewritten_store_path, render_store_without_secrets(&entries.join("\n"), &removed_secret_names)).unwrap();
        let rewritten_store: Result<secr::store::SecretStore, AppError> = read_store(&rewritten_store_path);
        std::fs::remove_file(&rewritten_store_path).ok();

        let rewritten_store: secr::store::SecretStore = rewritten_store.unwrap();
        assert_eq!(secr::store::list_secret_names(&rewritten_store), vec!["daily_password".to_string()]);
        assert_eq!(secr::cryptography::decrypt(&rewritten_store, &key, "daily_password").unwrap(), b"one");
    }

    #[test]
    fn render_store_without_secrets_keeps_comments() {
        let store_text: &str = "\
# posting accounts
daily_password:
    nonce: 'AAAA'
    ciphertext: 'BBBB'

gimmick_password:
    nonce: 'CCCC'
    ciphertext: 'DDDD'
";

        let rendered: String = render_store_without_secrets(store_text, &["gimmick_password".to_string()]);

        assert!(rendered.starts_with("# posting accounts\n"));
    }

    #[test]
    fn render_store_without_secrets_leaves_one_blank_line_where_an_entry_was_removed() {
        let store_text: &str = "\
first_password:
    nonce: 'AAAA'
    ciphertext: 'BBBB'

middle_password:
    nonce: 'CCCC'
    ciphertext: 'DDDD'

last_password:
    nonce: 'EEEE'
    ciphertext: 'FFFF'
";

        let rendered: String = render_store_without_secrets(store_text, &["middle_password".to_string()]);

        assert_eq!(
            rendered,
            "first_password:\n    nonce: 'AAAA'\n    ciphertext: 'BBBB'\n\nlast_password:\n    nonce: 'EEEE'\n    ciphertext: 'FFFF'\n",
        );
    }

    #[test]
    fn verify_store_file_rejects_a_store_holding_other_secret_names() {
        let key: Vec<u8> = decode_key(&generate_key_base64()).unwrap();
        let store_path: PathBuf = get_temp_store_path("verify");
        std::fs::write(&store_path, render_encrypted_entry(&key, "daily_password", "one").unwrap()).unwrap();

        let verification: Result<(), AppError> = verify_store_file(&store_path, &[]);
        std::fs::remove_file(&store_path).ok();

        assert!(verification.is_err());
    }

    #[test]
    fn render_encrypted_entry_appends_entries_that_secr_loads_and_decrypts() {
        let key: Vec<u8> = decode_key(&generate_key_base64()).unwrap();
        let store_path: PathBuf = get_temp_store_path("render");
        let first_entry: String = render_encrypted_entry(&key, "first", "one").unwrap();
        let second_entry: String = render_encrypted_entry(&key, "second", "two").unwrap();
        std::fs::write(&store_path, format!("{}\n{}", first_entry, second_entry)).unwrap();

        let store: Result<secr::store::SecretStore, secr::error::Error> = secr::load::load_secrets_from_file(&store_path);
        std::fs::remove_file(&store_path).ok();

        let store: secr::store::SecretStore = store.unwrap();
        assert_eq!(secr::cryptography::decrypt(&store, &key, "first").unwrap(), b"one");
        assert_eq!(secr::cryptography::decrypt(&store, &key, "second").unwrap(), b"two");
    }
}
