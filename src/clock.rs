use time::OffsetDateTime;
use time::UtcOffset;
use time::format_description::well_known::Rfc3339;

use crate::error::AppError;

pub fn now_rfc3339() -> Result<String, AppError> {
    let formatted: String = OffsetDateTime::now_utc().format(&Rfc3339)?;

    Ok(formatted)
}

pub fn falls_on_today_utc(rfc3339: &str) -> Result<bool, AppError> {
    let parsed: OffsetDateTime = OffsetDateTime::parse(rfc3339, &Rfc3339)?;
    let parsed_date: time::Date = parsed.to_offset(UtcOffset::UTC).date();
    let today: time::Date = OffsetDateTime::now_utc().date();

    Ok(parsed_date == today)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn falls_on_today_utc_is_true_for_the_current_timestamp() {
        let now: String = now_rfc3339().unwrap();

        assert!(falls_on_today_utc(&now).unwrap());
    }

    #[test]
    fn falls_on_today_utc_is_false_for_a_past_date() {
        assert!(!falls_on_today_utc("2000-01-01T00:00:00Z").unwrap());
    }
}
