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

use super::config::{Config, PROXY_REREAD, ROUTER_REVERIFY};
use super::verifier_port::{
    AtcBundle, PinnedModel, ProxyDoc, TinfoilVerifier, TinfoilVerifyError, VerifiedRouter,
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
    /// `https://{bundle.domain}` (a test override takes precedence in tests).
    pub(crate) base: String,
    pub(crate) domain: String,
    pub(crate) spki_hex: String,
}

/// A router host must be a bare lowercase ASCII `*.tinfoil.sh` hostname: no
/// scheme, port, path, userinfo, uppercase or IDN. The ATC serves bundles for
/// several router hosts, and the domain decides where requests are sent, so it
/// is validated before it is trusted.
pub fn validate_router_domain(domain: &str) -> Result<(), TinfoilVerifyError> {
    const SUFFIX: &str = ".tinfoil.sh";
    let Some(prefix) = domain.strip_suffix(SUFFIX) else {
        return Err(TinfoilVerifyError::Malformed);
    };
    let label_ok = |l: &str| {
        !l.is_empty()
            && l.len() <= 63
            && !l.starts_with('-')
            && !l.ends_with('-')
            && l.bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
    };
    if domain.len() > 253 || !prefix.split('.').all(label_ok) {
        return Err(TinfoilVerifyError::Malformed);
    }
    Ok(())
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
    #[cfg(test)]
    base_override: Option<String>,
    #[cfg(test)]
    route: Option<std::net::SocketAddr>,
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
            #[cfg(test)]
            base_override: cfg.base_override.clone(),
            #[cfg(test)]
            route: cfg.route,
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
        #[allow(unused_mut)]
        let mut builder = reqwest::Client::builder()
            .use_preconfigured_tls(self.tls_roots.build_config(fp))
            .redirect(reqwest::redirect::Policy::none())
            .connect_timeout(CONNECT_TIMEOUT);
        #[allow(unused_mut)]
        let mut base = format!("https://{domain}");
        #[cfg(test)]
        {
            if let Some(addr) = self.route {
                builder = builder.resolve(domain, addr);
                base = format!("https://{domain}:{}", addr.port());
            }
            if let Some(o) = &self.base_override {
                base = o.clone();
            }
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
        let bytes = resp.bytes().await.map_err(|_| TinfoilVerifyError::Fetch)?;
        if bytes.len() > MAX_DOC_BYTES {
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
                    tracing::info!(slug = %slug, repo = %p.repo, tag = %p.tag, "Tinfoil model pinned")
                }
                Err(e) => tracing::warn!(slug = %slug, reason = e.reason(), "Tinfoil model closed"),
            }
        }
        self.state.store(Arc::new(Some(VerifiedState {
            router,
            models,
            verified_at: Instant::now(),
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
    /// router rotated its key). Re-verifies once, deduplicated against any
    /// verification that finished since `seen_generation` and rate limited.
    pub(super) async fn on_connect_failure(&self, seen_generation: u64) {
        let _guard = self.verify_lock.lock().await;
        if self.verify_gen.load(Ordering::SeqCst) != seen_generation {
            return;
        }
        {
            let mut last = self
                .last_mismatch_reverify
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            if last.is_some_and(|t| t.elapsed() < MISMATCH_REVERIFY_COOLDOWN) {
                return;
            }
            *last = Some(Instant::now());
        }
        let _ = self.verify_locked().await;
    }

    /// Start the proxy re-read (60 s) and full re-verify (300 s) loop. Holds a
    /// `Weak` reference, so it ends once the session is dropped. Idempotent.
    pub fn spawn_refresh(self: &Arc<Self>) {
        if self.refresh_started.swap(true, Ordering::SeqCst) {
            return;
        }
        let weak: Weak<Self> = Arc::downgrade(self);
        tokio::spawn(async move {
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
        });
    }

    /// The router's published context window for `slug` (`GET /v1/models`,
    /// unauthenticated, over the pinned client).
    pub async fn published_context_window(&self, slug: &str) -> Option<u32> {
        let snap = self.state.load_full();
        let t = &snap.as_ref().as_ref()?.transport;
        let v: serde_json::Value = Self::fetch_json(&t.client, &format!("{}/v1/models", t.base))
            .await
            .ok()?;
        v.get("data")?
            .as_array()?
            .iter()
            .find(|m| m.get("id").and_then(|i| i.as_str()) == Some(slug))?
            .get("context_window")?
            .as_u64()
            .and_then(|n| u32::try_from(n).ok())
    }
}
