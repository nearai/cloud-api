//! Startup wiring for attested third-party providers (Chutes today, Tinfoil
//! next). Two-phase by design: `reserve_attested_3p` runs BEFORE any
//! external/discovery load (fail-closed id reservation), and
//! `register_attested_3p` runs AFTER the refresh task is started.

use config::ExternalProvidersConfig;
use database::repositories::ModelRepository;
use inference_providers::ProviderSource;
use services::inference_provider_pool::{InferenceProviderPool, ProviderPoolRole};
use std::sync::Arc;

/// Standard OpenAI sampling knobs Chutes (sglang) honors, expressed in
/// OpenRouter's fixed `supported_sampling_parameters` vocabulary. Seeded onto
/// every auto-created Chutes catalog row so `GET /v1/models` advertises real
/// capabilities instead of an empty list (which silently disables routing for
/// OpenRouter-style consumers). `n` is intentionally omitted: it is not part of
/// OpenRouter's vocabulary. Must remain a subset of `routes::admin::VALID_SAMPLING_PARAMS`.
pub(crate) const CHUTES_SUPPORTED_SAMPLING_PARAMS: &[&str] = &[
    "temperature",
    "top_p",
    "frequency_penalty",
    "presence_penalty",
    "stop",
    "seed",
    "max_tokens",
];

/// Feature capabilities Chutes (sglang) exposes, in OpenRouter's fixed
/// `supported_features` vocabulary: `tools` => tool/function-calling, `json_mode`
/// => JSON `response_format`. Streaming is always supported but is not a member
/// of OpenRouter's feature vocabulary, so it is not advertised here. Must remain
/// a subset of `routes::admin::VALID_FEATURES`.
///
/// `tools` is a *default* assumption, not a universal guarantee: tool-calling in
/// sglang is model-family specific (needs a compatible chat template + tool-call
/// parser), so a family without it would be over-advertised here — the inverse of
/// the empty-array bug. That risk is bounded because the seed lands INACTIVE: an
/// operator must PATCH the row (and is warned to verify tool support, clearing
/// `supported_features` if absent) before any traffic is served.
pub(crate) const CHUTES_SUPPORTED_FEATURES: &[&str] = &["tools", "json_mode"];

/// Catalog-row seed for an attested provider's auto-created model row.
pub struct CatalogSeed {
    pub description: &'static str,
    pub supported_features: &'static [&'static str],
    pub supported_sampling_parameters: &'static [&'static str],
}

pub(crate) const CHUTES_SEED: CatalogSeed = CatalogSeed {
    description: "Attested model served via Chutes TEE (verified end-to-end by NEAR AI).",
    supported_features: CHUTES_SUPPORTED_FEATURES,
    supported_sampling_parameters: CHUTES_SUPPORTED_SAMPLING_PARAMS,
};

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
        let canonical_ids: Vec<String> = cfg
            .chutes_models
            .iter()
            .map(|e| e.canonical_id.clone())
            .collect();
        if !canonical_ids.is_empty() {
            pool.reserve_pinned_models(&canonical_ids);
            tracing::info!(
                count = canonical_ids.len(),
                "Reserved Chutes canonical ids as verifiable (fail-closed) before external load"
            );
        }
    }
    let tinfoil_ids: Vec<String> = cfg
        .tinfoil_models
        .iter()
        .map(|e| e.canonical_id.clone())
        .collect();
    if !tinfoil_ids.is_empty() {
        pool.reserve_pinned_models(&tinfoil_ids);
        tracing::info!(
            count = tinfoil_ids.len(),
            "Reserved Tinfoil canonical ids as verifiable (fail-closed) before external load"
        );
    }
}

