//! Time from a measurement's first `unknown_*` rejection to its first
//! admission (the `cloud_api.attested_3p.time_to_trust_seconds` (`provider:tinfoil`) metric).
//!
//! Keys are router release digests and `slug@tag` strings, never customer data.

use std::sync::Mutex;
use std::time::Duration;

use tokio::time::Instant;

/// Which measurement a time-to-trust sample belongs to. The metric label is
/// `as_str()`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrustKind {
    Router,
    Model,
}

impl TrustKind {
    pub fn as_str(self) -> &'static str {
        match self {
            TrustKind::Router => "router",
            TrustKind::Model => "model",
        }
    }
}

/// Rejected measurements remembered at once. At the bound new keys are
/// ignored, never evicting a measurement already being timed, so a flapping
/// router cannot grow the set without limit or erase a clock in progress.
///
/// The same bound caps `done` (admitted samples not yet drained by the API
/// poller, which runs about once a minute). When a stalled poller leaves
/// `done` full, further samples are dropped (with a warning) rather than
/// growing without limit.
const MAX_PENDING: usize = 64;

/// A key not rejected again for this long is dropped on the next `rejected`.
/// Keys churn (superseded router releases, unpublished slugs), and without
/// expiry stale keys would fill the set for the rest of the process lifetime.
/// A key still being rejected keeps refreshing `last_seen`, so its clock
/// survives however long the delay lasts.
const PENDING_TTL: Duration = Duration::from_secs(24 * 60 * 60);

struct Pending {
    kind: TrustKind,
    key: String,
    first_rejected: Instant,
    last_seen: Instant,
}

#[derive(Default)]
struct Inner {
    pending: Vec<Pending>,
    /// `(kind, seconds)` admitted since the last drain.
    done: Vec<(TrustKind, f64)>,
    /// The "pending set full" warning was already emitted; reset once the set
    /// drains below the bound, so a full set warns once, not per attempt.
    pending_full_warned: bool,
}

#[derive(Default)]
pub(super) struct TrustTimer(Mutex<Inner>);

impl TrustTimer {
    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.0.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// The first rejection of `key` starts its clock; repeats keep it and
    /// refresh `last_seen`. Keys not seen for `PENDING_TTL` are dropped first
    /// so their slots free up.
    pub(super) fn rejected(&self, kind: TrustKind, key: &str) {
        let mut g = self.lock();
        let now = Instant::now();
        if let Some(p) = g
            .pending
            .iter_mut()
            .find(|p| p.kind == kind && p.key == key)
        {
            p.last_seen = now;
        }
        g.pending
            .retain(|p| now.duration_since(p.last_seen) < PENDING_TTL);
        if g.pending.len() < MAX_PENDING {
            g.pending_full_warned = false;
        }
        if g.pending.iter().any(|p| p.kind == kind && p.key == key) {
            return;
        }
        if g.pending.len() >= MAX_PENDING {
            if !g.pending_full_warned {
                g.pending_full_warned = true;
                tracing::warn!("time-to-trust pending set full; new keys are not timed");
            }
            return;
        }
        g.pending.push(Pending {
            kind,
            key: key.to_string(),
            first_rejected: now,
            last_seen: now,
        });
    }

    /// The first admission of a previously rejected `key` records its
    /// elapsed time once and forgets it.
    pub(super) fn admitted(&self, kind: TrustKind, key: &str) {
        let mut g = self.lock();
        if let Some(i) = g
            .pending
            .iter()
            .position(|p| p.kind == kind && p.key == key)
        {
            let at = g.pending.remove(i).first_rejected;
            g.pending_full_warned = false;
            if g.done.len() < MAX_PENDING {
                g.done.push((kind, at.elapsed().as_secs_f64()));
            } else {
                tracing::warn!("time-to-trust sample dropped; undrained samples are full");
            }
        }
    }

