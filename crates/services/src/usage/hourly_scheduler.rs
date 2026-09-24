//! Maintains `usage_hourly`: plans each tick from data-derived progress and drives the
//! `UsageHourlyRepository` port. Planning is pure so it is unit-tested without a database.

use chrono::{DateTime, NaiveDate, TimeDelta, Timelike, Utc};

use std::sync::Arc;
use tracing::{error, info, warn};

use super::ports::{DayParity, HourlyProgress, UsageHourlyRepository};

pub const REREAD_HOURS: i64 = 3;
pub const CATCH_UP_DAYS: i64 = 3;
pub const NIGHTLY_PARITY_HOUR: u32 = 3;
pub const TICK_MINUTE: u32 = 5;

/// Floor to the UTC boundary of `unit_secs`; rem_euclid keeps pre-epoch times flooring down.
fn trunc_secs(t: DateTime<Utc>, unit_secs: i64) -> DateTime<Utc> {
    t - TimeDelta::seconds(t.timestamp().rem_euclid(unit_secs))
        - TimeDelta::nanoseconds(i64::from(t.timestamp_subsec_nanos()))
}

pub fn trunc_hour(t: DateTime<Utc>) -> DateTime<Utc> {
    trunc_secs(t, 3600)
}

pub fn trunc_day(t: DateTime<Utc>) -> DateTime<Utc> {
    trunc_secs(t, 86_400)
}

/// One data-derived cursor (spec §5.3). `from` resumes at the next raw hour after the last
/// computed hour (jumping empty spans), never later than the 3-hour re-read boundary.
/// Catch-up windows end on UTC midnight so every historical day ends inside exactly one window.
pub fn plan_window(progress: HourlyProgress, now: DateTime<Utc>) -> (DateTime<Utc>, DateTime<Utc>) {
    let target = trunc_hour(now);
    let reread = target - TimeDelta::hours(REREAD_HOURS);
    let from = progress.next_raw_hour.unwrap_or(target).min(reread);
    let to = if from < reread {
        (trunc_day(from) + TimeDelta::days(CATCH_UP_DAYS)).min(reread)
    } else {
        target
    };
    (from, to)
}

/// UTC days whose parity to check after recomputing [from, to), sorted and deduplicated.
/// The nightly 03:xx check adds yesterday only once it has been recomputed (its end <= `to`),
/// so a tick still catching up far behind never checks a day it has not written yet.
pub fn parity_days(from: DateTime<Utc>, to: DateTime<Utc>, now: DateTime<Utc>) -> Vec<NaiveDate> {
    let horizon = trunc_hour(now);
    let mut days = std::collections::BTreeSet::new();
    // Starts after `from` (trunc_day(from) <= from), so every day_end satisfies from < day_end.
    let mut day_end = trunc_day(from) + TimeDelta::days(1);
    while day_end <= to {
        if day_end + TimeDelta::hours(REREAD_HOURS) <= horizon {
            days.insert((day_end - TimeDelta::days(1)).date_naive());
        }
        day_end += TimeDelta::days(1);
    }
    let yesterday_start = trunc_day(now) - TimeDelta::days(1);
    if now.hour() == NIGHTLY_PARITY_HOUR && yesterday_start + TimeDelta::days(1) <= to {
        days.insert(yesterday_start.date_naive());
    }
    days.into_iter().collect()
}

/// Delay from `now` to the next HH:05:00 UTC (strictly in the future).
pub fn initial_delay(now: DateTime<Utc>) -> std::time::Duration {
    let mut next = trunc_hour(now) + TimeDelta::minutes(TICK_MINUTE as i64);
    if next <= now {
        next += TimeDelta::hours(1);
    }
    // Invariant: next > now by construction above, so the delta is positive.
    (next - now).to_std().expect("next tick is in the future")
}

/// Tick spacing while catching up after deploy (spec §5.2): one 3-day window per minute.
pub const CATCH_UP_TICK_SECS: u64 = 60;

/// Delay until the next regular tick: the next HH:05 UTC when the interval is the default
/// hourly cadence, otherwise the plain interval.
pub fn next_regular_delay(now: DateTime<Utc>, interval_secs: u64) -> std::time::Duration {
    if interval_secs == 3600 {
        initial_delay(now)
    } else {
        std::time::Duration::from_secs(interval_secs)
    }
}

