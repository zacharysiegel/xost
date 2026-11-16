use std::path::Path;
use std::path::PathBuf;

use simple_alert::alert::Alert;
use simple_alert::alert::AlertLevel;

use crate::error;
use crate::error::AppError;

pub const ALERT_SERVICE: &str = "xost";
const WHOLE_CONFIG_ALERT_SUBJECT: &str = "every account";

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum AlertDelivery {
    Delivered,
    Failed,
}

pub fn send(account: &str, level: AlertLevel, message: &str) -> AlertDelivery {
    let alert_config_path_lookup: Result<PathBuf, simple_alert::error::AppError> = simple_alert::config::default_config_path();

    match alert_config_path_lookup {
        Ok(alert_config_path) => send_with_config(&alert_config_path, account, level, message),
        Err(error) => {
            print_delivery_failure(&error, &render_alert_message(account, message));

            AlertDelivery::Failed
        }
    }
}

pub fn send_with_config(alert_config_path: &Path, account: &str, level: AlertLevel, message: &str) -> AlertDelivery {
    let alert_message: String = render_alert_message(account, message);
    let send_result: Result<(), simple_alert::error::AppError> = deliver(alert_config_path, level, &alert_message);

    match send_result {
        Ok(()) => AlertDelivery::Delivered,
        Err(error) => {
            print_delivery_failure(&error, &alert_message);

            AlertDelivery::Failed
        }
    }
}

fn deliver(alert_config_path: &Path, level: AlertLevel, alert_message: &str) -> Result<(), simple_alert::error::AppError> {
    let alert: Alert = Alert::new(ALERT_SERVICE, level, alert_message)?;

    simple_alert::send::send_alert(alert_config_path, &alert)
}

pub fn send_unusable_config_alert(requested_account: Option<&str>, config_error: &AppError) -> AlertDelivery {
    let message: String = format!("the config cannot be read or parsed: {}", error::get_error_message(config_error));

    send(get_unusable_config_alert_subject(requested_account), AlertLevel::Error, &message)
}

fn get_unusable_config_alert_subject(requested_account: Option<&str>) -> &str {
    requested_account.unwrap_or(WHOLE_CONFIG_ALERT_SUBJECT)
}

pub fn render_alert_message(account: &str, message: &str) -> String {
    format!("{}: {}", account, message)
}

fn print_delivery_failure(delivery_error: &simple_alert::error::AppError, alert_message: &str) {
    eprintln!("xost: could not send alert");
    eprintln!("  Error: {}", error::get_error_message(delivery_error));
    eprintln!("  Message: {}", alert_message);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn render_alert_message_starts_with_the_account_name() {
        let alert_message: String = render_alert_message("daily", "tweet id 2 was marked failed: boom");

        assert_eq!(alert_message, "daily: tweet id 2 was marked failed: boom");
    }

    #[test]
    fn get_unusable_config_alert_subject_names_the_requested_account() {
        assert_eq!(get_unusable_config_alert_subject(Some("daily")), "daily");
    }

    #[test]
    fn get_unusable_config_alert_subject_names_every_account_without_a_requested_account() {
        assert_eq!(get_unusable_config_alert_subject(None), WHOLE_CONFIG_ALERT_SUBJECT);
    }

    #[test]
    fn send_with_config_reports_a_failed_delivery_without_propagating_the_error() {
        let missing_config_path: PathBuf =
            std::env::temp_dir().join(format!("xost-alert-test-{}", std::process::id())).join("missing-config.toml");

        let delivery: AlertDelivery = send_with_config(&missing_config_path, "daily", AlertLevel::Error, "message");

        assert_eq!(delivery, AlertDelivery::Failed);
    }
}