    pub(super) fn take(&self) -> Vec<(TrustKind, f64)> {
        std::mem::take(&mut self.lock().done)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[tokio::test(start_paused = true)]
    async fn emits_once_from_first_rejection_to_first_admission() {
        let t = TrustTimer::default();
        t.admitted(TrustKind::Router, "d1"); // never rejected: nothing
        assert!(t.take().is_empty());
        t.rejected(TrustKind::Router, "d1");
        tokio::time::advance(Duration::from_secs(30)).await;
        t.rejected(TrustKind::Router, "d1"); // repeat keeps the first clock
        tokio::time::advance(Duration::from_secs(30)).await;
        t.admitted(TrustKind::Router, "d1");
        assert_eq!(t.take(), vec![(TrustKind::Router, 60.0)]);
        t.admitted(TrustKind::Router, "d1"); // forgotten: no second sample
        assert!(t.take().is_empty());
    }

    #[tokio::test(start_paused = true)]
    async fn stale_clock_expires_and_frees_a_slot() {
        let t = TrustTimer::default();
        for i in 0..MAX_PENDING {
            t.rejected(TrustKind::Model, &format!("s{i}@v"));
        }
        t.rejected(TrustKind::Model, "full@v"); // ignored, set is full
        assert_eq!(t.lock().pending.len(), MAX_PENDING);
        tokio::time::advance(PENDING_TTL + Duration::from_secs(1)).await;
        t.rejected(TrustKind::Model, "fresh@v"); // old clocks expire first
        assert_eq!(t.lock().pending.len(), 1);
        tokio::time::advance(Duration::from_secs(3)).await;
        t.admitted(TrustKind::Model, "s0@v"); // expired: no sample
        assert!(t.take().is_empty());
        t.admitted(TrustKind::Model, "fresh@v");
        assert_eq!(t.take(), vec![(TrustKind::Model, 3.0)]);
    }

    #[tokio::test(start_paused = true)]
    async fn continuously_rejected_key_keeps_its_first_clock() {
        let t = TrustTimer::default();
        t.rejected(TrustKind::Router, "d1");
        for _ in 0..(25 * 60) {
            tokio::time::advance(Duration::from_secs(60)).await;
            t.rejected(TrustKind::Router, "d1");
        }
        t.admitted(TrustKind::Router, "d1");
        let samples = t.take();
        assert_eq!(samples.len(), 1);
        assert!((samples[0].1 - 25.0 * 3600.0).abs() < 5.0, "{samples:?}");
    }

    #[tokio::test(start_paused = true)]
    async fn kinds_are_independent() {
        let t = TrustTimer::default();
        t.rejected(TrustKind::Model, "m@v1");
        tokio::time::advance(Duration::from_secs(5)).await;
        t.admitted(TrustKind::Router, "m@v1");
        assert!(t.take().is_empty());
        t.admitted(TrustKind::Model, "m@v1");
        assert_eq!(t.take(), vec![(TrustKind::Model, 5.0)]);
    }

    #[tokio::test(start_paused = true)]
    async fn full_pending_set_ignores_new_keys() {
        let t = TrustTimer::default();
        for i in 0..(MAX_PENDING + 10) {
            t.rejected(TrustKind::Model, &format!("s{i}@v"));
        }
        assert_eq!(t.lock().pending.len(), MAX_PENDING);
        tokio::time::advance(Duration::from_secs(7)).await;
        // The oldest is still being timed; keys past the bound never were.
        t.admitted(TrustKind::Model, "s0@v");
        assert_eq!(t.take(), vec![(TrustKind::Model, 7.0)]);
        t.admitted(TrustKind::Model, &format!("s{}@v", MAX_PENDING + 9));
        assert!(t.take().is_empty());
        // A freed slot admits a new key again.
        t.rejected(TrustKind::Model, "late@v");
        tokio::time::advance(Duration::from_secs(2)).await;
        t.admitted(TrustKind::Model, "late@v");
        assert_eq!(t.take(), vec![(TrustKind::Model, 2.0)]);
    }
}
