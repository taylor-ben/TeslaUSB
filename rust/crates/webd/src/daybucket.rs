//! Helpers for timezone-aware day bucketing on read endpoints.

use jiff::civil::Date;
use jiff::tz::TimeZone;
use jiff::{Timestamp, ToSpan};

use crate::error::ApiError;

const INVALID_DAY_MESSAGE: &str = "day must match YYYY-MM-DD";
const INVALID_TIMEZONE_MESSAGE: &str = "unknown or invalid timezone";

/// Parse an API `tz` query parameter into a [`TimeZone`].
pub(crate) fn parse_tz(tz: &str) -> Result<TimeZone, ApiError> {
    if tz.trim().is_empty() || tz.chars().any(char::is_whitespace) || tz.contains('\0') {
        return Err(ApiError::bad_request(
            "invalid_timezone",
            INVALID_TIMEZONE_MESSAGE,
        ));
    }
    if tz == "UTC" {
        return Ok(TimeZone::UTC);
    }
    TimeZone::get(tz)
        .map_err(|_| ApiError::bad_request("invalid_timezone", INVALID_TIMEZONE_MESSAGE))
}

/// Render the local civil day (`YYYY-MM-DD`) for a UTC epoch second.
pub(crate) fn civil_day(epoch_secs: i64, tz: &TimeZone) -> String {
    let ts = Timestamp::from_second(epoch_secs).unwrap_or(Timestamp::UNIX_EPOCH);
    ts.to_zoned(tz.clone()).date().to_string()
}

/// Compute the UTC epoch-second range `[start, end)` for a local civil day.
pub(crate) fn local_day_bounds(day: &str, tz: &TimeZone) -> Result<(i64, i64), ApiError> {
    let date: Date = day
        .parse()
        .map_err(|_| ApiError::bad_request("invalid_day", INVALID_DAY_MESSAGE))?;
    let next = date
        .checked_add(1.days())
        .map_err(|_| ApiError::bad_request("invalid_day", INVALID_DAY_MESSAGE))?;
    let start = date
        .at(0, 0, 0, 0)
        .to_zoned(tz.clone())
        .map_err(|_| ApiError::bad_request("invalid_day", INVALID_DAY_MESSAGE))?;
    let end = next
        .at(0, 0, 0, 0)
        .to_zoned(tz.clone())
        .map_err(|_| ApiError::bad_request("invalid_day", INVALID_DAY_MESSAGE))?;
    Ok((start.timestamp().as_second(), end.timestamp().as_second()))
}

#[cfg(test)]
mod tests {
    use super::{civil_day, local_day_bounds, parse_tz};
    use crate::error::ApiError;
    use jiff::Timestamp;

    fn epoch(instant: &str) -> i64 {
        instant.parse::<Timestamp>().unwrap().as_second()
    }

    #[test]
    fn civil_day_buckets_in_requested_timezone() {
        let utc = parse_tz("UTC").unwrap();
        let ny = parse_tz("America/New_York").unwrap();
        let when = epoch("2026-07-04T01:00:00Z");

        assert_eq!(civil_day(when, &utc), "2026-07-04");
        assert_eq!(civil_day(when, &ny), "2026-07-03");
    }

    #[test]
    fn local_day_bounds_apply_dst_offset_for_summer_and_winter_days() {
        let ny = parse_tz("America/New_York").unwrap();

        let summer = local_day_bounds("2026-07-03", &ny).unwrap();
        assert_eq!(
            summer,
            (epoch("2026-07-03T04:00:00Z"), epoch("2026-07-04T04:00:00Z"))
        );

        let winter = local_day_bounds("2026-01-15", &ny).unwrap();
        assert_eq!(
            winter,
            (epoch("2026-01-15T05:00:00Z"), epoch("2026-01-16T05:00:00Z"))
        );
    }

    #[test]
    fn local_day_bounds_handles_dst_short_and_long_days() {
        let ny = parse_tz("America/New_York").unwrap();

        let spring = local_day_bounds("2026-03-08", &ny).unwrap();
        assert_eq!(spring.1 - spring.0, 23 * 3_600);

        let fall = local_day_bounds("2026-11-01", &ny).unwrap();
        assert_eq!(fall.1 - fall.0, 25 * 3_600);
    }

    #[test]
    fn parse_tz_accepts_utc_and_valid_names_and_rejects_invalid() {
        assert!(parse_tz("UTC").is_ok());
        assert!(parse_tz("America/New_York").is_ok());
        assert!(parse_tz("").is_err());
        assert!(parse_tz("Not/AZone").is_err());
        assert!(matches!(
            parse_tz("Not/AZone"),
            Err(ApiError::BadRequest { code, .. }) if code == "invalid_timezone"
        ));
    }
}