/// Phase 2 (after `start_refresh_task`): build and register providers.
/// Chutes first, then Tinfoil (fixes backup tie order).
pub(crate) async fn register_attested_3p(
    pool: &Arc<InferenceProviderPool>,
    models_repo: &ModelRepository,
    cfg: &ExternalProvidersConfig,
) {
    let chutes_registered = register_chutes(pool, models_repo, cfg).await;
    register_tinfoil(cfg);
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
                                    // Ensure a catalog row exists under the canonical id so the
                                    // data plane resolves the model (and usage bills against a
                                    // real id). If NEAR already serves this id, its row is left
                                    // untouched and we just add Chutes as a fallback provider.
                                    let role = ensure_attested_3p_catalog_row(
                                        models_repo,
                                        ProviderSource::Chutes,
                                        &entry.canonical_id,
                                        Some(&CHUTES_SEED),
                                    )
                                    .await
                                    .unwrap_or(ProviderPoolRole::Fallback);
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
             PATCH /v1/admin/models or re-enable ENABLE_CHUTES"
        );
    }
    registered
}

/// Tinfoil provider registration is added in a follow-up PR; ids stay
/// reserved (fail-closed).
fn register_tinfoil(cfg: &ExternalProvidersConfig) {
    if !cfg.tinfoil_models.is_empty() {
        tracing::warn!(
            count = cfg.tinfoil_models.len(),
            "TINFOIL_MODELS is set but this build has no Tinfoil provider; ids stay reserved (fail-closed)"
        );
    }
}

