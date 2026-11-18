use jiff::{tz::Offset, Timestamp};

/// Parse an instant using the same syntax as timestamp fields deserialized by Serde.
pub fn parse_timestamp(text: &str) -> Option<Timestamp> {
    text.parse().ok()
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
    fn timestamps_use_jiff_input_syntax_and_keep_utc_precision() {
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
            assert_eq!(format_utc(parse_timestamp(input).unwrap()), expected);
        }
        assert_eq!(
            unix_seconds(parse_timestamp("1969-12-31T23:59:59.5Z").unwrap()),
            -1
        );
        for input in ["2026-01-01T00:00Z", "2026-01-01T00:00:00Z[UTC]"] {
            assert_eq!(
                parse_timestamp(input).unwrap(),
                serde_json::from_value::<Timestamp>(serde_json::json!(input)).unwrap()
            );
        }
        for invalid in [
            "2026-01-01T00:00:00.1234567891Z",
            "2026-01-01X00:00:00Z",
            "2026-01-01T00:00:00",
            "2026-01-01",
        ] {
            assert!(parse_timestamp(invalid).is_none());
        }
    }
}
