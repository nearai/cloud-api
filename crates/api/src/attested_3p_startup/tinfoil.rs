//! Tinfoil attested backup: startup preflight, per-model registration and the
//! metrics sampler. Shared ordering and reservation live in the parent module.

use super::ensure_attested_3p_catalog_row;
use config::{AttestedThirdPartyModelEntry, ExternalProvidersConfig};
use database::repositories::ModelRepository;
use inference_providers::attested::tinfoil::verifier_port::TinfoilVerifyError;
use inference_providers::attested::tinfoil::{self as tinfoil_provider, TinfoilRouterSession};
use inference_providers::ProviderSource;
use services::attestation::tinfoil_pins::TinfoilPins;
use services::inference_provider_pool::InferenceProviderPool;
use services::metrics::{consts, MetricsServiceTrait};
use std::sync::Arc;

/// Why Tinfoil registration is skipped before any network work (fail-closed:
/// the canonical ids stay reserved with no provider, so requests 404/503).
#[derive(Debug, PartialEq, Eq)]
enum TinfoilSkip {
    NoModels,
    NoKey,
    PinsUnusable,
}

/// Decide whether Tinfoil can be registered at all. Pure so every skip branch
/// is unit-testable without a network.
fn tinfoil_preflight(
    cfg: &ExternalProvidersConfig,
    pins: Result<TinfoilPins, String>,
) -> Result<(String, TinfoilPins), TinfoilSkip> {
    if cfg.tinfoil_models.is_empty() {
        return Err(TinfoilSkip::NoModels);
    }
    let Some(key) = cfg.tinfoil_api_key.clone() else {
        return Err(TinfoilSkip::NoKey);
    };
    match pins {
        Ok(p) if !p.router.is_empty() => Ok((key, p)),
        _ => Err(TinfoilSkip::PinsUnusable),
    }
}

/// Why one `TINFOIL_MODELS` entry is not registered.
#[derive(Debug, PartialEq, Eq)]
enum TinfoilEntrySkip {
    MissingCtx,
    ExceedsPublished { ctx: u32, published: u32 },
}

/// The one `@ctx` validator for a `TINFOIL_MODELS` entry: `@ctx` is required,
/// and must not exceed the router's published window when that is known.
fn tinfoil_entry_ctx(
    entry: &AttestedThirdPartyModelEntry,
    published: Option<u32>,
) -> Result<u32, TinfoilEntrySkip> {
    let ctx = entry
        .max_context_tokens
        .ok_or(TinfoilEntrySkip::MissingCtx)?;
    match published {
        Some(w) if ctx > w => Err(TinfoilEntrySkip::ExceedsPublished { ctx, published: w }),
        _ => Ok(ctx),
    }
}

/// Production entry: compiled pins and the real session constructor.
pub(super) async fn register_tinfoil(
    pool: &Arc<InferenceProviderPool>,
    models_repo: &ModelRepository,
    cfg: &ExternalProvidersConfig,
    metrics: Arc<dyn MetricsServiceTrait>,
) {
    register_tinfoil_with(
        pool,
        models_repo,
        cfg,
        services::attestation::tinfoil::vetted_tinfoil_pins(),
        |pcfg, verifier| TinfoilRouterSession::new(pcfg, verifier),
        metrics,
    )
    .await;
}

