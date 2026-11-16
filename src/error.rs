const APP_ERROR_MESSAGE_PREFIX: &str = "Error: ";

minimer::define_app_error!(pub AppError);

minimer::impl_from_error!(AppError, rusqlite::Error);
minimer::impl_from_error!(AppError, chromiumoxide::error::CdpError);
minimer::impl_from_error!(AppError, serde_json::Error);
minimer::impl_from_error!(AppError, secr::error::Error);
minimer::impl_from_error!(AppError, totp_rs::SecretParseError);
minimer::impl_from_error!(AppError, totp_rs::TotpError);
minimer::impl_from_error!(AppError, jiff::Error);
minimer::impl_from_error!(AppError, simple_alert::error::AppError);

pub fn get_error_message(error: &minimer::AppError) -> String {
    let message: &str = error.message.strip_prefix(APP_ERROR_MESSAGE_PREFIX).unwrap_or(&error.message);

    message.to_string()
}
