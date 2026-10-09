//! `TinfoilRouterSession`: the shared, attested transport to Tinfoil's router.
//!
//! Trust chain: the ATC bundle is verified by the injected [`TinfoilVerifier`]
//! (AMD chain, policy, measurement pins, `report_data` <-> TLS SPKI). Only then
//! is the SPKI added to the `FingerprintState` that the pinned reqwest client
//! enforces on every handshake. The state starts `Blocked` (never `Bootstrap`,
//! which accepts any WebPKI certificate) and returns to `Blocked` on any
//! verification failure. There is no unpinned client for router traffic; the
//! only unpinned fetch is the ATC bundle, which is evidence verified
//! cryptographically before anything is trusted.

use std::collections::{BTreeMap, HashSet};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, RwLock, Weak};
use std::time::{Duration, Instant};

use arc_swap::ArcSwap;

use super::config::{Config, Redirect, PROXY_REREAD, ROUTER_REVERIFY};
use super::verifier_port::{
    validate_router_domain, AtcBundle, PinnedModel, ProxyDoc, TinfoilVerifier, TinfoilVerifyError,
    VerifiedRouter,
};
use crate::spki_verifier::{FingerprintState, SharedTlsRoots};

/// Timeout for evidence and model-document fetches.
const FETCH_TIMEOUT: Duration = Duration::from_secs(15);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
/// Minimum spacing between re-verifications triggered by request-path
/// connection failures, so a persistently bad peer cannot hammer the ATC.
const MISMATCH_REVERIFY_COOLDOWN: Duration = Duration::from_secs(10);
/// Cap on evidence / model-document bodies.
pub(super) const MAX_DOC_BYTES: usize = 8 * 1024 * 1024;

const PROXY_PATH: &str = "/.well-known/tinfoil-proxy";

/// Result of the last successful verification.
pub struct VerifiedState {
    pub router: VerifiedRouter,
    /// Per published slug: pinned, or the reason it is closed.
    pub models: BTreeMap<String, Result<PinnedModel, TinfoilVerifyError>>,
    pub verified_at: Instant,
    /// Context window the router publishes per slug (`GET /v1/models`, fetched
    /// over the pinned client at verification). A slug missing here has no
    /// known window (the document was unavailable or omitted it).
    pub context_windows: BTreeMap<String, u32>,
    /// The attestation bundle that was verified (served by `get_attestation_report`).
    pub bundle: AtcBundle,
    /// Pinned client and request base for exactly this verification. Swapped
    /// atomically with the rest of the state, so a request never pairs one
    /// router's pin with another's host.
    pub(crate) transport: Arc<Transport>,
}

/// Everything needed to talk to the verified router.
pub(crate) struct Transport {
    pub(crate) client: reqwest::Client,
    /// `https://{bundle.domain}`.
    pub(crate) base: String,
    pub(crate) domain: String,
    pub(crate) spki_hex: String,
}

/// How a capped body read ended.
#[derive(Debug, PartialEq, Eq)]
pub(super) enum BodyRead {
    Complete,
    /// More than `cap` bytes were available; the buffer holds the first `cap`.
    Truncated,
    /// The stream errored; the buffer holds what arrived before that.
    Failed,
}

/// Read a response body, never holding more than `cap` bytes.
pub(super) async fn read_capped(resp: reqwest::Response, cap: usize) -> (Vec<u8>, BodyRead) {
    use futures_util::StreamExt;
    let mut buf: Vec<u8> = Vec::new();
    let mut stream = resp.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let Ok(chunk) = chunk else {
            return (buf, BodyRead::Failed);
        };
        let room = cap - buf.len();
        if chunk.len() > room {
            buf.extend_from_slice(&chunk[..room]);
            return (buf, BodyRead::Truncated);
        }
        buf.extend_from_slice(&chunk);
    }
    (buf, BodyRead::Complete)
}

