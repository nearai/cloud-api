//! Maintains `usage_hourly`: plans each tick from data-derived progress and drives the
//! `UsageHourlyRepository` port. Planning is pure so it is unit-tested without a database.

use std::sync::Arc;

use chrono::{DateTime, NaiveDate, TimeDelta, Timelike, Utc};
use tracing::{error, info, warn};

use super::ports::{AggregateLockBehavior, DayParity, HourlyProgress, UsageHourlyRepository};
use crate::metrics::{
    consts::{get_environment, METRIC_USAGE_HOURLY_LAG_SECONDS, TAG_ENVIRONMENT},
    MetricsServiceTrait,
};

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
/// The nightly 03:xx check assumes the default hourly cadence; custom intervals may miss it.
/// It adds yesterday only once it has been recomputed (its end <= `to`), so a tick still catching
/// up far behind never checks a day it has not written yet.
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
    metrics_service: Arc<dyn MetricsServiceTrait>,
    task_handle: tokio::sync::Mutex<Option<tokio::task::JoinHandle<()>>>,
}

impl UsageHourlyScheduler {
    pub fn new(
        repository: Arc<dyn UsageHourlyRepository>,
        metrics_service: Arc<dyn MetricsServiceTrait>,
    ) -> Self {
        Self {
            repository,
            metrics_service,
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
                // Fast catch-up: while behind, retry after CATCH_UP_TICK_SECS, including after a
                // failed tick. Once caught up, wait for the regular cadence.
                let mut delay = std::time::Duration::from_secs(CATCH_UP_TICK_SECS);
                let mut catching_up = true;
                loop {
                    tokio::time::sleep(delay).await;
                    match scheduler.run_once(Utc::now()).await {
                        Ok(outcome) => catching_up = !outcome.caught_up,
                        Err(e) => error!(error = %e, "usage_hourly tick failed"),
                    }
                    delay = if catching_up {
                        std::time::Duration::from_secs(CATCH_UP_TICK_SECS)
                    } else {
                        next_regular_delay(Utc::now(), interval_secs)
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
        // Freshness (spec §9): seconds since the start of the oldest raw hour not yet aggregated
        // (the current hour when none is pending). Hours with no raw usage are not lag, so a
        // quiet span does not look like a stalled job. Recorded on every tick (catch-up,
        // skipped or steady); an alert fires above 3 hours. Nothing to record while the table
        // is still empty.
        if progress.max_hour.is_some() {
            let current_hour = trunc_hour(now);
            let pending_from = progress
                .next_raw_hour
                .map_or(current_hour, |hour| hour.min(current_hour));
            let lag = now - pending_from;
            let env_tag = format!("{TAG_ENVIRONMENT}:{}", get_environment());
            self.metrics_service.record_histogram(
                METRIC_USAGE_HOURLY_LAG_SECONDS,
                lag.as_seconds_f64(),
                &[env_tag.as_str()],
            );
        }
        let (from, to) = plan_window(progress, now);
        let caught_up = from == trunc_hour(now) - TimeDelta::hours(REREAD_HOURS);

        let Some(report) = self
            .repository
            .recompute(from, to, AggregateLockBehavior::SkipIfBusy)
            .await
            .map_err(|error| {
                anyhow::anyhow!("usage_hourly recompute [{from}, {to}) failed: {error:#}")
            })?
        else {
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

/// Largest window one admin repair may recompute.
pub const MAX_REPAIR_DAYS: i64 = 31;

/// Rejects repair windows that are not whole, ordered UTC hours within `MAX_REPAIR_DAYS`.
pub fn validate_repair_window(from: DateTime<Utc>, to: DateTime<Utc>) -> Result<(), String> {
    if trunc_hour(from) != from || trunc_hour(to) != to {
        return Err("start and end must be whole UTC hours".to_string());
    }
    if from >= to {
        return Err("start must be before end".to_string());
    }
    if to - from > TimeDelta::days(MAX_REPAIR_DAYS) {
        return Err(format!("window must not exceed {MAX_REPAIR_DAYS} days"));
    }
    Ok(())
}

/// Latest `from` a repair may start at without leaving uncomputed raw hours below it. Readers
/// and the scheduler treat every hour below `max_hour + 1h` as final, so a repair that starts
/// past the next uncomputed raw hour would move `max_hour` over hours nothing ever computes,
/// and readers would serve them from an empty aggregate. `None` means no raw hour is pending.
pub fn repair_frontier(progress: HourlyProgress) -> Option<DateTime<Utc>> {
    progress.next_raw_hour
}

#[derive(Debug, thiserror::Error)]
pub enum RepairError {
    #[error("{0}")]
    InvalidWindow(String),
    #[error(
        "usage_hourly has not been computed up to {frontier}; start the repair at or before it, \
         or wait for the scheduler to catch up"
    )]
    AheadOfAggregate { frontier: DateTime<Utc> },
    #[error(transparent)]
    Failed(#[from] anyhow::Error),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RepairReport {
    pub rows_written: u64,
    /// Raw vs aggregate totals for every UTC day the window touches, after the recompute.
    pub days: Vec<DayParity>,
}

/// Explicit operator repair for rows that reached raw after their hour left the scheduler's
/// 3-hour re-read (backfill, clock skew, a day flagged by parity): recomputes [from, to) one
/// UTC day per transaction, waiting for the aggregate lock, then reports each touched day's
/// parity. Refuses windows that start past `repair_frontier`.
pub async fn repair(
    repository: &dyn UsageHourlyRepository,
    from: DateTime<Utc>,
    to: DateTime<Utc>,
) -> Result<RepairReport, RepairError> {
    validate_repair_window(from, to).map_err(RepairError::InvalidWindow)?;
    // The scheduler only moves the frontier forward, so a check outside the lock stays valid.
    if let Some(frontier) = repair_frontier(repository.progress().await?) {
        if from > frontier {
            return Err(RepairError::AheadOfAggregate { frontier });
        }
    }
    let mut rows_written = 0;
    let mut chunk_start = from;
    while chunk_start < to {
        let chunk_end = (trunc_day(chunk_start) + TimeDelta::days(1)).min(to);
        let report = repository
            .recompute(chunk_start, chunk_end, AggregateLockBehavior::Wait)
            .await?
            .ok_or_else(|| anyhow::anyhow!("usage_hourly repair: Wait returned no recompute"))?;
        rows_written += report.rows_written;
        chunk_start = chunk_end;
    }
    let mut days = Vec::new();
    let mut day = trunc_day(from);
    while day < to {
        days.push(repository.day_parity(day.date_naive()).await?);
        day += TimeDelta::days(1);
    }
    info!(
        from = %from,
        to = %to,
        rows_written,
        days = days.len(),
        mismatched_days = days.iter().filter(|p| !p.is_ok()).count(),
        "usage_hourly repair"
    );
    Ok(RepairReport { rows_written, days })
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use super::*;
    use crate::metrics::capturing::{CapturingMetricsService, MetricValue};
    use crate::metrics::consts::{get_environment, METRIC_USAGE_HOURLY_LAG_SECONDS};
    use crate::usage::ports::{DayTotals, RecomputeReport};

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

    #[derive(Default)]
    struct FakeRepo {
        progress: Mutex<Option<HourlyProgress>>,
        lock_busy: bool,
        /// Raw totals differ from the aggregate on every checked day.
        parity_mismatch: bool,
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
            _lock_behavior: AggregateLockBehavior,
        ) -> anyhow::Result<Option<RecomputeReport>> {
            if self.lock_busy {
                return Ok(None);
            }
            self.recomputes.lock().unwrap().push((from, to));
            Ok(Some(RecomputeReport { rows_written: 7 }))
        }
        async fn day_parity(&self, day: NaiveDate) -> anyhow::Result<DayParity> {
            self.parity_calls.lock().unwrap().push(day);
            let raw = DayTotals {
                request_count: i64::from(self.parity_mismatch),
                ..DayTotals::default()
            };
            Ok(DayParity {
                day,
                raw,
                aggregate: DayTotals::default(),
            })
        }
    }

    #[tokio::test]
    async fn tick_recomputes_planned_window_then_checks_parity_days() {
        let repo = Arc::new(FakeRepo::default());
        *repo.progress.lock().unwrap() = Some(p(None, Some("2026-05-01T14:00:00Z")));
        let scheduler =
            UsageHourlyScheduler::new(repo.clone(), Arc::new(crate::metrics::MockMetricsService));
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
    async fn tick_reports_parity_mismatch_in_the_outcome() {
        let repo = Arc::new(FakeRepo {
            parity_mismatch: true,
            ..Default::default()
        });
        *repo.progress.lock().unwrap() = Some(p(None, Some("2026-05-01T14:00:00Z")));
        let outcome =
            UsageHourlyScheduler::new(repo.clone(), Arc::new(crate::metrics::MockMetricsService))
                .run_once(t("2026-09-24T10:05:00Z"))
                .await
                .unwrap();
        assert_eq!(outcome.parity.len(), 3);
        assert!(outcome.parity.iter().all(|day| !day.is_ok()));
    }

    #[tokio::test]
    async fn tick_skips_without_parity_when_another_replica_holds_the_lock() {
        let repo = Arc::new(FakeRepo {
            lock_busy: true,
            ..Default::default()
        });
        *repo.progress.lock().unwrap() = Some(p(None, Some("2026-05-01T14:00:00Z")));
        let outcome =
            UsageHourlyScheduler::new(repo.clone(), Arc::new(crate::metrics::MockMetricsService))
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
        let behind =
            UsageHourlyScheduler::new(repo.clone(), Arc::new(crate::metrics::MockMetricsService))
                .run_once(t("2026-09-24T10:05:00Z"))
                .await
                .unwrap();
        assert!(!behind.caught_up);
        *repo.progress.lock().unwrap() = Some(p(Some("2026-09-24T09:00:00Z"), None));
        let current =
            UsageHourlyScheduler::new(repo.clone(), Arc::new(crate::metrics::MockMetricsService))
                .run_once(t("2026-09-24T10:05:00Z"))
                .await
                .unwrap();
        assert!(current.caught_up);
    }

    #[tokio::test]
    async fn tick_records_no_lag_across_a_quiet_span() {
        // Newest aggregate hour 02:00, no raw usage again until 10:00: the job is caught up,
        // so lag is measured from the pending 10:00 hour, not from 03:00 (7h05m).
        let repo = Arc::new(FakeRepo::default());
        *repo.progress.lock().unwrap() = Some(p(
            Some("2026-09-24T02:00:00Z"),
            Some("2026-09-24T10:00:00Z"),
        ));
        let metrics = Arc::new(CapturingMetricsService::new());
        UsageHourlyScheduler::new(repo, metrics.clone())
            .run_once(t("2026-09-24T10:05:00Z"))
            .await
            .unwrap();
        let recorded = metrics.get_metrics();
        assert_eq!(recorded.len(), 1);
        assert!(matches!(recorded[0].value, MetricValue::Histogram(v) if v == 300.0));
    }

    #[tokio::test]
    async fn tick_records_lag_from_the_oldest_pending_raw_hour_when_stalled() {
        // Newest aggregate hour 02:00 with raw usage every hour since: stalled for 7h05m.
        let repo = Arc::new(FakeRepo::default());
        *repo.progress.lock().unwrap() = Some(p(
            Some("2026-09-24T02:00:00Z"),
            Some("2026-09-24T03:00:00Z"),
        ));
        let metrics = Arc::new(CapturingMetricsService::new());
        UsageHourlyScheduler::new(repo, metrics.clone())
            .run_once(t("2026-09-24T10:05:00Z"))
            .await
            .unwrap();
        let recorded = metrics.get_metrics();
        assert_eq!(recorded.len(), 1);
        assert!(matches!(recorded[0].value, MetricValue::Histogram(v) if v == 25_500.0));
    }

    #[tokio::test]
    async fn tick_records_lag_from_the_current_hour_when_nothing_is_pending() {
        let repo = Arc::new(FakeRepo::default());
        *repo.progress.lock().unwrap() = Some(p(Some("2026-09-24T09:00:00Z"), None));
        let metrics = Arc::new(CapturingMetricsService::new());
        UsageHourlyScheduler::new(repo, metrics.clone())
            .run_once(t("2026-09-24T10:05:00Z"))
            .await
            .unwrap();
        let recorded = metrics.get_metrics();
        assert_eq!(recorded.len(), 1);
        assert_eq!(recorded[0].name, METRIC_USAGE_HOURLY_LAG_SECONDS);
        assert_eq!(
            recorded[0].tags,
            vec![format!("{TAG_ENVIRONMENT}:{}", get_environment())]
        );
        // Nothing pending after 09:00: now − current hour = 10:05 − 10:00.
        assert!(matches!(recorded[0].value, MetricValue::Histogram(v) if v == 300.0));
    }

    #[tokio::test]
    async fn tick_records_no_lag_before_the_first_aggregate() {
        let repo = Arc::new(FakeRepo::default());
        *repo.progress.lock().unwrap() = Some(p(None, Some("2026-05-01T14:00:00Z")));
        let metrics = Arc::new(CapturingMetricsService::new());
        UsageHourlyScheduler::new(repo, metrics.clone())
            .run_once(t("2026-09-24T10:05:00Z"))
            .await
            .unwrap();
        assert!(metrics.get_metrics().is_empty());
    }

    #[tokio::test]
    async fn tick_records_lag_while_catching_up_and_when_skipped() {
        // Catch-up tick (months behind) and a tick that loses the lock both report freshness.
        for lock_busy in [false, true] {
            let repo = Arc::new(FakeRepo {
                lock_busy,
                ..Default::default()
            });
            *repo.progress.lock().unwrap() = Some(p(
                Some("2026-05-03T20:00:00Z"),
                Some("2026-05-13T07:00:00Z"),
            ));
            let metrics = Arc::new(CapturingMetricsService::new());
            let outcome = UsageHourlyScheduler::new(repo, metrics.clone())
                .run_once(t("2026-09-24T10:05:00Z"))
                .await
                .unwrap();
            assert!(!outcome.caught_up, "lock_busy={lock_busy}");
            assert_eq!(outcome.skipped, lock_busy);
            let recorded = metrics.get_metrics();
            assert_eq!(recorded.len(), 1, "lock_busy={lock_busy}");
            assert_eq!(recorded[0].name, METRIC_USAGE_HOURLY_LAG_SECONDS);
            // 09-24 10:05 − 05-13 07:00 (oldest pending raw hour; the empty span before it is
            // not lag) = 134 days 3 h 5 min.
            assert!(matches!(recorded[0].value, MetricValue::Histogram(v) if v == 11_588_700.0));
        }
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

    #[test]
    fn repair_window_must_be_whole_ordered_hours_within_the_cap() {
        assert!(
            validate_repair_window(t("2026-09-01T00:00:00Z"), t("2026-09-01T01:00:00Z")).is_ok()
        );
        assert!(
            validate_repair_window(t("2026-09-01T00:30:00Z"), t("2026-09-01T01:00:00Z")).is_err()
        );
        assert!(
            validate_repair_window(t("2026-09-01T00:00:00Z"), t("2026-09-01T01:00:01Z")).is_err()
        );
        assert!(
            validate_repair_window(t("2026-09-01T01:00:00Z"), t("2026-09-01T01:00:00Z")).is_err()
        );
        assert!(
            validate_repair_window(t("2026-09-01T00:00:00Z"), t("2026-10-02T00:00:00Z")).is_ok()
        );
        assert!(
            validate_repair_window(t("2026-09-01T00:00:00Z"), t("2026-10-02T01:00:00Z")).is_err()
        );
    }

    #[tokio::test]
    async fn repair_recomputes_one_utc_day_per_transaction_and_reports_parity_per_day() {
        let repo = FakeRepo {
            parity_mismatch: true,
            ..FakeRepo::default()
        };
        *repo.progress.lock().unwrap() = Some(p(
            Some("2026-09-24T09:00:00Z"),
            Some("2026-09-24T10:00:00Z"),
        ));
        let report = repair(&repo, t("2026-09-01T22:00:00Z"), t("2026-09-03T02:00:00Z"))
            .await
            .unwrap();
        assert_eq!(
            *repo.recomputes.lock().unwrap(),
            vec![
                (t("2026-09-01T22:00:00Z"), t("2026-09-02T00:00:00Z")),
                (t("2026-09-02T00:00:00Z"), t("2026-09-03T00:00:00Z")),
                (t("2026-09-03T00:00:00Z"), t("2026-09-03T02:00:00Z")),
            ]
        );
        assert_eq!(report.rows_written, 21);
        let days: Vec<NaiveDate> = report.days.iter().map(|p| p.day).collect();
        assert_eq!(
            days,
            vec![d("2026-09-01"), d("2026-09-02"), d("2026-09-03")]
        );
        assert!(report.days.iter().all(|p| !p.is_ok()));
    }

    #[tokio::test]
    async fn repair_rejects_an_invalid_window_without_touching_the_repository() {
        let repo = FakeRepo::default();
        assert!(matches!(
            repair(&repo, t("2026-09-01T00:30:00Z"), t("2026-09-01T02:00:00Z")).await,
            Err(RepairError::InvalidWindow(_))
        ));
        assert!(repo.recomputes.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn repair_rejects_a_window_that_starts_past_the_next_uncomputed_raw_hour() {
        // Catch-up has computed through March 10 and the next raw hour is 11:00. Repairing
        // September would move max_hour past March 10 11:00 .. September, which nothing computes.
        let repo = FakeRepo::default();
        *repo.progress.lock().unwrap() = Some(p(
            Some("2026-03-10T10:00:00Z"),
            Some("2026-03-10T11:00:00Z"),
        ));
        let result = repair(&repo, t("2026-09-20T00:00:00Z"), t("2026-09-27T00:00:00Z")).await;
        assert!(matches!(
            result,
            Err(RepairError::AheadOfAggregate { frontier }) if frontier == t("2026-03-10T11:00:00Z")
        ));
        assert!(repo.recomputes.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn repair_may_start_at_or_before_the_frontier_or_anywhere_when_nothing_is_pending() {
        for (progress, from) in [
            // Starting exactly at the next uncomputed raw hour extends the aggregate contiguously.
            (
                p(Some("2026-03-10T10:00:00Z"), Some("2026-03-10T11:00:00Z")),
                "2026-03-10T11:00:00Z",
            ),
            // The empty span after max_hour holds no raw rows, so starting inside it leaves no hole.
            (
                p(Some("2026-03-10T10:00:00Z"), Some("2026-03-20T07:00:00Z")),
                "2026-03-15T00:00:00Z",
            ),
            // Empty aggregate: the repair must start at or before the oldest raw hour.
            (
                p(None, Some("2026-01-01T05:00:00Z")),
                "2026-01-01T00:00:00Z",
            ),
            // No raw hour pending: every raw row is already below max_hour + 1h.
            (
                p(Some("2026-09-24T09:00:00Z"), None),
                "2026-09-25T00:00:00Z",
            ),
        ] {
            let repo = FakeRepo::default();
            *repo.progress.lock().unwrap() = Some(progress);
            let from = t(from);
            repair(&repo, from, from + TimeDelta::hours(2))
                .await
                .unwrap_or_else(|error| panic!("{progress:?} from {from}: {error}"));
            assert_eq!(repo.recomputes.lock().unwrap()[0].0, from);
        }
    }
}
