use base64::Engine;

use crate::config::Config;
use crate::error::AppError;

pub fn get_required(config: &Config, name: &str) -> Result<String, AppError> {
    get_optional(config, name)?.ok_or_else(|| AppError::new(&format!("required secret '{}' not found in secr store", name)))
}

pub fn get_optional(config: &Config, name: &str) -> Result<Option<String>, AppError> {
    let store: secr::store::SecretStore = secr::load::load_secrets_from_file(&config.secr_store_path)?;

    let present: bool = secr::store::list_secret_names(&store).iter().any(|candidate| candidate == name);
    if !present {
        return Ok(None);
    }

    let key: Vec<u8> = secr::BASE64
        .decode(config.secr_key_base64.as_bytes())
        .map_err(|error| AppError::new(&format!("the secr key in config is not valid base64: {}", error)))?;

    let plaintext: Vec<u8> = secr::cryptography::decrypt(&store, &key, name)?;
    let value: String = String::from_utf8(plaintext)?;

    Ok(Some(value))
}