pub struct TinfoilRouterSession {
    atc_url: String,
    verifier: Arc<dyn TinfoilVerifier>,
    state: ArcSwap<Option<VerifiedState>>,
    tls_roots: SharedTlsRoots,
    /// Fetches the ATC evidence bundle (WebPKI only; content is verified).
    atc_client: reqwest::Client,
    verify_lock: tokio::sync::Mutex<()>,
    /// Bumped after every completed verification attempt (single-flight dedupe).
    verify_gen: AtomicU64,
    last_mismatch_reverify: std::sync::Mutex<Option<Instant>>,
    refresh_started: AtomicBool,
    /// A request-path re-verification is scheduled or running (dedupe flag).
    reverify_pending: AtomicBool,
    redirect: Redirect,
    /// Count of upstream 401/402/403 responses. Exposed for the metric
    /// `cloud_api.tinfoil.upstream_auth_failure`, which this crate cannot emit.
    auth_failures: AtomicU64,
    /// Outcome of the last completed verification attempt (`None` = it passed
    /// or none has run). Polled by the API layer for
    /// `cloud_api.tinfoil.verification`, which this crate cannot emit.
    last_error: std::sync::Mutex<Option<TinfoilVerifyError>>,
}

impl TinfoilRouterSession {
    pub fn new(cfg: Config, verifier: Arc<dyn TinfoilVerifier>) -> Result<Arc<Self>, String> {
        Self::new_with_roots(cfg, verifier, SharedTlsRoots::load())
    }

    pub(crate) fn new_with_roots(
        cfg: Config,
        verifier: Arc<dyn TinfoilVerifier>,
        tls_roots: SharedTlsRoots,
    ) -> Result<Arc<Self>, String> {
        let atc_client = reqwest::Client::builder()
            .use_preconfigured_tls(
                tls_roots.build_config(Arc::new(RwLock::new(FingerprintState::Bootstrap))),
            )
            .redirect(reqwest::redirect::Policy::none())
            .connect_timeout(CONNECT_TIMEOUT)
            .build()
            .map_err(|e| format!("build Tinfoil ATC client: {e}"))?;
        Ok(Arc::new(Self {
            atc_url: cfg.atc_url.clone(),
            verifier,
            state: ArcSwap::from_pointee(None),
            tls_roots,
            atc_client,
            verify_lock: tokio::sync::Mutex::new(()),
            verify_gen: AtomicU64::new(0),
            last_mismatch_reverify: std::sync::Mutex::new(None),
            refresh_started: AtomicBool::new(false),
            reverify_pending: AtomicBool::new(false),
            redirect: cfg.redirect.clone(),
            auth_failures: AtomicU64::new(0),
            last_error: std::sync::Mutex::new(None),
        }))
    }

    /// Pin currently enforced: `Blocked` unless the router is verified.
    pub fn fingerprint_state(&self) -> FingerprintState {
        match &**self.state.load() {
            None => FingerprintState::Blocked,
            Some(st) => FingerprintState::Pinned(HashSet::from([st.transport.spki_hex.clone()])),
        }
    }

    /// Pinned client + base for `domain`, enforcing exactly `spki_hex`. Built
    /// fresh so no pooled connection outlives the pin it was made under.
    pub(super) fn build_transport(
        &self,
        domain: &str,
        spki_hex: &str,
    ) -> Result<Transport, String> {
        let fp = Arc::new(RwLock::new(FingerprintState::Pinned(HashSet::from([
            spki_hex.to_string(),
        ]))));
        let mut builder = reqwest::Client::builder()
            .use_preconfigured_tls(self.tls_roots.build_config(fp))
            .redirect(reqwest::redirect::Policy::none())
            .connect_timeout(CONNECT_TIMEOUT);
        let mut base = format!("https://{domain}");
        // `redirect` is default (a no-op) in production; only test constructors
        // set it, so the same builder runs in tests and release.
        if let Some(addr) = self.redirect.resolve {
            builder = builder.resolve(domain, addr);
            base = format!("https://{domain}:{}", addr.port());
        }
        if let Some(o) = &self.redirect.base {
            base = o.clone();
        }
        Ok(Transport {
            client: builder
                .build()
                .map_err(|e| format!("build pinned Tinfoil client: {e}"))?,
            base,
            domain: domain.to_string(),
            spki_hex: spki_hex.to_string(),
        })
    }

