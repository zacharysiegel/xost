use std::collections::BTreeSet;

use jiff::Timestamp;
use jiff::ZonedDateTime;
use jiff::civil::DateTime;
use jiff::tz::TimeZone;

use crate::error::AppError;

const MINUTE: FieldSpecification = FieldSpecification {
    name: "minute",
    minimum: 0,
    maximum: 59,
};
const HOUR: FieldSpecification = FieldSpecification {
    name: "hour",
    minimum: 0,
    maximum: 23,
};
const DAY_OF_MONTH: FieldSpecification = FieldSpecification {
    name: "day-of-month",
    minimum: 1,
    maximum: 31,
};
const MONTH: FieldSpecification = FieldSpecification {
    name: "month",
    minimum: 1,
    maximum: 12,
};
const DAY_OF_WEEK: FieldSpecification = FieldSpecification {
    name: "day-of-week",
    minimum: 0,
    maximum: 7,
};
const SUNDAY: u8 = 0;
const SUNDAY_ALTERNATE: u8 = 7;
const SECONDS_PER_MINUTE: i64 = 60;
const NEXT_RUN_SEARCH_HORIZON_MINUTES: i64 = 366 * 24 * 60;

#[derive(Debug, PartialEq)]
pub enum CronField {
    Every,
    /// Text beginning with '*' other than a lone '*', such as */2.
    OnlyFromWildcard(BTreeSet<u8>),
    Only(BTreeSet<u8>),
}

impl CronField {
    pub fn restricted_values(&self) -> Option<&BTreeSet<u8>> {
        match self {
            CronField::Every => None,
            CronField::OnlyFromWildcard(values) | CronField::Only(values) => Some(values),
        }
    }

    fn matches(&self, value: i8) -> bool {
        let Some(values) = self.restricted_values() else {
            return true;
        };

        u8::try_from(value).is_ok_and(|value| values.contains(&value))
    }
}

pub enum DayRestriction {
    EveryDay,
    DayOfMonthOnly,
    DayOfWeekOnly,
    EitherDayField,
}

#[derive(Debug, PartialEq)]
pub enum NextRun {
    At(ZonedDateTime),
    NoneWithinHorizon,
}

#[derive(Debug, PartialEq)]
pub struct CronSchedule {
    pub minute: CronField,
    pub hour: CronField,
    pub day_of_month: CronField,
    pub month: CronField,
    /// 7 is stored as 0.
    pub day_of_week: CronField,
}

impl CronSchedule {
    pub fn parse(expression: &str) -> Result<CronSchedule, AppError> {
        let is_macro: bool = expression.trim_start().starts_with('@');

        if is_macro {
            return Err(AppError::new(&format!(
                "schedule '{}' is a macro, which is not supported; write five fields: minute hour day-of-month month day-of-week",
                expression,
            )));
        }

        let raw_fields: Vec<&str> = expression.split_whitespace().collect();

        let [minute, hour, day_of_month, month, day_of_week]: &[&str] = raw_fields.as_slice() else {
            return Err(AppError::new(&format!(
                "schedule '{}' has {} fields; expected five: minute hour day-of-month month day-of-week",
                expression,
                raw_fields.len(),
            )));
        };

        let schedule: CronSchedule = CronSchedule {
            minute: parse_field(minute, &MINUTE)?,
            hour: parse_field(hour, &HOUR)?,
            day_of_month: parse_field(day_of_month, &DAY_OF_MONTH)?,
            month: parse_field(month, &MONTH)?,
            day_of_week: normalize_sunday(parse_field(day_of_week, &DAY_OF_WEEK)?),
        };

        validate_day_fields_expressible_in_launchd(&schedule, expression)?;

        Ok(schedule)
    }

    pub fn day_restriction(&self) -> DayRestriction {
        match (self.day_of_month.restricted_values(), self.day_of_week.restricted_values()) {
            (None, None) => DayRestriction::EveryDay,
            (Some(_), None) => DayRestriction::DayOfMonthOnly,
            (None, Some(_)) => DayRestriction::DayOfWeekOnly,
            (Some(_), Some(_)) => DayRestriction::EitherDayField,
        }
    }