/// Tinfoil attested backup. Fail-closed at every step: no key, no usable pins,
/// a session that cannot be built, a missing/oversized `@ctx` or a missing
/// catalog row means that provider is not registered (its id stays reserved).
/// A failed first verification still registers: providers answer 503 until a
/// later re-verify succeeds. Pins and the session constructor are injected so
/// the branches can be tested without the network.
pub async fn register_tinfoil_with(
    pool: &Arc<InferenceProviderPool>,
    models_repo: &ModelRepository,
    cfg: &ExternalProvidersConfig,
    pins: Result<TinfoilPins, String>,
    build_session: impl FnOnce(
        tinfoil_provider::Config,
        Arc<services::attestation::tinfoil::TinfoilPolicyVerifier>,
    ) -> Result<Arc<TinfoilRouterSession>, String>,
    metrics: Arc<dyn MetricsServiceTrait>,
) {
    let (api_key, pins) = match tinfoil_preflight(cfg, pins) {
        Ok(ok) => ok,
        Err(TinfoilSkip::NoModels) => return,
        Err(TinfoilSkip::NoKey) => {
            tracing::warn!(
                "TINFOIL_MODELS set but TINFOIL_API_KEY missing; ids stay reserved (fail-closed)"
            );
            return;
        }
        Err(TinfoilSkip::PinsUnusable) => {
            tracing::warn!("Tinfoil pins empty or invalid; not registering (fail-closed)");
            return;
        }
    };
    let verifier = Arc::new(services::attestation::tinfoil::TinfoilPolicyVerifier::new(
        pins,
    ));
    let pcfg = tinfoil_provider::Config::new(api_key, cfg.timeout_seconds);
    let session = match build_session(pcfg.clone(), verifier) {
        Ok(s) => s,
        Err(e) => {
            tracing::warn!(error = %e, "Failed to build Tinfoil router session; not registering");
            return;
        }
    };
    session.spawn_refresh();
    if let Err(e) = session.verify_now().await {
        tracing::warn!(
            reason = e.reason(),
            "Initial Tinfoil verification failed; registering anyway (503 until a re-verify succeeds)"
        );
    }
    register_tinfoil_models(pool, models_repo, cfg, &session, &pcfg, metrics).await;
}

/// Per-entry registration against an existing session. The no-catalog-row skip
/// is covered end-to-end by
/// `chutes_catalog::ensure_catalog_row_without_seed_and_without_row_returns_none`.
pub async fn register_tinfoil_models(
    pool: &Arc<InferenceProviderPool>,
    models_repo: &ModelRepository,
    cfg: &ExternalProvidersConfig,
    session: &Arc<TinfoilRouterSession>,
    pcfg: &tinfoil_provider::Config,
    metrics: Arc<dyn MetricsServiceTrait>,
) {
    let mut registered: Vec<(String, String)> = Vec::new();
    for entry in &cfg.tinfoil_models {
        // The single @ctx validator. Unverified at boot (no published window
        // yet) is not a reason to skip: the provider re-checks the declaration
        // after every verification and fails closed if it is too large.
        let ctx = match tinfoil_entry_ctx(
            entry,
            session.published_context_window(&entry.upstream_id),
        ) {
            Ok(ctx) => ctx,
            Err(TinfoilEntrySkip::MissingCtx) => {
                tracing::error!(
                    canonical = %entry.canonical_id,
                    "TINFOIL_MODELS entry lacks @ctx; not registered"
                );
                continue;
            }
            Err(TinfoilEntrySkip::ExceedsPublished { ctx, published }) => {
                tracing::error!(
                    canonical = %entry.canonical_id,
                    ctx,
                    published,
                    "TINFOIL_MODELS @ctx exceeds the router's published context window; not registered"
                );
                continue;
            }
        };
        let Some(role) = ensure_attested_3p_catalog_row(
            models_repo,
            ProviderSource::Tinfoil,
            &entry.canonical_id,
            None,
        )
        .await
        else {
            continue;
        };
        let provider = tinfoil_provider::Provider::new(
            session.clone(),
            pcfg,
            entry.upstream_id.clone(),
            entry.canonical_id.clone(),
        )
        .with_declared_ctx(ctx);
        pool.register_pinned_provider(
            entry.canonical_id.clone(),
            Arc::new(provider),
            Some(ctx),
            role,
        )
        .await;
        tracing::info!(
            canonical = %entry.canonical_id,
            slug = %entry.upstream_id,
            role = ?role,
            "Registered Tinfoil attested provider"
        );
        registered.push((entry.canonical_id.clone(), entry.upstream_id.clone()));
    }
    if !registered.is_empty() {
        drop(spawn_tinfoil_metrics(session.clone(), registered, metrics));
    }
}

/// One metrics sample set from the session's current state. `last_auth` holds
/// the auth-failure count already reported so only the delta is emitted.
///
/// `inference_providers` cannot depend on the metrics crate, so the API layer
/// polls the session (same cadence as the router's 60 s proxy re-read) instead
/// of the provider pushing. `available` is a 0/1 histogram sample: the metrics
/// service has no gauge instrument.
fn emit_tinfoil_metrics(
    session: &TinfoilRouterSession,
    models: &[(String, String)],
    metrics: &dyn MetricsServiceTrait,
    last_auth: &mut u64,
) {
    let statuses: Vec<(String, Result<(), TinfoilVerifyError>)> = models
        .iter()
        .map(|(canonical, slug)| (canonical.clone(), session.model_status(slug).map(|_| ())))
        .collect();
    emit_tinfoil_samples(
        session.last_verify_error(),
        &statuses,
        session.upstream_auth_failures(),
        metrics,
        last_auth,
    );
}