#[derive(Debug)]
pub struct TickOutcome {
    pub from: DateTime<Utc>,
    pub to: DateTime<Utc>,
    pub caught_up: bool,
    pub skipped: bool,
    pub rows_written: u64,
    pub parity: Vec<DayParity>,
}

/// Keeps `usage_hourly` current (spec §5.2). Multi-instance safe: `recompute` takes a
/// transaction-scoped try-lock, so at most one replica writes per tick; losers log `skipped`.
pub struct UsageHourlyScheduler {
    repository: Arc<dyn UsageHourlyRepository>,
    task_handle: tokio::sync::Mutex<Option<tokio::task::JoinHandle<()>>>,
}

impl UsageHourlyScheduler {
    pub fn new(repository: Arc<dyn UsageHourlyRepository>) -> Self {
        Self {
            repository,
            task_handle: tokio::sync::Mutex::new(None),
        }
    }

    /// First tick shortly after start. While catching up (after deploy), ticks every
    /// CATCH_UP_TICK_SECS; once caught up, at HH:05 UTC (outside the :17-:24 and :01 IO burst
    /// windows) every `interval_secs`. 0 disables (test servers drive `run_once` directly).
    pub async fn start(self: Arc<Self>, interval_secs: u64) {
        if interval_secs == 0 {
            info!("usage_hourly scheduler disabled (interval is 0)");
            return;
        }
        let handle = tokio::spawn({
            let scheduler = self.clone();
            async move {
                // Fast catch-up: while behind, tick again after CATCH_UP_TICK_SECS; once caught up
                // (or on error), wait for the next HH:05 on the regular cadence.
                let mut delay = std::time::Duration::from_secs(CATCH_UP_TICK_SECS);
                loop {
                    tokio::time::sleep(delay).await;
                    delay = match scheduler.run_once(Utc::now()).await {
                        Ok(outcome) if !outcome.caught_up => {
                            std::time::Duration::from_secs(CATCH_UP_TICK_SECS)
                        }
                        Ok(_) => next_regular_delay(Utc::now(), interval_secs),
                        Err(e) => {
                            error!(error = %e, "usage_hourly tick failed");
                            next_regular_delay(Utc::now(), interval_secs)
                        }
                    };
                }
            }
        });
        *self.task_handle.lock().await = Some(handle);
        info!(
            "usage_hourly scheduler started with interval: {} seconds",
            interval_secs
        );
    }

    pub async fn shutdown(&self) {
        if let Some(handle) = self.task_handle.lock().await.take() {
            handle.abort();
            info!("usage_hourly scheduler task cancelled");
        }
    }