    pub fn matches(&self, local_time: DateTime) -> bool {
        self.minute.matches(local_time.minute())
            && self.hour.matches(local_time.hour())
            && self.month.matches(local_time.month())
            && self.matches_day(local_time)
    }

    fn matches_day(&self, local_time: DateTime) -> bool {
        let day_of_month_matches: bool = self.day_of_month.matches(local_time.day());
        let day_of_week_matches: bool = self.day_of_week.matches(local_time.weekday().to_sunday_zero_offset());

        match self.day_restriction() {
            DayRestriction::EveryDay => true,
            DayRestriction::DayOfMonthOnly => day_of_month_matches,
            DayRestriction::DayOfWeekOnly => day_of_week_matches,
            DayRestriction::EitherDayField => day_of_month_matches || day_of_week_matches,
        }
    }
}

/* launchd fires a calendar interval whose Day and Weekday are both set when either matches. Vixie cron instead
requires both day fields to match when either field's text begins with '*' and both are restricted. */
fn validate_day_fields_expressible_in_launchd(schedule: &CronSchedule, expression: &str) -> Result<(), AppError> {
    let day_fields_both_restricted: bool = matches!(schedule.day_restriction(), DayRestriction::EitherDayField);
    let day_field_begins_with_wildcard: bool = matches!(schedule.day_of_month, CronField::OnlyFromWildcard(_))
        || matches!(schedule.day_of_week, CronField::OnlyFromWildcard(_));

    if day_fields_both_restricted && day_field_begins_with_wildcard {
        return Err(AppError::new(&format!(
            "schedule '{}' restricts both day-of-month and day-of-week with one of them beginning with '*', \
             which cron runs only on days matching both fields; launchd runs a job on days matching either field, \
             so it cannot run every Nth day only when it is also a given weekday. \
             Restrict one day field, or write both without '*' to run on days matching either",
            expression,
        )));
    }

    Ok(())
}

/// The first minute strictly after `after` whose wall-clock reading in `after`'s time zone matches the schedule.
pub fn get_next_run_after(schedule: &CronSchedule, after: &ZonedDateTime) -> NextRun {
    let time_zone: &TimeZone = after.time_zone();
    let after_minute_start_second: i64 = after.timestamp().as_second().div_euclid(SECONDS_PER_MINUTE) * SECONDS_PER_MINUTE;

    for minute_offset in 1..=NEXT_RUN_SEARCH_HORIZON_MINUTES {
        let candidate_lookup: Result<Timestamp, jiff::Error> =
            Timestamp::from_second(after_minute_start_second + minute_offset * SECONDS_PER_MINUTE);

        let Ok(candidate) = candidate_lookup else {
            return NextRun::NoneWithinHorizon;
        };

        let candidate_local_time: DateTime = time_zone.to_datetime(candidate);

        if schedule.matches(candidate_local_time) {
            return NextRun::At(candidate.to_zoned_datetime(time_zone.clone()));
        }
    }

    NextRun::NoneWithinHorizon
}

struct FieldSpecification {
    name: &'static str,
    minimum: u8,
    maximum: u8,
}

enum TermRange {
    Wildcard,
    Span { first: u8, last: u8 },
    Single(u8),
}

fn parse_field(raw: &str, specification: &FieldSpecification) -> Result<CronField, AppError> {
    if raw == "*" {
        return Ok(CronField::Every);
    }

    let mut values: BTreeSet<u8> = BTreeSet::new();

    for term in raw.split(',') {
        let term_values: Vec<u8> = parse_term(term, specification).map_err(|reason| {
            AppError::new(&format!(
                "schedule {} field '{}' is invalid: {}",
                specification.name, raw, reason,
            ))
        })?;

        values.extend(term_values);
    }

    if raw.starts_with('*') {
        return Ok(CronField::OnlyFromWildcard(values));
    }

    Ok(CronField::Only(values))
}

