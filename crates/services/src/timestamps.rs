use jiff::{tz::Offset, Timestamp};
use time::{format_description::well_known::Rfc3339, OffsetDateTime};

/// Parse a full RFC 3339 instant for API fields.
pub fn parse_rfc3339(text: &str) -> Option<Timestamp> {
    if !matches!(text.as_bytes().get(10), Some(b'T' | b't' | b' ')) {
        return None;
    }
    let instant = OffsetDateTime::parse(text, &Rfc3339).ok()?;
    Timestamp::from_nanosecond(instant.unix_timestamp_nanos()).ok()
}

/// Whole Unix seconds, rounded down for fractional instants before the epoch.
pub fn unix_seconds(timestamp: Timestamp) -> i64 {
    timestamp.as_second() - i64::from(timestamp.subsec_nanosecond() < 0)
}

/// Format string-valued UTC fields with the existing millisecond, microsecond or nanosecond precision.
pub fn format_utc(timestamp: Timestamp) -> String {
    let nanos = timestamp.subsec_nanosecond();
    let digits = match nanos {
        0 => 0,
        n if n % 1_000_000 == 0 => 3,
        n if n % 1_000 == 0 => 6,
        _ => 9,
    };
    format!("{:.digits$}", timestamp.display_with_offset(Offset::UTC))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn timestamps_keep_rfc3339_syntax_and_utc_precision() {
        for (input, expected) in [
            ("2026-01-01T02:00:00+02:00", "2026-01-01T00:00:00+00:00"),
            ("1969-12-31T23:59:59.50Z", "1969-12-31T23:59:59.500+00:00"),
            (
                "2026-01-01T00:00:00.12340Z",
                "2026-01-01T00:00:00.123400+00:00",
            ),
            (
                "2026-01-01T00:00:00.123456780Z",
                "2026-01-01T00:00:00.123456780+00:00",
            ),
        ] {
            assert_eq!(format_utc(parse_rfc3339(input).unwrap()), expected);
        }
        assert_eq!(
            unix_seconds(parse_rfc3339("1969-12-31T23:59:59.5Z").unwrap()),
            -1
        );
        for invalid in [
            "2026-01-01X00:00:00Z",
            "2026-01-01T00:00Z",
            "2026-01-01T00:00:00Z[UTC]",
        ] {
            assert!(parse_rfc3339(invalid).is_none());
        }
    }
}