/// The pure half of [`emit_tinfoil_metrics`], so every state can be tested.
fn emit_tinfoil_samples(
    router_error: Option<TinfoilVerifyError>,
    statuses: &[(String, Result<(), TinfoilVerifyError>)],
    auth_total: u64,
    metrics: &dyn MetricsServiceTrait,
    last_auth: &mut u64,
) {
    let count = |reason: &str| {
        let result = if reason == "none" { "ok" } else { "failed" };
        metrics.record_count(
            consts::METRIC_TINFOIL_VERIFICATION,
            1,
            &[&format!("result:{result}"), &format!("reason:{reason}")],
        );
    };
    // `model_status` reports `Fetch` for every slug while the router is not
    // verified, so that also covers "never verified yet" (no error recorded).
    let router_down = statuses
        .iter()
        .any(|(_, s)| matches!(s, Err(TinfoilVerifyError::Fetch)));
    match router_error {
        Some(e) => count(e.reason()),
        None if router_down => count(TinfoilVerifyError::Fetch.reason()),
        None => count("none"),
    }
    for (canonical, status) in statuses {
        // A closed model on a healthy router is a model-pin problem
        // (`unknown_model_measurement`) with its own reason; an unverified
        // router is already counted once above.
        if let Err(e) = status {
            if !router_down {
                count(e.reason());
            }
        }
        metrics.record_histogram(
            consts::METRIC_TINFOIL_AVAILABLE,
            if status.is_ok() { 1.0 } else { 0.0 },
            &[&format!("{}:{canonical}", consts::TAG_MODEL)],
        );
    }
    if auth_total > *last_auth {
        metrics.record_count(
            consts::METRIC_TINFOIL_UPSTREAM_AUTH_FAILURE,
            (auth_total - *last_auth) as i64,
            &[],
        );
        *last_auth = auth_total;
    }
}

