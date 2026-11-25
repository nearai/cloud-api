//! Startup wiring for attested third-party providers (Chutes today, Tinfoil
//! next). Two-phase by design: `reserve_attested_3p` runs BEFORE any
//! external/discovery load (fail-closed id reservation), and
//! `register_attested_3p` runs AFTER the refresh task is started.

mod tinfoil;

use config::ExternalProvidersConfig;
use database::repositories::ModelRepository;
use inference_providers::ProviderSource;
use services::inference_provider_pool::{InferenceProviderPool, ProviderPoolRole};
use services::metrics::MetricsServiceTrait;
use std::sync::Arc;
use tinfoil::register_tinfoil;

pub use tinfoil::{register_tinfoil_models, register_tinfoil_with};

fn source_label(source: ProviderSource) -> &'static str {
    match source {
        ProviderSource::Chutes => "Chutes",
        ProviderSource::Tinfoil => "Tinfoil",
        ProviderSource::Vllm => "NEAR vLLM",
        ProviderSource::External => "external",
    }
}

/// Phase 1 (MUST run before external/discovery loads): fail-closed reservation.
///
/// Reserves EVERY configured canonical id as a pinned (verifiable) model up
/// front — even before we try to build the providers. This guarantees a
/// plaintext external/OpenRouter row sharing a canonical id can never register
/// for it, even if the provider fails to build (missing key / construction
/// error). A reserved id then serves only its attested provider(s) or fails
/// closed (404). Chutes ids are reserved under `enable_chutes`; Tinfoil ids
/// whenever `tinfoil_models` is non-empty (key or no key).
pub(crate) fn reserve_attested_3p(
    pool: &Arc<InferenceProviderPool>,
    cfg: &ExternalProvidersConfig,
) {
    if cfg.enable_chutes {
        reserve_source(pool, ProviderSource::Chutes, &cfg.chutes_models);
    }
    reserve_source(pool, ProviderSource::Tinfoil, &cfg.tinfoil_models);
}

/// Reserve one source's canonical ids as pinned (verifiable) models. The
/// single place the fail-closed reservation is done, so sources can't drift.
fn reserve_source(
    pool: &Arc<InferenceProviderPool>,
    source: ProviderSource,
    entries: &[config::AttestedThirdPartyModelEntry],
) {
    let canonical_ids: Vec<String> = entries.iter().map(|e| e.canonical_id.clone()).collect();
    if !canonical_ids.is_empty() {
        pool.reserve_pinned_models(&canonical_ids);
        tracing::info!(
            count = canonical_ids.len(),
            "Reserved {} canonical ids as verifiable (fail-closed) before external load",
            source_label(source)
        );
    }
}

/// Phase 2 (after `start_refresh_task`): build and register providers.
/// Chutes first, then Tinfoil (fixes backup tie order).
pub(crate) async fn register_attested_3p(
    pool: &Arc<InferenceProviderPool>,
    models_repo: &ModelRepository,
    cfg: &ExternalProvidersConfig,
    tinfoil_trust: services::attestation::tinfoil_trust::TinfoilTrustHandle,
    metrics: Arc<dyn MetricsServiceTrait>,
) {
    let chutes_registered = register_chutes(pool, models_repo, cfg).await;
    register_tinfoil(pool, models_repo, cfg, tinfoil_trust, metrics).await;
    tracing::debug!(chutes_registered, "Attested 3P registration finished");
}