    /// One pass: plan from progress, recompute, check parity days, log. Public so tests
    /// can drive it deterministically with an explicit clock.
    pub async fn run_once(&self, now: DateTime<Utc>) -> anyhow::Result<TickOutcome> {
        let started = std::time::Instant::now();
        let progress = self.repository.progress().await?;
        let (from, to) = plan_window(progress, now);
        let caught_up = from == trunc_hour(now) - TimeDelta::hours(REREAD_HOURS);

        let Some(report) = self.repository.recompute(from, to, false).await? else {
            info!(%from, %to, skipped = true, "usage_hourly tick");
            return Ok(TickOutcome {
                from,
                to,
                caught_up,
                skipped: true,
                rows_written: 0,
                parity: vec![],
            });
        };

        let mut parity = Vec::new();
        for day in parity_days(from, to, now) {
            let result = self.repository.day_parity(day).await?;
            if result.is_ok() {
                info!(%day, requests = result.raw.request_count, "usage_hourly parity ok");
            } else {
                warn!(
                    %day,
                    raw_requests = result.raw.request_count,
                    aggregate_requests = result.aggregate.request_count,
                    raw_cost = result.raw.total_cost,
                    aggregate_cost = result.aggregate.total_cost,
                    raw_tokens = result.raw.total_tokens,
                    aggregate_tokens = result.aggregate.total_tokens,
                    "usage_hourly parity mismatch"
                );
            }
            parity.push(result);
        }

        info!(
            %from,
            %to,
            rows_written = report.rows_written,
            duration_ms = started.elapsed().as_millis() as u64,
            max_hour = ?progress.max_hour,
            caught_up,
            skipped = false,
            "usage_hourly tick"
        );
        Ok(TickOutcome {
            from,
            to,
            caught_up,
            skipped: false,
            rows_written: report.rows_written,
            parity,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn t(s: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(s).unwrap().with_timezone(&Utc)
    }
    fn p(max_hour: Option<&str>, next_raw_hour: Option<&str>) -> HourlyProgress {
        HourlyProgress {
            max_hour: max_hour.map(t),
            next_raw_hour: next_raw_hour.map(t),
        }
    }
    fn d(s: &str) -> NaiveDate {
        NaiveDate::parse_from_str(s, "%Y-%m-%d").unwrap()
    }

    #[test]
    fn first_run_starts_at_oldest_raw_hour_and_ends_on_a_utc_midnight() {
        let (from, to) = plan_window(
            p(None, Some("2026-05-01T14:00:00Z")),
            t("2026-09-24T10:07:00Z"),
        );
        assert_eq!(from, t("2026-05-01T14:00:00Z"));
        assert_eq!(to, t("2026-05-04T00:00:00Z"));
    }

    #[test]
    fn catch_up_resumes_after_max_hour_and_jumps_empty_spans() {
        // max_hour D2 20:00; next raw row is 10 days later: the window starts there, not at 21:00.
        let (from, to) = plan_window(
            p(Some("2026-05-03T20:00:00Z"), Some("2026-05-13T07:00:00Z")),
            t("2026-09-24T10:07:00Z"),
        );
        assert_eq!(from, t("2026-05-13T07:00:00Z"));
        assert_eq!(to, t("2026-05-16T00:00:00Z"));
    }

    #[test]
    fn catch_up_never_passes_the_reread_boundary() {
        let (from, to) = plan_window(
            p(Some("2026-09-23T10:00:00Z"), Some("2026-09-23T11:00:00Z")),
            t("2026-09-24T10:07:00Z"),
        );
        assert_eq!(from, t("2026-09-23T11:00:00Z"));
        assert_eq!(to, t("2026-09-24T07:00:00Z")); // target (10:00) - 3h
    }

    #[test]
    fn steady_state_rereads_last_three_closed_hours() {
        let (from, to) = plan_window(
            p(Some("2026-09-24T09:00:00Z"), None),
            t("2026-09-24T10:05:00Z"),
        );
        assert_eq!(
            (from, to),
            (t("2026-09-24T07:00:00Z"), t("2026-09-24T10:00:00Z"))
        );
    }

    #[test]
    fn no_traffic_for_many_hours_is_caught_up_not_stalled() {
        // Review Focus 4: trailing empty span must not re-plan [max_hour+1h, ...) forever.
        let (from, to) = plan_window(
            p(Some("2026-09-20T09:00:00Z"), None),
            t("2026-09-24T10:05:00Z"),
        );
        assert_eq!(
            (from, to),
            (t("2026-09-24T07:00:00Z"), t("2026-09-24T10:00:00Z"))
        );
    }

    #[test]
    fn no_raw_data_at_all_plans_the_reread_window() {
        let (from, to) = plan_window(p(None, None), t("2026-09-24T10:05:00Z"));
        assert_eq!(
            (from, to),
            (t("2026-09-24T07:00:00Z"), t("2026-09-24T10:00:00Z"))
        );
    }

    #[test]
    fn outage_resumes_from_last_computed_hour() {
        let (from, to) = plan_window(
            p(Some("2026-09-23T04:00:00Z"), Some("2026-09-23T05:00:00Z")),
            t("2026-09-24T10:05:00Z"),
        );
        assert_eq!(
            (from, to),
            (t("2026-09-23T05:00:00Z"), t("2026-09-24T07:00:00Z"))
        );
    }

    #[test]
    fn window_is_never_empty_or_inverted() {
        let now = t("2026-09-24T10:05:00Z");
        for next in [
            None,
            Some("2020-01-01T00:00:00Z"),
            Some("2026-09-24T06:00:00Z"),
            Some("2026-09-24T07:00:00Z"),
            Some("2026-09-24T09:00:00Z"),
        ] {
            let (from, to) = plan_window(p(Some("2019-01-01T00:00:00Z"), next), now);
            assert!(from < to, "next={next:?}");
            assert!(from <= t("2026-09-24T07:00:00Z"));
            assert!(to <= t("2026-09-24T10:00:00Z"));
        }
    }

    #[test]
    fn parity_days_cover_each_catch_up_day_exactly_once() {
        let now = t("2026-09-24T10:05:00Z");
        // First run window [D0 14:00, D3 00:00): D0, D1, D2 end inside it.
        assert_eq!(
            parity_days(t("2026-05-01T14:00:00Z"), t("2026-05-04T00:00:00Z"), now),
            vec![d("2026-05-01"), d("2026-05-02"), d("2026-05-03")]
        );
        // Next window [D3 00:00, D6 00:00): D3, D4, D5 only; D2 is not repeated.
        assert_eq!(
            parity_days(t("2026-05-04T00:00:00Z"), t("2026-05-07T00:00:00Z"), now),
            vec![d("2026-05-04"), d("2026-05-05"), d("2026-05-06")]
        );
    }

    #[test]
    fn parity_waits_for_the_reread_horizon_and_runs_nightly_at_03() {
        // 01:05: window [22:00, 01:00) contains midnight, but 23:00 is still re-read until 02:05.
        assert!(parity_days(
            t("2026-09-23T22:00:00Z"),
            t("2026-09-24T01:00:00Z"),
            t("2026-09-24T01:05:00Z")
        )
        .is_empty());
        // 03:05 tick: yesterday is checked.
        assert_eq!(
            parity_days(
                t("2026-09-24T00:00:00Z"),
                t("2026-09-24T03:00:00Z"),
                t("2026-09-24T03:05:00Z")
            ),
            vec![d("2026-09-23")]
        );
    }

    #[test]
    fn nightly_parity_skips_yesterday_while_catching_up_far_behind() {
        // 03:05 tick still catching up months back: yesterday is not recomputed yet.
        assert_eq!(
            parity_days(
                t("2026-05-04T00:00:00Z"),
                t("2026-05-07T00:00:00Z"),
                t("2026-09-24T03:05:00Z")
            ),
            vec![d("2026-05-04"), d("2026-05-05"), d("2026-05-06")]
        );
    }

    #[test]
    fn nightly_parity_checks_yesterday_on_the_steady_03_tick() {
        assert_eq!(
            parity_days(
                t("2026-09-24T00:00:00Z"),
                t("2026-09-24T03:00:00Z"),
                t("2026-09-24T03:05:00Z")
            ),
            vec![d("2026-09-23")]
        );
    }

    #[test]
    fn nightly_parity_and_catch_up_day_are_deduplicated() {
        // Outage catch-up at 03:05 ending on the re-read boundary (00:00): the per-day loop
        // and the nightly rule both pick 09-23; it is checked once.
        assert_eq!(
            parity_days(
                t("2026-09-22T05:00:00Z"),
                t("2026-09-24T00:00:00Z"),
                t("2026-09-24T03:05:00Z")
            ),
            vec![d("2026-09-22"), d("2026-09-23")]
        );
    }

    #[test]
    fn non_03_tick_adds_no_nightly_day() {
        // Yesterday is already recomputed (its end 00:00 <= to), but only 03:xx adds it.
        assert!(parity_days(
            t("2026-09-24T01:00:00Z"),
            t("2026-09-24T04:00:00Z"),
            t("2026-09-24T04:05:00Z")
        )
        .is_empty());
    }

    #[test]
    fn trunc_uses_utc_boundaries_before_epoch_and_with_subseconds() {
        let x = t("1969-12-31T23:59:59.999999999Z");
        assert_eq!(trunc_hour(x), t("1969-12-31T23:00:00Z"));
        assert_eq!(trunc_day(x), t("1969-12-31T00:00:00Z"));
        let y = t("2026-09-24T10:07:03.25Z");
        assert_eq!(trunc_hour(y), t("2026-09-24T10:00:00Z"));
        assert_eq!(trunc_day(y), t("2026-09-24T00:00:00Z"));
    }

    #[test]
    fn initial_delay_targets_next_hh05() {
        assert_eq!(initial_delay(t("2026-09-24T10:03:00Z")).as_secs(), 120);
        assert_eq!(initial_delay(t("2026-09-24T10:05:00Z")).as_secs(), 3600);
        assert_eq!(initial_delay(t("2026-09-24T10:30:00Z")).as_secs(), 35 * 60);
    }
    use crate::usage::ports::{DayParity, DayTotals, RecomputeReport, UsageHourlyRepository};
    use std::sync::{Arc, Mutex};

    #[derive(Default)]
    struct FakeRepo {
        progress: Mutex<Option<HourlyProgress>>,
        lock_busy: bool,
        recomputes: Mutex<Vec<(DateTime<Utc>, DateTime<Utc>)>>,
        parity_calls: Mutex<Vec<NaiveDate>>,
    }

    #[async_trait::async_trait]
    impl UsageHourlyRepository for FakeRepo {
        async fn progress(&self) -> anyhow::Result<HourlyProgress> {
            Ok(self.progress.lock().unwrap().expect("progress set"))
        }
        async fn recompute(
            &self,
            from: DateTime<Utc>,
            to: DateTime<Utc>,
            _wait: bool,
        ) -> anyhow::Result<Option<RecomputeReport>> {
            if self.lock_busy {
                return Ok(None);
            }
            self.recomputes.lock().unwrap().push((from, to));
            Ok(Some(RecomputeReport { rows_written: 7 }))
        }
        async fn day_parity(&self, day: NaiveDate) -> anyhow::Result<DayParity> {
            self.parity_calls.lock().unwrap().push(day);
            Ok(DayParity {
                day,
                raw: DayTotals::default(),
                aggregate: DayTotals::default(),
            })
        }
    }

    #[tokio::test]
    async fn tick_recomputes_planned_window_then_checks_parity_days() {
        let repo = Arc::new(FakeRepo::default());
        *repo.progress.lock().unwrap() = Some(p(None, Some("2026-05-01T14:00:00Z")));
        let scheduler = UsageHourlyScheduler::new(repo.clone());
        let outcome = scheduler.run_once(t("2026-09-24T10:05:00Z")).await.unwrap();
        assert!(!outcome.skipped);
        assert_eq!(outcome.rows_written, 7);
        assert_eq!(
            *repo.recomputes.lock().unwrap(),
            vec![(t("2026-05-01T14:00:00Z"), t("2026-05-04T00:00:00Z"))]
        );
        assert_eq!(
            *repo.parity_calls.lock().unwrap(),
            vec![d("2026-05-01"), d("2026-05-02"), d("2026-05-03")]
        );
    }

    #[tokio::test]
    async fn tick_skips_without_parity_when_another_replica_holds_the_lock() {
        let repo = Arc::new(FakeRepo {
            lock_busy: true,
            ..Default::default()
        });
        *repo.progress.lock().unwrap() = Some(p(None, Some("2026-05-01T14:00:00Z")));
        let outcome = UsageHourlyScheduler::new(repo.clone())
            .run_once(t("2026-09-24T03:05:00Z"))
            .await
            .unwrap();
        assert!(outcome.skipped);
        assert!(repo.parity_calls.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn tick_reports_caught_up_only_on_the_reread_window() {
        let repo = Arc::new(FakeRepo::default());
        *repo.progress.lock().unwrap() = Some(p(None, Some("2026-05-01T14:00:00Z")));
        let behind = UsageHourlyScheduler::new(repo.clone())
            .run_once(t("2026-09-24T10:05:00Z"))
            .await
            .unwrap();
        assert!(!behind.caught_up);
        *repo.progress.lock().unwrap() = Some(p(Some("2026-09-24T09:00:00Z"), None));
        let current = UsageHourlyScheduler::new(repo.clone())
            .run_once(t("2026-09-24T10:05:00Z"))
            .await
            .unwrap();
        assert!(current.caught_up);
    }

    #[test]
    fn regular_delay_aligns_hourly_cadence_to_hh05() {
        assert_eq!(
            next_regular_delay(t("2026-09-24T10:30:00Z"), 3600).as_secs(),
            35 * 60
        );
        assert_eq!(
            next_regular_delay(t("2026-09-24T10:30:00Z"), 120).as_secs(),
            120
        );
    }
}
