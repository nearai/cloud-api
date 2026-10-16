//! Time from a measurement's first `unknown_*` rejection to its first
//! admission (the `cloud_api.attested_3p.time_to_trust_seconds` (`provider:tinfoil`) metric).
//!
//! Keys are router release digests and `slug@tag` strings, never customer data.

use std::sync::Mutex;

use tokio::time::Instant;

/// Rejected measurements remembered at once. At the bound new keys are
/// ignored, never evicting a measurement already being timed, so a flapping
/// router cannot grow the set without limit or erase a clock in progress.
const MAX_PENDING: usize = 64;

#[derive(Default)]
struct Inner {
    /// `(kind, key, first rejected at)`.
    pending: Vec<(&'static str, String, Instant)>,
    /// `(kind, seconds)` admitted since the last drain.
    done: Vec<(&'static str, f64)>,
}

#[derive(Default)]
pub(super) struct TrustTimer(Mutex<Inner>);

impl TrustTimer {
    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.0.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// The first rejection of `key` starts its clock; repeats keep it.
    pub(super) fn rejected(&self, kind: &'static str, key: &str) {
        let mut g = self.lock();
        if g.pending.iter().any(|(k, n, _)| *k == kind && n == key) {
            return;
        }
        if g.pending.len() >= MAX_PENDING {
            return;
        }
        g.pending.push((kind, key.to_string(), Instant::now()));
    }

    /// The first admission of a previously rejected `key` records its
    /// elapsed time once and forgets it.
    pub(super) fn admitted(&self, kind: &'static str, key: &str) {
        let mut g = self.lock();
        if let Some(i) = g
            .pending
            .iter()
            .position(|(k, n, _)| *k == kind && n == key)
        {
            let (_, _, at) = g.pending.remove(i);
            if g.done.len() < MAX_PENDING {
                g.done.push((kind, at.elapsed().as_secs_f64()));
            }
        }
    }

    pub(super) fn take(&self) -> Vec<(&'static str, f64)> {
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
        t.admitted("router", "d1"); // never rejected: nothing
        assert!(t.take().is_empty());
        t.rejected("router", "d1");
        tokio::time::advance(Duration::from_secs(30)).await;
        t.rejected("router", "d1"); // repeat keeps the first clock
        tokio::time::advance(Duration::from_secs(30)).await;
        t.admitted("router", "d1");
        assert_eq!(t.take(), vec![("router", 60.0)]);
        t.admitted("router", "d1"); // forgotten: no second sample
        assert!(t.take().is_empty());
    }

    #[tokio::test(start_paused = true)]
    async fn kinds_are_independent() {
        let t = TrustTimer::default();
        t.rejected("model", "m@v1");
        tokio::time::advance(Duration::from_secs(5)).await;
        t.admitted("router", "m@v1");
        assert!(t.take().is_empty());
        t.admitted("model", "m@v1");
        assert_eq!(t.take(), vec![("model", 5.0)]);
    }

    #[tokio::test(start_paused = true)]
    async fn full_pending_set_ignores_new_keys() {
        let t = TrustTimer::default();
        for i in 0..(MAX_PENDING + 10) {
            t.rejected("model", &format!("s{i}@v"));
        }
        assert_eq!(t.lock().pending.len(), MAX_PENDING);
        tokio::time::advance(Duration::from_secs(7)).await;
        // The oldest is still being timed; keys past the bound never were.
        t.admitted("model", "s0@v");
        assert_eq!(t.take(), vec![("model", 7.0)]);
        t.admitted("model", &format!("s{}@v", MAX_PENDING + 9));
        assert!(t.take().is_empty());
        // A freed slot admits a new key again.
        t.rejected("model", "late@v");
        tokio::time::advance(Duration::from_secs(2)).await;
        t.admitted("model", "late@v");
        assert_eq!(t.take(), vec![("model", 2.0)]);
    }
}