/// Chutes attested provider — hard-off by default (`ENABLE_CHUTES`). Each model
/// is served over a verified ML-KEM E2EE channel: every request attests the
/// chosen instance (TDX quote + report_data bindings + register-pinned
/// measurement + GPU) before encapsulating, so an unverified backend can never
/// serve a Chutes response. Registration is gated on the flag + an API key +
/// at least one model id.
async fn register_chutes(
    pool: &Arc<InferenceProviderPool>,
    models_repo: &ModelRepository,
    cfg: &ExternalProvidersConfig,
) -> usize {
    let mut registered = 0usize;
    if cfg.enable_chutes {
        match &cfg.chutes_api_key {
            Some(api_key) if !cfg.chutes_models.is_empty() => {
                // The pins are compiled in and CI checks they parse; if a bad
                // build ever slips through, Chutes stays off and the rest of the
                // API still starts.
                match services::attestation::chutes::vetted_golden_measurements() {
                    Err(e) => tracing::error!(
                        error = %e,
                        "Chutes golden measurements failed to parse; not registering any Chutes provider"
                    ),
                    Ok(policy) => {
                        let pccs_url = cfg.pccs_url.clone();
                        let allow_streaming = cfg.chutes_enable_streaming;
                        let verifier: Arc<
                            dyn inference_providers::attested::chutes::verifier_port::ChutesInstanceVerifier,
                        > = Arc::new(services::attestation::chutes::ChutesBackendVerifier::new(
                            policy,
                            pccs_url,
                        ));
                        for entry in &cfg.chutes_models {
                            if entry.max_context_tokens.is_none() {
                                tracing::warn!(
                                    canonical = %entry.canonical_id,
                                    "CHUTES_MODELS entry has no @ctx; it sorts last among fitting backups"
                                );
                            }
                            // The provider talks to Chutes with the chute SLUG (request_body
                            // pins it + cached_chute_id resolves it); we expose/route under
                            // the CANONICAL id (the NEAR-served id when NEAR also serves the
                            // model, else the OpenRouter id) — never the raw `-TEE` slug.
                            let pcfg = inference_providers::attested::chutes::Config::new(
                                api_key.clone(),
                                entry.upstream_id.clone(),
                                cfg.timeout_seconds,
                            )
                            .with_canonical_id(entry.canonical_id.clone())
                            .with_streaming(allow_streaming);
                            match inference_providers::attested::chutes::Provider::new(
                                pcfg,
                                verifier.clone(),
                            ) {
                                Ok(provider) => {
                                    // The catalog row under the canonical id must already
                                    // exist (created through cloud-platform) so the data plane
                                    // resolves the model and usage bills against a real id. If
                                    // NEAR already serves this id, its row is left untouched
                                    // and we just add Chutes as a fallback provider.
                                    let Some(role) = ensure_attested_3p_catalog_row(
                                        models_repo,
                                        ProviderSource::Chutes,
                                        &entry.canonical_id,
                                    )
                                    .await
                                    else {
                                        // `None` = no catalog row: skip registration entirely.
                                        continue;
                                    };
                                    // Register the stable role from catalog configuration,
                                    // independently of whether discovery happened to find a live
                                    // primary during this startup.
                                    pool.register_pinned_provider(
                                        entry.canonical_id.clone(),
                                        Arc::new(provider),
                                        entry.max_context_tokens,
                                        role,
                                    )
                                    .await;
                                    registered += 1;
                                    tracing::info!(
                                        canonical = %entry.canonical_id,
                                        chute_slug = %entry.upstream_id,
                                        role = ?role,
                                        "Registered Chutes attested provider"
                                    );
                                }
                                Err(e) => {
                                    tracing::warn!(
                                        canonical = %entry.canonical_id,
                                        chute_slug = %entry.upstream_id,
                                        error = %e,
                                        "Failed to build Chutes provider"
                                    );
                                }
                            }
                        }
                    }
                }
            }
            _ => {
                tracing::warn!(
                    "ENABLE_CHUTES is set but CHUTES_API_KEY or CHUTES_MODELS is missing; \
                     not registering any Chutes provider"
                );
            }
        }
    } else if !cfg.chutes_models.is_empty() {
        // Flag off but models still listed: any *active* catalog row left over
        // from a previous run would resolve to a model with no registered provider
        // (per-request provider errors, not a clean 404). Warn so an operator
        // notices and deactivates those rows (PATCH is_active=false).
        tracing::warn!(
            models = ?cfg.chutes_models,
            "ENABLE_CHUTES is off but CHUTES_MODELS is set; if any of these have an active \
             catalog row, requests will surface provider errors — deactivate them via \
             the admin API's PATCH /v1/admin/models or re-enable ENABLE_CHUTES"
        );
    }
    registered
}

