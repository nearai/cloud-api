//! The provider-neutral public pins view served by `GET /v1/attestation/pins`.
//!
//! Each attested backup provider contributes one section, keyed by its
//! [`ProviderSource::as_str`](inference_providers::ProviderSource::as_str)
//! name. The rendered body is memoized per vector of section versions, so the
//! anonymous route costs one atomic read per provider on a hit.

use std::sync::{Arc, Mutex};

/// One backup provider's public trust inputs (compiled pins plus any runtime trust it accepts).
pub trait ProviderPinsSource: Send + Sync {
    fn provider(&self) -> &'static str;
    /// Version of the provider's runtime trust inputs; the rendered view is rebuilt only when a version changes. Compiled-only providers return 0.
    fn version(&self) -> u64;
    /// The provider's public section. May do slow work (signature verification); called off the request path and only when `version` changed.
    fn public_view(&self) -> serde_json::Value;
}

/// The current version of every source, in source order.
pub fn source_versions(sources: &[Arc<dyn ProviderPinsSource>]) -> Vec<u64> {
    sources.iter().map(|s| s.version()).collect()
}

/// The full body: an object keyed by provider name. Slow; call off the
/// request path.
pub fn render_pins_view(sources: &[Arc<dyn ProviderPinsSource>]) -> serde_json::Value {
    serde_json::Value::Object(
        sources
            .iter()
            .map(|s| (s.provider().to_string(), s.public_view()))
            .collect(),
    )
}

/// The rendered body for the latest version vector seen. A body for a
/// different vector never replaces a newer one: versions only grow, so the
/// stored vector is replaced only when it is componentwise not newer.
#[derive(Default)]
pub struct PinsViewMemo {
    slot: Mutex<Option<(Vec<u64>, Arc<serde_json::Value>)>>,
    /// Held while a body is built, so concurrent misses build once: later
    /// callers wait, then find the body already memoized.
    build_gate: Mutex<()>,
}

impl PinsViewMemo {
    /// The body rendered for exactly `versions`, if memoized.
    pub fn get(&self, versions: &[u64]) -> Option<Arc<serde_json::Value>> {
        let slot = self.slot.lock().unwrap_or_else(|e| e.into_inner());
        slot.as_ref()
            .filter(|(v, _)| v.as_slice() == versions)
            .map(|(_, body)| body.clone())
    }

    /// Memoize `body` for `versions` unless a newer vector is stored.
    pub fn put(&self, versions: Vec<u64>, body: Arc<serde_json::Value>) {
        let mut slot = self.slot.lock().unwrap_or_else(|e| e.into_inner());
        let older = slot.as_ref().is_none_or(|(v, _)| {
            v.len() != versions.len() || v.iter().zip(&versions).all(|(have, new)| have <= new)
        });
        if older {
            *slot = Some((versions, body));
        }
    }

    /// The memoized body for `versions`, or `build` it and memoize it. Single
    /// flight: concurrent misses serialize on a gate and re-check the memo,
    /// so a burst after startup or a version change builds once. The build
    /// runs outside the memo lock, so hits are never blocked by it.
    pub fn get_or_build(
        &self,
        versions: Vec<u64>,
        build: impl FnOnce() -> serde_json::Value,
    ) -> Arc<serde_json::Value> {
        if let Some(hit) = self.get(&versions) {
            return hit;
        }
        let _gate = self.build_gate.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(hit) = self.get(&versions) {
            return hit;
        }
        let body = Arc::new(build());
        self.put(versions, body.clone());
        body
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    struct CountingSource {
        name: &'static str,
        version: AtomicU64,
        builds: AtomicU64,
    }

    impl CountingSource {
        fn new(name: &'static str) -> Arc<Self> {
            Arc::new(Self {
                name,
                version: AtomicU64::new(0),
                builds: AtomicU64::new(0),
            })
        }
    }

    impl ProviderPinsSource for CountingSource {
        fn provider(&self) -> &'static str {
            self.name
        }
        fn version(&self) -> u64 {
            self.version.load(Ordering::SeqCst)
        }
        fn public_view(&self) -> serde_json::Value {
            self.builds.fetch_add(1, Ordering::SeqCst);
            serde_json::json!({"v": self.version()})
        }
    }

    fn get(memo: &PinsViewMemo, sources: &[Arc<dyn ProviderPinsSource>]) -> Arc<serde_json::Value> {
        memo.get_or_build(source_versions(sources), || render_pins_view(sources))
    }

    #[test]
    fn memo_builds_each_source_once_per_version_vector() {
        let (a, b) = (CountingSource::new("a"), CountingSource::new("b"));
        let sources: Vec<Arc<dyn ProviderPinsSource>> = vec![a.clone(), b.clone()];
        let memo = PinsViewMemo::default();

        let first = get(&memo, &sources);
        let second = get(&memo, &sources);
        assert!(Arc::ptr_eq(&first, &second));
        assert_eq!(a.builds.load(Ordering::SeqCst), 1);
        assert_eq!(b.builds.load(Ordering::SeqCst), 1);
        assert_eq!(*first, serde_json::json!({"a": {"v": 0}, "b": {"v": 0}}));

        b.version.store(1, Ordering::SeqCst);
        let third = get(&memo, &sources);
        assert!(!Arc::ptr_eq(&first, &third));
        assert_eq!(a.builds.load(Ordering::SeqCst), 2, "whole body rebuilds");
        assert_eq!(b.builds.load(Ordering::SeqCst), 2);
        assert_eq!(third["b"]["v"], 1);
    }

    #[test]
    fn concurrent_misses_build_once() {
        let source = CountingSource::new("a");
        let sources: Vec<Arc<dyn ProviderPinsSource>> = vec![source.clone()];
        let memo = Arc::new(PinsViewMemo::default());
        let handles: Vec<_> = (0..8)
            .map(|_| {
                let (memo, sources) = (memo.clone(), sources.clone());
                std::thread::spawn(move || {
                    memo.get_or_build(source_versions(&sources), || {
                        std::thread::sleep(std::time::Duration::from_millis(50));
                        render_pins_view(&sources)
                    })
                })
            })
            .collect();
        for h in handles {
            h.join().unwrap();
        }
        assert_eq!(source.builds.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn an_older_vector_never_replaces_a_newer_body() {
        let memo = PinsViewMemo::default();
        memo.put(vec![2, 1], Arc::new(serde_json::json!("new")));
        memo.put(vec![1, 1], Arc::new(serde_json::json!("old")));
        assert!(memo.get(&[1, 1]).is_none());
        assert!(memo.get(&[2, 1]).is_some());
    }
}