fn parse_term(term: &str, specification: &FieldSpecification) -> Result<Vec<u8>, String> {
    let (range_text, step_text): (&str, Option<&str>) = match term.split_once('/') {
        Some((range_text, step_text)) => (range_text, Some(step_text)),
        None => (term, None),
    };

    let range: TermRange = parse_term_range(range_text, specification)?;

    let step: u8 = match step_text {
        Some(text) => parse_step(text)?,
        None => 1,
    };

    let (first, last): (u8, u8) = match range {
        TermRange::Wildcard => (specification.minimum, specification.maximum),
        TermRange::Span { first, last } => (first, last),
        TermRange::Single(value) if step_text.is_none() => (value, value),
        TermRange::Single(_) => return Err(format!("the step in '{}' needs '*' or a range before it", term)),
    };

    let term_values: Vec<u8> = (first..=last).step_by(usize::from(step)).collect();

    Ok(term_values)
}

fn parse_term_range(range_text: &str, specification: &FieldSpecification) -> Result<TermRange, String> {
    if range_text == "*" {
        return Ok(TermRange::Wildcard);
    }

    let Some((first_text, last_text)) = range_text.split_once('-') else {
        let value: u8 = parse_value(range_text, specification)?;

        return Ok(TermRange::Single(value));
    };

    let first: u8 = parse_value(first_text, specification)?;
    let last: u8 = parse_value(last_text, specification)?;

    if first > last {
        return Err(format!("the range '{}' runs backwards", range_text));
    }

    Ok(TermRange::Span { first, last })
}

fn parse_value(text: &str, specification: &FieldSpecification) -> Result<u8, String> {
    let out_of_range_reason: String = format!(
        "'{}' is not a number from {} to {}",
        text, specification.minimum, specification.maximum,
    );

    if !is_unsigned_integer(text) {
        return Err(out_of_range_reason);
    }

    let value: u8 = text.parse::<u8>().map_err(|_| out_of_range_reason.clone())?;

    if value < specification.minimum || value > specification.maximum {
        return Err(out_of_range_reason);
    }

    Ok(value)
}

fn parse_step(text: &str) -> Result<u8, String> {
    let step: Option<u8> = text.parse::<u8>().ok().filter(|step| is_unsigned_integer(text) && *step > 0);

    step.ok_or_else(|| format!("the step '{}' is not a positive number", text))
}

fn is_unsigned_integer(text: &str) -> bool {
    !text.is_empty() && text.chars().all(|character| character.is_ascii_digit())
}

fn normalize_sunday(day_of_week: CronField) -> CronField {
    match day_of_week {
        CronField::Every => CronField::Every,
        CronField::OnlyFromWildcard(values) => CronField::OnlyFromWildcard(with_sunday_as_0(values)),
        CronField::Only(values) => CronField::Only(with_sunday_as_0(values)),
    }
}