/// Look up an attested model's catalog row in the `models` table and derive
/// its provider role.
///
/// The data plane rejects any model without an active `models` row *before*
/// reaching the provider pool (`resolve_and_get_model` in completions), so a
/// pinned provider registered purely in-memory would 404 every request, and
/// usage rows carry a `FOREIGN KEY (model_id) REFERENCES models(id)`: the row
/// must genuinely exist. cloud-api never creates it. If it is missing, nothing
/// is written and `None` is returned (the model is not registered); the row is
/// created through cloud-platform.
pub async fn ensure_attested_3p_catalog_row(
    models_repo: &ModelRepository,
    source: ProviderSource,
    model_name: &str,
) -> Option<ProviderPoolRole> {
    let label = source_label(source);

    let role_for_existing = |model: &database::models::Model| {
        // A catalog row owned by another provider configuration means this
        // out-of-band provider is a fallback even if the configured primary is
        // temporarily undiscoverable. Rows created for this provider itself are
        // genuine standalone primaries.
        ProviderPoolRole::from_catalog(source, &model.provider_type, model.inference_url.is_some())
    };

    // Use the *unfiltered* lookup (not get_active_model_by_name): a deliberately
    // disabled row (is_active=false) must be respected.
    match models_repo.get_by_internal_name(model_name).await {
        Ok(Some(existing)) => {
            let role = role_for_existing(&existing);
            // Already in the catalog — respect operator configuration verbatim.
            // Surface a warning if the metadata contradicts attested serving so
            // a misconfigured row (e.g. attestation_supported=false) is visible.
            if !existing.is_active {
                tracing::warn!(
                    model = %model_name,
                    "{label} model has a DISABLED catalog row (is_active=false); requests will \
                     404 by design — re-enable via the admin API's PATCH /v1/admin/models if that's unintended"
                );
            } else if !existing.attestation_supported {
                tracing::warn!(
                    model = %model_name,
                    "{label} model has an existing catalog row with attestation_supported=false; \
                     E2EE/signature handling may misbehave — fix via the admin API's PATCH /v1/admin/models"
                );
            } else if existing.supported_sampling_parameters.is_empty()
                && existing.supported_features.is_empty()
            {
                // This is the exact #781 (M1) bug state on a pre-existing row: both
                // capability arrays are still the empty V0051 default, so
                // `GET /v1/models` advertises the model as supporting *nothing* and
                // OpenRouter-style routers won't route tool calls to it. Rows are
                // backfilled by migration V0060. Warn in case a row predates the migration or was cleared.
                tracing::warn!(
                    model = %model_name,
                    "{label} model has an existing catalog row with EMPTY supported_features \
                     and supported_sampling_parameters — OpenRouter-style routers will refuse \
                     to route tool calls to it; backfilled by migration V0060, or set via \
                     the admin API's PATCH /v1/admin/models"
                );
            } else {
                tracing::info!(model = %model_name, "{label} model already in catalog");
            }
            Some(role)
        }
        Ok(None) => {
            tracing::warn!(
                model = %model_name,
                "No catalog row for attested model; not registering (create the row through cloud-platform)"
            );
            None
        }
        Err(e) => {
            tracing::warn!(
                model = %model_name, error = %e,
                "Could not check catalog for {label} model"
            );
            // Unknown catalog state is fail-closed for organizations that
            // disable fallback. A later restart/registration can establish a
            // standalone primary role once the catalog is readable again.
            Some(ProviderPoolRole::Fallback)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use config::parse_attested_3p_models;

    #[tokio::test]
    async fn tinfoil_ids_reserved_without_key() {
        let cfg = ExternalProvidersConfig {
            openai_api_key: Some("sk-test-key".to_string()),
            timeout_seconds: 60,
            refresh_interval_secs: 0,
            tinfoil_models: parse_attested_3p_models("TINFOIL_MODELS", "m-tf=glm-5-3@1048576"),
            ..Default::default()
        };
        let pool = Arc::new(InferenceProviderPool::new(None, cfg.clone()));
        reserve_attested_3p(&pool, &cfg);
        // A plaintext external DB row for the same id must be skipped.
        let _ = pool
            .load_external_providers(vec![(
                "m-tf".to_string(),
                serde_json::json!({"backend": "openai_compatible", "base_url": "https://example.invalid",
                    "api_key": "sk-test-key"}),
            )])
            .await;
        assert!(!pool.has_provider("m-tf").await);
    }

    #[tokio::test]
    async fn chutes_ids_not_reserved_when_disabled() {
        let cfg = ExternalProvidersConfig {
            openai_api_key: Some("sk-test-key".to_string()),
            timeout_seconds: 60,
            refresh_interval_secs: 0,
            enable_chutes: false,
            chutes_models: parse_attested_3p_models("CHUTES_MODELS", "m-ch=slug"),
            ..Default::default()
        };
        let pool = Arc::new(InferenceProviderPool::new(None, cfg.clone()));
        reserve_attested_3p(&pool, &cfg);
        let _ = pool
            .load_external_providers(vec![(
                "m-ch".to_string(),
                serde_json::json!({"backend": "openai_compatible", "base_url": "https://example.invalid",
                    "api_key": "sk-test-key"}),
            )])
            .await;
        assert!(pool.has_provider("m-ch").await);
    }
    // --- settings load before consumers register ---

    use inference_providers::attested::tinfoil::verifier_port::{
        AtcBundle, PinSource, TinfoilVerifier, TinfoilVerifyError,
    };
    use services::admin_settings::{AdminSettingsRepository, StoredSetting};
    use services::attestation::tinfoil::TinfoilPolicyVerifier;
    use services::attestation::tinfoil_pins::TinfoilPins;

    /// A settings store: rows are served as stored, or every read fails.
    struct FakeRepo {
        rows: Vec<StoredSetting>,
        fail: bool,
    }

    #[async_trait::async_trait]
    impl AdminSettingsRepository for FakeRepo {
        async fn get_all(&self) -> anyhow::Result<Vec<StoredSetting>> {
            if self.fail {
                anyhow::bail!("settings store is down");
            }
            Ok(self.rows.clone())
        }
        async fn get(&self, key: &str) -> anyhow::Result<Option<StoredSetting>> {
            Ok(self.get_all().await?.into_iter().find(|r| r.key == key))
        }
        async fn upsert(
            &self,
            _key: &str,
            _value: serde_json::Value,
            _by_user: uuid::Uuid,
        ) -> anyhow::Result<StoredSetting> {
            anyhow::bail!("read-only fake")
        }
    }

    fn router_bundle() -> AtcBundle {
        serde_json::from_str(include_str!(
            "../../services/src/attestation/testdata/tinfoil/atc_bundle.json"
        ))
        .unwrap()
    }

    /// Runs the production startup helper against `repo`, then verifies the
    /// recorded router (no compiled router pin) through a verifier that shares
    /// the handle, as `register_tinfoil_with` wires it.
    async fn first_verify_with(repo: FakeRepo) -> Result<PinSource, TinfoilVerifyError> {
        let fail = repo.fail;
        let pool = InferenceProviderPool::new(None, ExternalProvidersConfig::default());
        let trust = crate::new_tinfoil_trust_handle();
        let settings = crate::build_admin_settings(Arc::new(repo), &pool, trust.clone()).await;
        let verifier = TinfoilPolicyVerifier::new(TinfoilPins::default(), trust);
        if !fail {
            let keys: Vec<_> = settings
                .get_all()
                .await
                .unwrap()
                .into_iter()
                .map(|v| v.key)
                .collect();
            assert_eq!(
                keys,
                [
                    "placement",
                    "attested_3p",
                    "tinfoil_trust",
                    "usage_admission"
                ]
            );
        }
        verifier.verify_router(&router_bundle()).map(|r| r.source)
    }

    fn stored(key: &str, value: serde_json::Value) -> StoredSetting {
        StoredSetting {
            key: key.to_string(),
            value,
            updated_by_user_id: None,
            updated_at: jiff::Timestamp::now(),
        }
    }

    #[tokio::test]
    async fn stored_router_sigstore_false_is_honoured_by_first_verify() {
        let repo = FakeRepo {
            rows: vec![stored(
                "tinfoil_trust",
                serde_json::json!({"router_sigstore": false}),
            )],
            fail: false,
        };
        assert_eq!(
            first_verify_with(repo).await.unwrap_err(),
            TinfoilVerifyError::UnknownRouterMeasurement
        );
    }

    /// A settings store whose reads can be switched on after startup.
    struct FlakyRepo {
        up: Arc<std::sync::atomic::AtomicBool>,
    }

    #[async_trait::async_trait]
    impl AdminSettingsRepository for FlakyRepo {
        async fn get_all(&self) -> anyhow::Result<Vec<StoredSetting>> {
            if self.up.load(std::sync::atomic::Ordering::SeqCst) {
                Ok(vec![])
            } else {
                anyhow::bail!("settings store is down")
            }
        }
        async fn get(&self, key: &str) -> anyhow::Result<Option<StoredSetting>> {
            Ok(self.get_all().await?.into_iter().find(|r| r.key == key))
        }
        async fn upsert(
            &self,
            _key: &str,
            _value: serde_json::Value,
            _by_user: uuid::Uuid,
        ) -> anyhow::Result<StoredSetting> {
            anyhow::bail!("read-only fake")
        }
    }

    #[tokio::test(start_paused = true)]
    async fn settings_store_down_at_startup_fails_closed_then_recovers() {
        let up = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let pool = InferenceProviderPool::new(None, ExternalProvidersConfig::default());
        let trust = crate::new_tinfoil_trust_handle();
        let _settings = crate::build_admin_settings(
            Arc::new(FlakyRepo { up: up.clone() }),
            &pool,
            trust.clone(),
        )
        .await;
        let verifier = TinfoilPolicyVerifier::new(TinfoilPins::default(), trust.clone());
        // Store down: no panic, and the Sigstore path stays off.
        assert_eq!(
            verifier.verify_router(&router_bundle()).unwrap_err(),
            TinfoilVerifyError::UnknownRouterMeasurement
        );
        assert_eq!(trust.load().version, 0);

        // The store recovers; the startup retry loads it within 30 s.
        up.store(true, std::sync::atomic::Ordering::SeqCst);
        tokio::time::sleep(
            services::admin_settings::INITIAL_LOAD_RETRY + std::time::Duration::from_secs(1),
        )
        .await;
        assert_eq!(trust.load().version, 1);
        assert!(trust.load().value.router_sigstore);
        assert_eq!(
            verifier.verify_router(&router_bundle()).unwrap().source,
            PinSource::SigstoreAtc
        );
    }

    /// Drives `register_tinfoil_with` against a settings store and returns the
    /// verifier it built (the session constructor refuses, so nothing is
    /// registered and nothing touches the network or the database).
    async fn verifier_from_register(repo: FakeRepo) -> Arc<TinfoilPolicyVerifier> {
        let pool = Arc::new(InferenceProviderPool::new(
            None,
            ExternalProvidersConfig::default(),
        ));
        let trust = crate::new_tinfoil_trust_handle();
        let _settings = crate::build_admin_settings(Arc::new(repo), &pool, trust.clone()).await;
        let cfg = ExternalProvidersConfig {
            tinfoil_api_key: Some("k".to_string()),
            tinfoil_models: parse_attested_3p_models("TINFOIL_MODELS", "m-tf=glm-5-3@1048576"),
            timeout_seconds: 5,
            ..Default::default()
        };
        let models_repo =
            database::repositories::ModelRepository::new(database::pool::DbPool::uninitialized());
        let captured = std::sync::Mutex::new(None);
        register_tinfoil_with(
            &pool,
            &models_repo,
            &cfg,
            Ok(TinfoilPins::default()),
            trust,
            |_, verifier| {
                *captured.lock().unwrap() = Some(verifier);
                Err("no session in unit tests".to_string())
            },
            Arc::new(services::metrics::MockMetricsService),
        )
        .await;
        let verifier = captured.lock().unwrap().take();
        verifier.expect("build_session was called")
    }

    #[tokio::test]
    async fn register_tinfoil_with_uses_preloaded_trust_handle() {
        let off = FakeRepo {
            rows: vec![stored(
                "tinfoil_trust",
                serde_json::json!({"router_sigstore": false}),
            )],
            fail: false,
        };
        let verifier = verifier_from_register(off).await;
        assert_eq!(
            verifier.verify_router(&router_bundle()).unwrap_err(),
            TinfoilVerifyError::UnknownRouterMeasurement
        );

        let unseeded = FakeRepo {
            rows: vec![],
            fail: false,
        };
        let verifier = verifier_from_register(unseeded).await;
        assert_eq!(
            verifier.verify_router(&router_bundle()).unwrap().source,
            PinSource::SigstoreAtc
        );
    }
}