fn spawn_tinfoil_metrics(
    session: Arc<TinfoilRouterSession>,
    models: Vec<(String, String)>,
    metrics: Arc<dyn MetricsServiceTrait>,
) -> tokio::task::JoinHandle<()> {
    let weak = Arc::downgrade(&session);
    drop(session);
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(tinfoil_provider::PROXY_REREAD);
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        let mut last_auth = 0u64;
        loop {
            tick.tick().await;
            // Providers hold the only other strong references; stop once gone.
            let Some(session) = weak.upgrade() else { break };
            emit_tinfoil_metrics(&session, &models, metrics.as_ref(), &mut last_auth);
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use config::parse_attested_3p_models;

    fn tinfoil_cfg(key: Option<&str>, models: &str) -> ExternalProvidersConfig {
        ExternalProvidersConfig {
            tinfoil_api_key: key.map(String::from),
            tinfoil_models: parse_attested_3p_models("TINFOIL_MODELS", models),
            ..Default::default()
        }
    }

    fn pins_with_router() -> TinfoilPins {
        TinfoilPins {
            router: vec![services::attestation::tinfoil_pins::RouterPin {
                measurement: "00".repeat(48),
                repo: "tinfoilsh/confidential-model-router".to_string(),
                tag: "v0.0.0-test".to_string(),
            }],
            models: Default::default(),
        }
    }

    #[test]
    fn tinfoil_preflight_skips_fail_closed() {
        let models = "m-tf=gpt-oss-120b@131072";
        assert_eq!(
            tinfoil_preflight(&tinfoil_cfg(Some("k"), ""), Ok(pins_with_router())).unwrap_err(),
            TinfoilSkip::NoModels
        );
        assert_eq!(
            tinfoil_preflight(&tinfoil_cfg(None, models), Ok(pins_with_router())).unwrap_err(),
            TinfoilSkip::NoKey
        );
        // Empty router pins (the compiled state on this branch) and a parse
        // failure both refuse to register.
        assert_eq!(
            tinfoil_preflight(&tinfoil_cfg(Some("k"), models), Ok(TinfoilPins::default()))
                .unwrap_err(),
            TinfoilSkip::PinsUnusable
        );
        assert_eq!(
            tinfoil_preflight(&tinfoil_cfg(Some("k"), models), Err("bad".into())).unwrap_err(),
            TinfoilSkip::PinsUnusable
        );
        let (key, _) =
            tinfoil_preflight(&tinfoil_cfg(Some("k"), models), Ok(pins_with_router())).unwrap();
        assert_eq!(key, "k");
    }

    #[test]
    fn tinfoil_entry_ctx_rules() {
        let e = |raw: &str| {
            parse_attested_3p_models("TINFOIL_MODELS", raw)
                .into_iter()
                .next()
                .unwrap()
        };
        assert_eq!(
            tinfoil_entry_ctx(&e("m=slug"), Some(131072)).unwrap_err(),
            TinfoilEntrySkip::MissingCtx
        );
        assert_eq!(
            tinfoil_entry_ctx(&e("m=slug@200000"), Some(131072)).unwrap_err(),
            TinfoilEntrySkip::ExceedsPublished {
                ctx: 200000,
                published: 131072
            }
        );
        assert_eq!(
            tinfoil_entry_ctx(&e("m=slug@131072"), Some(131072)),
            Ok(131072)
        );
        // Unknown published window (fetch failed): the declared value stands.
        assert_eq!(tinfoil_entry_ctx(&e("m=slug@200000"), None), Ok(200000));
    }

    #[test]
    fn unverified_tinfoil_session_reports_closed_and_fetch_error() {
        use services::metrics::capturing::{CapturingMetricsService, MetricValue};
        let verifier = Arc::new(services::attestation::tinfoil::TinfoilPolicyVerifier::new(
            TinfoilPins::default(),
        ));
        let session =
            TinfoilRouterSession::new(tinfoil_provider::Config::new("k".to_string(), 30), verifier)
                .unwrap();
        let metrics = CapturingMetricsService::new();
        let mut last_auth = 0u64;
        let models = vec![("m-tf".to_string(), "gpt-oss-120b".to_string())];
        emit_tinfoil_metrics(&session, &models, &metrics, &mut last_auth);
        let got = metrics.get_metrics();
        let avail = got
            .iter()
            .find(|m| m.name == consts::METRIC_TINFOIL_AVAILABLE)
            .expect("availability sample");
        assert!(matches!(avail.value, MetricValue::Histogram(v) if v == 0.0));
        assert_eq!(avail.tags, vec!["model:m-tf".to_string()]);
        let ver = got
            .iter()
            .find(|m| m.name == consts::METRIC_TINFOIL_VERIFICATION)
            .expect("verification sample");
        assert_eq!(
            ver.tags,
            vec![
                "result:failed".to_string(),
                "reason:fetch_error".to_string()
            ]
        );
        assert!(!got
            .iter()
            .any(|m| m.name == consts::METRIC_TINFOIL_UPSTREAM_AUTH_FAILURE));
    }

    fn counts(
        m: &services::metrics::capturing::CapturingMetricsService,
        name: &str,
    ) -> Vec<Vec<String>> {
        m.get_metrics()
            .into_iter()
            .filter(|r| r.name == name)
            .map(|r| r.tags)
            .collect()
    }

    #[test]
    fn verified_router_with_unpinned_model_counts_the_model_reason_once() {
        use services::metrics::capturing::CapturingMetricsService;
        let m = CapturingMetricsService::new();
        let mut last = 0u64;
        let statuses = vec![
            ("pinned".to_string(), Ok(())),
            (
                "unpinned".to_string(),
                Err(TinfoilVerifyError::UnknownModelMeasurement),
            ),
        ];
        emit_tinfoil_samples(None, &statuses, 0, &m, &mut last);
        let v = |r: &str, why: &str| vec![format!("result:{r}"), format!("reason:{why}")];
        assert_eq!(
            counts(&m, consts::METRIC_TINFOIL_VERIFICATION),
            vec![v("ok", "none"), v("failed", "unknown_model_measurement")],
            "router counted ok once; the model reason once; nothing doubled"
        );
        let avail: Vec<_> = m
            .get_metrics()
            .into_iter()
            .filter(|r| r.name == consts::METRIC_TINFOIL_AVAILABLE)
            .map(|r| (r.tags[0].clone(), format!("{:?}", r.value)))
            .collect();
        assert_eq!(avail.len(), 2);
        assert!(avail[0].0 == "model:pinned" && avail[0].1.contains("1.0"));
        assert!(avail[1].0 == "model:unpinned" && avail[1].1.contains("0.0"));
    }

    #[test]
    fn router_failure_is_counted_once_not_per_model() {
        use services::metrics::capturing::CapturingMetricsService;
        let m = CapturingMetricsService::new();
        let mut last = 0u64;
        let down = || Err(TinfoilVerifyError::Fetch);
        let statuses = vec![("a".to_string(), down()), ("b".to_string(), down())];
        emit_tinfoil_samples(
            Some(TinfoilVerifyError::UnknownRouterMeasurement),
            &statuses,
            0,
            &m,
            &mut last,
        );
        assert_eq!(
            counts(&m, consts::METRIC_TINFOIL_VERIFICATION),
            vec![vec![
                "result:failed".to_string(),
                "reason:unknown_router_measurement".to_string()
            ]]
        );
    }

    #[test]
    fn second_poll_emits_only_the_auth_failure_increment() {
        use services::metrics::capturing::{CapturingMetricsService, MetricValue};
        let m = CapturingMetricsService::new();
        let mut last = 0u64;
        emit_tinfoil_samples(None, &[], 2, &m, &mut last);
        assert_eq!(last, 2);
        assert_eq!(
            counts(&m, consts::METRIC_TINFOIL_VERIFICATION),
            vec![vec!["result:ok".to_string(), "reason:none".to_string()]]
        );
        assert_eq!(
            m.get_metrics().len(),
            2,
            "one verification + one auth delta"
        );
        // Same total: nothing for auth failures. Higher total: only the delta.
        emit_tinfoil_samples(None, &[], 2, &m, &mut last);
        emit_tinfoil_samples(None, &[], 5, &m, &mut last);
        let all = m.get_metrics();
        let auth: Vec<_> = all
            .iter()
            .filter(|r| r.name == consts::METRIC_TINFOIL_UPSTREAM_AUTH_FAILURE)
            .map(|r| match r.value {
                MetricValue::Count(n) => n,
                _ => -1,
            })
            .collect();
        assert_eq!(auth, vec![2, 3]);
        // Three polls: three verification samples, two auth deltas, nothing else.
        assert_eq!(all.len(), 5);
    }

    fn unverified_session() -> Arc<TinfoilRouterSession> {
        let verifier = Arc::new(services::attestation::tinfoil::TinfoilPolicyVerifier::new(
            TinfoilPins::default(),
        ));
        TinfoilRouterSession::new(tinfoil_provider::Config::new("k".to_string(), 30), verifier)
            .unwrap()
    }

    #[tokio::test(start_paused = true)]
    async fn metrics_task_emits_every_proxy_reread_and_exits_when_the_session_drops() {
        use services::metrics::capturing::CapturingMetricsService;
        let m = Arc::new(CapturingMetricsService::new());
        let session = unverified_session();
        let handle = spawn_tinfoil_metrics(
            session.clone(),
            vec![("m-tf".to_string(), "gpt-oss-120b".to_string())],
            m.clone(),
        );
        let per_poll = |m: &CapturingMetricsService| {
            (
                counts(m, consts::METRIC_TINFOIL_VERIFICATION).len(),
                counts(m, consts::METRIC_TINFOIL_AVAILABLE).len(),
            )
        };
        // The first poll is immediate.
        for _ in 0..5 {
            tokio::task::yield_now().await;
        }
        assert_eq!(per_poll(&m), (1, 1));
        // Nothing more until PROXY_REREAD elapses.
        tokio::time::advance(tinfoil_provider::PROXY_REREAD / 2).await;
        for _ in 0..5 {
            tokio::task::yield_now().await;
        }
        assert_eq!(per_poll(&m), (1, 1));
        tokio::time::advance(tinfoil_provider::PROXY_REREAD).await;
        for _ in 0..5 {
            tokio::task::yield_now().await;
        }
        assert_eq!(per_poll(&m), (2, 2));
        assert!(!handle.is_finished());

        // Last strong reference gone: the next tick ends the task, emitting nothing.
        drop(session);
        tokio::time::advance(tinfoil_provider::PROXY_REREAD).await;
        tokio::time::timeout(std::time::Duration::from_secs(1), handle)
            .await
            .expect("metrics task must exit once the session is dropped")
            .unwrap();
        assert_eq!(per_poll(&m), (2, 2));
    }
}