fn with_sunday_as_0(mut values: BTreeSet<u8>) -> BTreeSet<u8> {
    let had_alternate_sunday: bool = values.remove(&SUNDAY_ALTERNATE);

    if had_alternate_sunday {
        values.insert(SUNDAY);
    }

    values
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::clock;

    fn get_field_matching_only(values: &[u8]) -> CronField {
        CronField::Only(values.iter().copied().collect())
    }

    fn get_next_run_for(expression: &str, after_rfc3339: &str) -> NextRun {
        get_next_run_after(&CronSchedule::parse(expression).unwrap(), &clock::get_utc_time(after_rfc3339))
    }

    #[test]
    fn get_next_run_after_is_later_today_for_a_daily_time_not_yet_reached() {
        let next_run: NextRun = get_next_run_for("0 9 * * *", "2026-11-15T08:30:00Z");

        assert_eq!(next_run, NextRun::At(clock::get_utc_time("2026-11-15T09:00:00Z")));
    }

    #[test]
    fn get_next_run_after_is_tomorrow_for_a_daily_time_reached_this_minute() {
        let next_run: NextRun = get_next_run_for("0 9 * * *", "2026-11-15T09:00:00Z");

        assert_eq!(next_run, NextRun::At(clock::get_utc_time("2026-11-16T09:00:00Z")));
    }

    #[test]
    fn get_next_run_after_waits_for_the_restricted_day_of_week() {
        let next_run: NextRun = get_next_run_for("0 9 * * 3", "2026-11-15T10:00:00Z");

        assert_eq!(next_run, NextRun::At(clock::get_utc_time("2026-11-18T09:00:00Z")));
    }

    #[test]
    fn get_next_run_after_crosses_a_month_boundary() {
        let next_run: NextRun = get_next_run_for("0 0 1 * *", "2026-11-30T12:00:00Z");

        assert_eq!(next_run, NextRun::At(clock::get_utc_time("2026-12-01T00:00:00Z")));
    }

    #[test]
    fn get_next_run_after_matches_the_day_of_week_first_when_both_day_fields_are_restricted() {
        let next_run: NextRun = get_next_run_for("0 9 20 * 1", "2026-11-15T10:00:00Z");

        assert_eq!(next_run, NextRun::At(clock::get_utc_time("2026-11-16T09:00:00Z")));
    }

    #[test]
    fn get_next_run_after_matches_the_day_of_month_first_when_both_day_fields_are_restricted() {
        let next_run: NextRun = get_next_run_for("0 9 16 * 5", "2026-11-15T10:00:00Z");

        assert_eq!(next_run, NextRun::At(clock::get_utc_time("2026-11-16T09:00:00Z")));
    }

    #[test]
    fn get_next_run_after_finds_none_within_the_horizon_for_a_date_that_never_occurs() {
        let next_run: NextRun = get_next_run_for("0 0 31 2 *", "2026-11-15T10:00:00Z");

        assert_eq!(next_run, NextRun::NoneWithinHorizon);
    }

    #[test]
    fn parse_keeps_a_lone_wildcard_unrestricted() {
        let schedule: CronSchedule = CronSchedule::parse("0 9 * * *").unwrap();

        assert_eq!(schedule.day_of_month, CronField::Every);
    }

    #[test]
    fn parse_expands_a_comma_list_of_values_and_ranges() {
        let schedule: CronSchedule = CronSchedule::parse("0 1,5-7,23 * * *").unwrap();

        assert_eq!(schedule.hour, get_field_matching_only(&[1, 5, 6, 7, 23]));
    }

    #[test]
    fn parse_expands_a_step_over_the_wildcard_and_records_that_it_began_with_the_wildcard() {
        let schedule: CronSchedule = CronSchedule::parse("*/20 * * * *").unwrap();

        assert_eq!(schedule.minute, CronField::OnlyFromWildcard([0, 20, 40].into_iter().collect()));
    }

    #[test]
    fn parse_expands_a_step_over_a_range() {
        let schedule: CronSchedule = CronSchedule::parse("0 0 1-10/4 * *").unwrap();

        assert_eq!(schedule.day_of_month, get_field_matching_only(&[1, 5, 9]));
    }

    #[test]
    fn parse_stores_day_of_week_7_as_sunday_0() {
        let schedule: CronSchedule = CronSchedule::parse("0 9 * * 5-7").unwrap();

        assert_eq!(schedule.day_of_week, get_field_matching_only(&[0, 5, 6]));
    }

    #[test]
    fn parse_rejects_a_day_name_naming_the_field() {
        let error: AppError = CronSchedule::parse("0 9 * * MON").unwrap_err();

        assert!(error.to_string().contains("day-of-week"));
    }

    #[test]
    fn parse_rejects_a_value_outside_the_field_range() {
        assert!(CronSchedule::parse("60 9 * * *").is_err());
    }

    #[test]
    fn parse_rejects_a_macro() {
        assert!(CronSchedule::parse("@daily").is_err());
    }

    #[test]
    fn parse_rejects_six_fields() {
        assert!(CronSchedule::parse("0 0 9 * * *").is_err());
    }

    #[test]
    fn parse_rejects_a_zero_step() {
        assert!(CronSchedule::parse("*/0 9 * * *").is_err());
    }

    #[test]
    fn parse_rejects_a_step_after_a_single_value() {
        assert!(CronSchedule::parse("5/10 9 * * *").is_err());
    }

    #[test]
    fn parse_rejects_a_stepped_wildcard_day_of_month_alongside_a_restricted_day_of_week() {
        let error: AppError = CronSchedule::parse("0 9 */2 * 1").unwrap_err();

        assert!(error.to_string().contains("launchd"));
    }

    #[test]
    fn parse_rejects_a_backwards_range() {
        assert!(CronSchedule::parse("0 17-9 * * *").is_err());
    }
}
