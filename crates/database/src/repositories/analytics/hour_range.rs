//! Hour normalization for reports served from `usage_hourly` (spec §6.1). A report body that
//! reads the hourly aggregate widens its input range to whole UTC hours and echoes the
//! widened range; exact readers (credit_type raw bodies, revenue density) never call this.

use chrono::{DateTime, TimeDelta, Utc};
use services::usage::trunc_hour;

/// Half-open `[start, end)` widened to whole hours: `[trunc_hour(start), ceil_hour(end))`.
pub(crate) fn hour_range(
    start: DateTime<Utc>,
    end_exclusive: DateTime<Utc>,
) -> (DateTime<Utc>, DateTime<Utc>) {
    (trunc_hour(start), ceil_hour(end_exclusive))
}

fn ceil_hour(t: DateTime<Utc>) -> DateTime<Utc> {
    let floor = trunc_hour(t);
    if floor == t {
        floor
    } else {
        floor + TimeDelta::hours(1)
    }
}

/// Inclusive `[start, end]` (the summary's `created_at <= end`) widened to whole hours and
/// returned half-open: `[trunc_hour(start), trunc_hour(end) + 1h)`. A single instant
/// therefore covers its whole hour and is never empty.
pub(crate) fn hour_range_inclusive(
    start: DateTime<Utc>,
    end_inclusive: DateTime<Utc>,
) -> (DateTime<Utc>, DateTime<Utc>) {
    (
        trunc_hour(start),
        trunc_hour(end_inclusive) + TimeDelta::hours(1),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn t(s: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(s).unwrap().with_timezone(&Utc)
    }

    #[test]
    fn widens_partial_hours_outward() {
        assert_eq!(
            hour_range(t("2026-09-23T10:15:00Z"), t("2026-09-23T12:00:01Z")),
            (t("2026-09-23T10:00:00Z"), t("2026-09-23T13:00:00Z"))
        );
    }

    #[test]
    fn keeps_aligned_bounds() {
        assert_eq!(
            hour_range(t("2026-09-23T10:00:00Z"), t("2026-09-23T12:00:00Z")),
            (t("2026-09-23T10:00:00Z"), t("2026-09-23T12:00:00Z"))
        );
    }

    #[test]
    fn sub_hour_window_covers_its_hour() {
        assert_eq!(
            hour_range(t("2026-09-23T10:15:00Z"), t("2026-09-23T10:15:00.000001Z")),
            (t("2026-09-23T10:00:00Z"), t("2026-09-23T11:00:00Z"))
        );
    }

    #[test]
    fn inclusive_single_instant_covers_its_hour() {
        assert_eq!(
            hour_range_inclusive(t("2026-07-02T00:00:00Z"), t("2026-07-02T00:00:00Z")),
            (t("2026-07-02T00:00:00Z"), t("2026-07-02T01:00:00Z"))
        );
    }

    #[test]
    fn inclusive_end_on_an_hour_keeps_that_whole_hour() {
        // Review Focus 3: `created_at <= 01:00:00` includes the 01:00 hour.
        assert_eq!(
            hour_range_inclusive(t("2026-07-01T10:30:00Z"), t("2026-07-02T01:00:00Z")),
            (t("2026-07-01T10:00:00Z"), t("2026-07-02T02:00:00Z"))
        );
    }
}