    pub(super) fn snapshot(&self) -> Arc<Option<VerifiedState>> {
        self.state.load_full()
    }

    pub(super) fn generation(&self) -> u64 {
        self.verify_gen.load(Ordering::SeqCst)
    }

    /// Whether a request-path re-verification is scheduled or running.
    #[cfg(test)]
    pub(super) fn reverify_pending(&self) -> bool {
        self.reverify_pending.load(Ordering::SeqCst)
    }

    /// Number of upstream 401/402/403 responses seen (key or billing problems).
    pub fn upstream_auth_failures(&self) -> u64 {
        self.auth_failures.load(Ordering::Relaxed)
    }

    /// Why the last verification attempt failed, or `None` if it passed.
    pub fn last_verify_error(&self) -> Option<TinfoilVerifyError> {
        self.last_error
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    pub(super) fn record_auth_failure(&self) {
        self.auth_failures.fetch_add(1, Ordering::Relaxed);
    }

    #[cfg(test)]
    pub(crate) fn install_state(&self, s: VerifiedState) {
        self.state.store(Arc::new(Some(s)));
    }

    /// Pin status of one slug as of the last proxy read. `Fetch` means the router
    /// is not currently verified; `UnknownModelMeasurement` means the router does
    /// not publish the slug.
    pub fn model_status(&self, slug: &str) -> Result<PinnedModel, TinfoilVerifyError> {
        match &**self.state.load() {
            None => Err(TinfoilVerifyError::Fetch),
            Some(st) => st
                .models
                .get(slug)
                .cloned()
                .unwrap_or(Err(TinfoilVerifyError::UnknownModelMeasurement)),
        }
    }

    fn fail_closed(&self) {
        self.state.store(Arc::new(None));
    }

    async fn fetch_json<T: serde::de::DeserializeOwned>(
        client: &reqwest::Client,
        url: &str,
    ) -> Result<T, TinfoilVerifyError> {
        let resp = client
            .get(url)
            .timeout(FETCH_TIMEOUT)
            .send()
            .await
            .map_err(|_| TinfoilVerifyError::Fetch)?;
        if !resp.status().is_success() {
            return Err(TinfoilVerifyError::Fetch);
        }
        // Refuse an oversized body up front, and never buffer more than the cap.
        if resp
            .content_length()
            .is_some_and(|n| n > MAX_DOC_BYTES as u64)
        {
            return Err(TinfoilVerifyError::Fetch);
        }
        let (bytes, outcome) = read_capped(resp, MAX_DOC_BYTES).await;
        if outcome != BodyRead::Complete {
            return Err(TinfoilVerifyError::Fetch);
        }
        serde_json::from_slice(&bytes).map_err(|_| TinfoilVerifyError::Fetch)
    }

    fn check_models(
        &self,
        doc: &ProxyDoc,
    ) -> BTreeMap<String, Result<PinnedModel, TinfoilVerifyError>> {
        doc.models
            .iter()
            .map(|(slug, entry)| (slug.clone(), self.verifier.check_model(slug, entry)))
            .collect()
    }

    /// ATC -> `verify_router` -> pin SPKI -> proxy document (over the pinned
    /// client) -> `check_model` per published slug. Single-flight: concurrent
    /// callers queue on one lock. Any failure leaves the session closed.
    pub async fn verify_now(&self) -> Result<(), TinfoilVerifyError> {
        let _guard = self.verify_lock.lock().await;
        self.verify_locked().await
    }

    async fn verify_locked(&self) -> Result<(), TinfoilVerifyError> {
        let result = self.do_verify().await;
        self.verify_gen.fetch_add(1, Ordering::SeqCst);
        *self.last_error.lock().unwrap_or_else(|e| e.into_inner()) = result.as_ref().err().cloned();
        if let Err(e) = &result {
            self.fail_closed();
            tracing::warn!(reason = e.reason(), "Tinfoil verification failed; closed");
        }
        result
    }

    /// ATC -> `verify_router` -> domain check -> proxy document fetched over a
    /// client pinned to the NEW key and host -> publish pin, host and state
    /// together. Nothing is published unless every step passes.
    ///
    /// The ATC serves bundles for more than one router host (observed live:
    /// `inference.tinfoil.sh` and `router-0.tinfoil.sh`, chosen per request),
    /// each attesting its own key, so requests go to the attested domain.
    async fn do_verify(&self) -> Result<(), TinfoilVerifyError> {
        let bundle: AtcBundle = Self::fetch_json(&self.atc_client, &self.atc_url).await?;
        let router = self.verifier.verify_router(&bundle)?;
        validate_router_domain(&bundle.domain)?;
        let fp = hex::encode(router.spki_sha256);

        let current = self.state.load_full();
        let transport = match &*current {
            Some(st) if st.transport.spki_hex == fp && st.transport.domain == bundle.domain => {
                st.transport.clone()
            }
            _ => Arc::new(
                self.build_transport(&bundle.domain, &fp)
                    .map_err(|_| TinfoilVerifyError::Fetch)?,
            ),
        };
        let doc: ProxyDoc = Self::fetch_json(
            &transport.client,
            &format!("{}{PROXY_PATH}", transport.base),
        )
        .await?;
        let models = self.check_models(&doc);
        // Published windows are best effort and never fail the verify. A failed
        // fetch keeps the last known windows so an oversized declared context
        // cannot start passing just because `/v1/models` is briefly down; only a
        // successful fetch replaces them.
        let context_windows = match Self::fetch_context_windows(&transport).await {
            Some(fresh) => fresh,
            None => current
                .as_ref()
                .as_ref()
                .map(|st| st.context_windows.clone())
                .unwrap_or_default(),
        };
        tracing::info!(
            router_tag = %router.tag,
            router_measurement = %router.measurement_hex,
            pinned_models = models.values().filter(|m| m.is_ok()).count(),
            published_models = models.len(),
            "Tinfoil router verified"
        );
        for (slug, m) in &models {
            match m {
                Ok(p) => {
                    tracing::info!(slug = %slug, repo = %p.entry.repo, tag = %p.entry.tag, "Tinfoil model pinned")
                }
                Err(e) => tracing::warn!(slug = %slug, reason = e.reason(), "Tinfoil model closed"),
            }
        }
        self.state.store(Arc::new(Some(VerifiedState {
            router,
            models,
            verified_at: Instant::now(),
            context_windows,
            bundle,
            transport,
        })));
        Ok(())
    }

    /// Re-read the router's model document and re-check every slug against the
    /// pins. A fetch failure, or a session that is not verified, escalates to a
    /// full re-verification (which fails closed).
    pub async fn reread_proxy(&self) {
        let _guard = self.verify_lock.lock().await;
        let current = self.state.load_full();
        let Some(cur) = &*current else {
            let _ = self.verify_locked().await;
            return;
        };
        let doc: Result<ProxyDoc, _> = Self::fetch_json(
            &cur.transport.client,
            &format!("{}{PROXY_PATH}", cur.transport.base),
        )
        .await;
        match doc {
            Ok(doc) => {
                let models = self.check_models(&doc);
                for (slug, m) in &models {
                    let was_ok = cur.models.get(slug).is_some_and(|p| p.is_ok());
                    if let (true, Err(e)) = (was_ok, m) {
                        tracing::warn!(slug = %slug, reason = e.reason(), "Tinfoil model closed");
                    }
                }
                self.state.store(Arc::new(Some(VerifiedState {
                    router: cur.router.clone(),
                    models,
                    verified_at: cur.verified_at,
                    context_windows: cur.context_windows.clone(),
                    bundle: cur.bundle.clone(),
                    transport: cur.transport.clone(),
                })));
            }
            Err(_) => {
                let _ = self.verify_locked().await;
            }
        }
    }

    /// A request-path connection failure (typically an SPKI mismatch after the
    /// router rotated its key). Schedules ONE detached re-verification and returns
    /// immediately, so the failing request is never held behind the (slow) ATC
    /// round trip or behind another verification holding the lock. Deduplicated
    /// three ways: against a verification that finished since `seen_generation`,
    /// against one already pending, and by a cooldown.
    pub(super) fn on_connect_failure(self: &Arc<Self>, seen_generation: u64) {
        if self.verify_gen.load(Ordering::SeqCst) != seen_generation {
            return;
        }
        if self.reverify_pending.swap(true, Ordering::SeqCst) {
            return;
        }
        {
            let mut last = self
                .last_mismatch_reverify
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            if last.is_some_and(|t| t.elapsed() < MISMATCH_REVERIFY_COOLDOWN) {
                self.reverify_pending.store(false, Ordering::SeqCst);
                return;
            }
            *last = Some(Instant::now());
        }
        let weak = Arc::downgrade(self);
        tokio::spawn(async move {
            let Some(session) = weak.upgrade() else {
                return;
            };
            {
                let _guard = session.verify_lock.lock().await;
                // A scheduled refresh may have verified while we queued.
                if session.verify_gen.load(Ordering::SeqCst) == seen_generation {
                    let _ = session.verify_locked().await;
                }
            }
            session.reverify_pending.store(false, Ordering::SeqCst);
        });
    }

    /// Start the proxy re-read (60 s) and full re-verify (300 s) loop. Holds a
    /// `Weak` reference, so it ends once the session is dropped. Idempotent.
    pub fn spawn_refresh(self: &Arc<Self>) {
        drop(self.spawn_refresh_task());
    }

    /// [`Self::spawn_refresh`], returning the task's handle (`None` when a
    /// refresh loop was already started) so tests can observe its exit.
    pub(super) fn spawn_refresh_task(self: &Arc<Self>) -> Option<tokio::task::JoinHandle<()>> {
        if self.refresh_started.swap(true, Ordering::SeqCst) {
            return None;
        }
        let weak: Weak<Self> = Arc::downgrade(self);
        Some(tokio::spawn(async move {
            let mut proxy = tokio::time::interval(PROXY_REREAD);
            let mut router = tokio::time::interval(ROUTER_REVERIFY);
            for t in [&mut proxy, &mut router] {
                t.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
                t.tick().await;
            }
            loop {
                enum Which {
                    Proxy,
                    Router,
                }
                let which = tokio::select! {
                    _ = proxy.tick() => Which::Proxy,
                    _ = router.tick() => Which::Router,
                };
                let Some(session) = weak.upgrade() else { break };
                match which {
                    Which::Proxy => session.reread_proxy().await,
                    Which::Router => {
                        let _ = session.verify_now().await;
                    }
                }
            }
        }))
    }

    /// The router's published context window for `slug` as of the last
    /// verification. `None` when the router is not verified, does not publish the
    /// slug, or its model list was unavailable then.
    pub fn published_context_window(&self, slug: &str) -> Option<u32> {
        let snap = self.state.load_full();
        snap.as_ref().as_ref()?.context_windows.get(slug).copied()
    }

    /// `GET /v1/models` (unauthenticated) over the pinned client, as slug ->
    /// `context_window`. `None` on any failure (unreachable, oversized, not
    /// JSON, no `data` array), so callers can tell it from an empty listing.
    async fn fetch_context_windows(t: &Transport) -> Option<BTreeMap<String, u32>> {
        let Ok(v) =
            Self::fetch_json::<serde_json::Value>(&t.client, &format!("{}/v1/models", t.base))
                .await
        else {
            return None;
        };
        let data = v.get("data").and_then(|d| d.as_array())?;
        Some(
            data.iter()
                .filter_map(|m| {
                    let id = m.get("id")?.as_str()?;
                    let w = u32::try_from(m.get("context_window")?.as_u64()?).ok()?;
                    Some((id.to_string(), w))
                })
                .collect(),
        )
    }
}
