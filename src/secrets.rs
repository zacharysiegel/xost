use std::path::Path;

use base64::Engine;

use crate::config::Config;
use crate::error::AppError;

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

pub fn read_stored_secret_names(store_path: &Path) -> Result<Vec<String>, AppError> {
    let contents: String = match std::fs::read_to_string(store_path) {
        Ok(contents) => contents,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(error.into()),
    };

    if contents.trim().is_empty() {
        return Ok(Vec::new());
    }

    let store: secr::store::SecretStore = secr::load::load_secrets_from_file(store_path)?;

    Ok(secr::store::list_secret_names(&store))
}

pub fn render_encrypted_entry(key: &Vec<u8>, name: &str, plaintext: &str) -> Result<String, AppError> {
    let secret: secr::secret::SecretBase64 = secr::cryptography::encrypt(key, plaintext.as_bytes())?;

    Ok(format!("{}\n", secret.to_yaml(name)))
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::path::PathBuf;

    fn temp_store_path(label: &str) -> PathBuf {
        std::env::temp_dir().join(format!("xost-secrets-test-{}-{}.yaml", std::process::id(), label))
    }

    #[test]
    fn decode_key_rejects_a_key_of_the_wrong_length() {
        let short_key_base64: String = secr::BASE64.encode([0u8; 16]);

        assert!(decode_key(&short_key_base64).is_err());
    }

    #[test]
    fn read_stored_secret_names_treats_a_missing_store_as_empty() {
        let store_path: PathBuf = temp_store_path("missing");

        assert_eq!(read_stored_secret_names(&store_path).unwrap(), Vec::<String>::new());
    }

    #[test]
    fn read_stored_secret_names_treats_a_blank_store_as_empty() {
        let store_path: PathBuf = temp_store_path("blank");
        std::fs::write(&store_path, "\n").unwrap();

        let names: Result<Vec<String>, AppError> = read_stored_secret_names(&store_path);
        std::fs::remove_file(&store_path).ok();

        assert_eq!(names.unwrap(), Vec::<String>::new());
    }

    #[test]
    fn render_encrypted_entry_appends_entries_that_secr_loads_and_decrypts() {
        let key: Vec<u8> = decode_key(&generate_key_base64()).unwrap();
        let store_path: PathBuf = temp_store_path("render");
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
