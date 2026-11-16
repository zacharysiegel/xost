use jiff::Timestamp;
#[cfg(test)]
use jiff::ZonedDateTime;
use jiff::civil::Date;
use jiff::tz::TimeZone;

use crate::error::AppError;

pub fn now_rfc3339() -> String {
    Timestamp::now().to_string()
}

pub fn parse_rfc3339(rfc3339: &str) -> Result<Timestamp, AppError> {
    let parsed: Timestamp = rfc3339.parse::<Timestamp>()?;

    Ok(parsed)
}

pub fn falls_on_today_utc(rfc3339: &str) -> Result<bool, AppError> {
    let parsed_date: Date = TimeZone::UTC.to_datetime(parse_rfc3339(rfc3339)?).date();
    let today: Date = TimeZone::UTC.to_datetime(Timestamp::now()).date();

    Ok(parsed_date == today)
}

#[cfg(test)]
pub(crate) fn get_utc_time(rfc3339: &str) -> ZonedDateTime {
    parse_rfc3339(rfc3339).unwrap().to_zoned_datetime(TimeZone::UTC)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn falls_on_today_utc_is_true_for_the_current_timestamp() {
        let now: String = now_rfc3339();

        assert!(falls_on_today_utc(&now).unwrap());
    }

    #[test]
    fn falls_on_today_utc_is_false_for_a_past_date() {
        assert!(!falls_on_today_utc("2000-01-01T00:00:00Z").unwrap());
    }
}