/// Ensure an attested model has a catalog row in the `models` table.
///
/// The data plane rejects any model without an active `models` row *before*
/// reaching the provider pool (`resolve_and_get_model` in completions), so a
/// pinned provider registered purely in-memory would 404 every request. Worse,
/// usage rows carry a `FOREIGN KEY (model_id) REFERENCES models(id)` — a
/// synthesized id can't be billed — so the row must genuinely exist.
///
/// Idempotent and non-clobbering: if a row already exists (operator pre-seeded
/// it with real pricing/metadata via the admin API) we leave it untouched. We
/// only INSERT when missing (and a `seed` is given), INACTIVE with **zero
/// pricing** — the operator must set real per-token rates via
/// `PATCH /v1/admin/models` before serving paid traffic, which we warn about.
///
/// With `seed: None` and no existing row, nothing is seeded and `None` is
/// returned (3P-only models are out of scope).
pub async fn ensure_attested_3p_catalog_row(
    models_repo: &ModelRepository,
    source: ProviderSource,
    model_name: &str,
    seed: Option<&CatalogSeed>,
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
    // disabled row (is_active=false) must be respected, not silently re-activated
    // and clobbered by the seed path below.
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
                     404 by design — re-enable via PATCH /v1/admin/models if that's unintended"
                );
            } else if !existing.attestation_supported {
                tracing::warn!(
                    model = %model_name,
                    "{label} model has an existing catalog row with attestation_supported=false; \
                     E2EE/signature handling may misbehave — fix via PATCH /v1/admin/models"
                );
            } else if existing.supported_sampling_parameters.is_empty()
                && existing.supported_features.is_empty()
            {
                // This is the exact #781 (M1) bug state on a pre-existing row: both
                // capability arrays are still the empty V0051 default, so
                // `GET /v1/models` advertises the model as supporting *nothing* and
                // OpenRouter-style routers won't route tool calls to it. New rows are
                // seeded non-empty below; existing rows are backfilled by migration
                // V0060. Warn in case a row predates the migration or was cleared.
                tracing::warn!(
                    model = %model_name,
                    "{label} model has an existing catalog row with EMPTY supported_features \
                     and supported_sampling_parameters — OpenRouter-style routers will refuse \
                     to route tool calls to it; backfilled by migration V0060, or set via \
                     PATCH /v1/admin/models"
                );
            } else {
                tracing::info!(model = %model_name, "{label} model already in catalog");
            }
            Some(role)
        }
        Ok(None) => {
            let Some(seed) = seed else {
                tracing::warn!(
                    model = %model_name,
                    "No catalog row for attested fallback; not registering (out of scope: 3P-only models)"
                );
                return None;
            };
            // Friendly display name = last path segment; owner = leading segment.
            let display_name = model_name.rsplit('/').next().unwrap_or(model_name);
            let owned_by = model_name.split('/').next().unwrap_or(source.as_str());
            let req = database::models::UpdateModelPricingRequest {
                model_display_name: Some(display_name.to_string()),
                model_description: Some(seed.description.to_string()),
                // Generous default; operator should set the model's real context
                // window via the admin API. Not enforced as a hard reject here.
                context_length: Some(128_000),
                verifiable: Some(true),
                attestation_supported: Some(true),
                // Seed INACTIVE. `is_active=false` is the only field that actually
                // gates serving (`resolve_and_get_model` filters `WHERE is_active`;
                // `is_ready` is pure display metadata and does NOT gate). Seeding
                // inactive makes it *impossible* to serve — and therefore bill at
                // the zero default pricing — until an operator explicitly sets real
                // per-token rates AND flips is_active=true (one admin PATCH). This
                // closes the unpriced-serving window rather than merely warning.
                is_active: Some(false),
                is_ready: Some(Some(false)),
                provider_type: Some(source.as_str().to_string()),
                owned_by: Some(owned_by.to_string()),
                input_modalities: Some(vec!["text".to_string()]),
                output_modalities: Some(vec!["text".to_string()]),
                // OpenRouter-style routers gate tool/function-calling on these two
                // arrays; leaving them empty (the SQL default) silently advertises a
                // model as supporting *nothing*, so routers refuse to route tool
                // calls to it. Operators can still override via PATCH
                // /v1/admin/models. Both lists are restricted to OpenRouter's fixed
                // vocabulary (asserted by a unit test) so the seeded row would pass
                // the same admin write-path validation.
                supported_sampling_parameters: Some(
                    seed.supported_sampling_parameters
                        .iter()
                        .copied()
                        .map(String::from)
                        .collect(),
                ),
                supported_features: Some(
                    seed.supported_features
                        .iter()
                        .copied()
                        .map(String::from)
                        .collect(),
                ),
                // Pricing left None -> defaults to 0 on INSERT. The inactive seed
                // above prevents this zero price from ever being charged.
                ..Default::default()
            };
            // INSERT ... ON CONFLICT DO NOTHING: if an operator created/activated
            // the row concurrently with startup, their row wins and is left
            // untouched (no clobbering is_active/pricing back to the seed defaults).
            match models_repo.seed_model_if_absent(model_name, &req).await {
                Ok(Some(_)) => {
                    tracing::warn!(
                        model = %model_name,
                        "Seeded {label} catalog row as INACTIVE with zero pricing — set real \
                         per-token rates AND is_active=true via PATCH /v1/admin/models to serve \
                         (kept inactive so paid traffic can't be billed at $0). The seed \
                         advertises `tools`/`json_mode`: verify this model family actually \
                         supports tool-calling (per-family parser + compatible chat \
                         template) before activating, and clear `supported_features` via the \
                         same PATCH if it doesn't"
                    );
                    Some(ProviderPoolRole::Primary)
                }
                Ok(None) => {
                    tracing::info!(
                        model = %model_name,
                        "{label} catalog row already present (created concurrently); left untouched"
                    );
                    match models_repo.get_by_internal_name(model_name).await {
                        Ok(Some(existing)) => Some(role_for_existing(&existing)),
                        _ => Some(ProviderPoolRole::Fallback),
                    }
                }
                Err(e) => {
                    tracing::error!(
                        model = %model_name, error = %e,
                        "Failed to seed {label} catalog row; requests for this model will 404 \
                         until a row exists (create it via PATCH /v1/admin/models)"
                    );
                    Some(ProviderPoolRole::Fallback)
                }
            }
        }
        Err(e) => {
            tracing::warn!(
                model = %model_name, error = %e,
                "Could not check catalog for {label} model; skipping auto-seed"
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
                serde_json::json!({"backend": "openai_compatible", "base_url": "https://example.invalid"}),
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
                serde_json::json!({"backend": "openai_compatible", "base_url": "https://example.invalid"}),
            )])
            .await;
        assert!(pool.has_provider("m-ch").await);
    }

    #[test]
    fn chutes_seed_advertises_nonempty_capabilities() {
        assert!(!CHUTES_SEED.supported_sampling_parameters.is_empty());
        assert!(CHUTES_SEED.supported_features.contains(&"tools"));
    }
}
