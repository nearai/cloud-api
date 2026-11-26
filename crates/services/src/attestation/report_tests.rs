//! Parameter handling for `get_attestation_report`: `signing_algo` is
//! normalized once and that value is what reaches the model backend, and an
//! unknown model is a client error rather than a provider failure. The
//! `MockProvider` backend rejects non-lowercase algorithms the same way
//! inference-proxy does.

use std::{sync::Arc, time::Duration};

use async_trait::async_trait;
use config::ExternalProvidersConfig;
use inference_providers::mock::MockProvider;
use uuid::Uuid;

use super::{
    chat_signature_lifecycle_tests::{lifecycle_service, RecordingRepository},
    models::DstackCpuQuote,
    ports::AttestationServiceTrait,
    AttestationError, AttestationService, GatewayQuoteCollector, GatewayQuoteInput,
};
use crate::{
    inference_provider_pool::InferenceProviderPool,
    models::{ModelCaller, ModelWithPricing, ModelsRepository},
};

const MODEL: &str = "test-org/attested-model";
const NONCE: &str = "00112233445566778899aabbccddeeff00112233445566778899aabbccddeeff";
/// Mock report `signing_public_key` for an explicit `ed25519` request.
const MOCK_ED25519_PUBLIC_KEY: &str =
    "fedcba9876543210fedcba9876543210fedcba9876543210fedcba9876543210";

/// Echoes the requested algorithm like the dstack collector, without a socket.
struct EchoGatewayQuoteCollector;

#[async_trait]
impl GatewayQuoteCollector for EchoGatewayQuoteCollector {
    async fn collect_gateway_quote(
        &self,
        input: GatewayQuoteInput,
    ) -> Result<DstackCpuQuote, AttestationError> {
        Ok(DstackCpuQuote {
            signing_address: input.signing_address,
            signing_algo: input.signing_algo,
            intel_quote: "00".to_string(),
            event_log: "[]".to_string(),
            report_data: hex::encode(input.report_data),
            request_nonce: input.request_nonce,
            info: serde_json::json!({}),
            vpc: input.vpc,
            tls_cert_fingerprint: input.tls_cert_fingerprint,
        })
    }
}

/// Catalog holding only `MODEL`; any other identifier is unknown.
struct SingleModelRepository;

#[async_trait]
impl ModelsRepository for SingleModelRepository {
    async fn get_all_active_models(&self) -> anyhow::Result<Vec<ModelWithPricing>> {
        Ok(vec![catalog_model()])
    }

    async fn get_model_by_name(
        &self,
        model_name: &str,
    ) -> anyhow::Result<Option<ModelWithPricing>> {
        Ok((model_name == MODEL).then(catalog_model))
    }

    async fn resolve_and_get_model(
        &self,
        identifier: &str,
    ) -> anyhow::Result<Option<ModelWithPricing>> {
        Ok((identifier == MODEL).then(catalog_model))
    }

    async fn get_configured_model_names(&self) -> anyhow::Result<Vec<String>> {
        Ok(vec![MODEL.to_string()])
    }
}

fn catalog_model() -> ModelWithPricing {
    ModelWithPricing {
        upcoming_pricing: None,
        id: Uuid::nil(),
        model_name: MODEL.to_string(),
        model_display_name: "Attested".to_string(),
        model_description: String::new(),
        model_icon: None,
        input_cost_per_token: 0,
        output_cost_per_token: 0,
        cost_per_image: 0,
        cache_read_cost_per_token: None,
        text_pricing: None,
        context_length: 4096,
        verifiable: true,
        aliases: Vec::new(),
        owned_by: "test".to_string(),
        provider_type: "vllm".to_string(),
        provider_config: None,
        attestation_supported: true,
        input_modalities: None,
        output_modalities: None,
        inference_url: None,
        hugging_face_id: None,
        quantization: None,
        max_output_length: None,
        supported_sampling_parameters: Vec::new(),
        supported_features: Vec::new(),
        datacenters: None,
        is_ready: None,
        deprecation_date: None,
        successor_model_name: None,
        openrouter_slug: None,
        is_listed: true,
        created_at: jiff::Timestamp::now(),
    }
}

/// Catalog holding `MODEL` as an unlisted model that one organization holds a
/// grant for.
struct RestrictedModelRepository {
    granted_organization: Uuid,
}

#[async_trait]
impl ModelsRepository for RestrictedModelRepository {
    async fn get_all_active_models(&self) -> anyhow::Result<Vec<ModelWithPricing>> {
        Ok(vec![unlisted_catalog_model()])
    }

    async fn get_model_by_name(
        &self,
        model_name: &str,
    ) -> anyhow::Result<Option<ModelWithPricing>> {
        Ok((model_name == MODEL).then(unlisted_catalog_model))
    }

    async fn resolve_and_get_model(
        &self,
        identifier: &str,
    ) -> anyhow::Result<Option<ModelWithPricing>> {
        Ok((identifier == MODEL).then(unlisted_catalog_model))
    }

    async fn get_configured_model_names(&self) -> anyhow::Result<Vec<String>> {
        Ok(vec![MODEL.to_string()])
    }

    async fn organization_has_model_access(
        &self,
        organization_id: Uuid,
        _model_id: Uuid,
    ) -> anyhow::Result<bool> {
        Ok(organization_id == self.granted_organization)
    }
}

fn unlisted_catalog_model() -> ModelWithPricing {
    ModelWithPricing {
        is_listed: false,
        ..catalog_model()
    }
}

/// Service whose pool serves `MODEL` from a recording `MockProvider`. Returns
/// the number of attestation calls pool registration already made, so tests
/// can look only at the calls their own requests caused.
async fn report_service() -> (AttestationService, Arc<MockProvider>, usize) {
    let pool = Arc::new(InferenceProviderPool::new(
        None,
        ExternalProvidersConfig::default(),
    ));
    let provider = Arc::new(MockProvider::new());
    pool.register_provider(MODEL.to_string(), provider.clone())
        .await;
    let registration_calls = provider.attestation_signing_algos().len();

    let mut service = lifecycle_service(Arc::new(RecordingRepository::default()), pool);
    service.models_repository = Arc::new(SingleModelRepository);
    service.gateway_quote_collector = Arc::new(EchoGatewayQuoteCollector);
    (service, provider, registration_calls)
}

fn backend_algos_since(provider: &MockProvider, baseline: usize) -> Vec<Option<String>> {
    provider.attestation_signing_algos()[baseline..].to_vec()
}

#[tokio::test]
async fn fleet_reports_survive_cache_hits_and_nonce_requests_bypass_the_cache() {
    use std::sync::atomic::{AtomicUsize, Ordering};

    let calls = Arc::new(AtomicUsize::new(0));
    let recorded_calls = calls.clone();
    let provider = Arc::new(
        MockProvider::new()
            .with_tier(inference_providers::ProviderTier::Near)
            .with_attestation_reports_handler(move |_, _, nonce, _, _| {
                recorded_calls.fetch_add(1, Ordering::SeqCst);
                Ok(["a", "b"]
                    .into_iter()
                    .map(|instance| {
                        let mut report = serde_json::json!({
                            "instance_id": instance,
                            "signing_public_key": "shared-key",
                            "request_nonce": nonce,
                            "intel_quote": "",
                            "event_log": [],
                        })
                        .as_object()
                        .unwrap()
                        .clone();
                        if instance == "a" {
                            report.insert("dstack_attestation".into(), serde_json::json!("0003"));
                        } else {
                            report.insert("intel_quote".into(), serde_json::json!("abcd"));
                        }
                        report
                    })
                    .collect())
            }),
    );
    let pool = Arc::new(InferenceProviderPool::new(
        None,
        ExternalProvidersConfig::default(),
    ));
    pool.register_provider(MODEL.to_string(), provider).await;
    let mut service = lifecycle_service(Arc::new(RecordingRepository::default()), pool);
    service.models_repository = Arc::new(SingleModelRepository);
    service.gateway_quote_collector = Arc::new(EchoGatewayQuoteCollector);
    service.report_cache = Some(
        moka::future::Cache::builder()
            .max_capacity(16)
            .time_to_live(Duration::from_secs(60))
            .build(),
    );

    for (nonce, expected_calls) in [(None, 1), (None, 1), (Some(NONCE), 2), (Some(NONCE), 3)] {
        let report = service
            .get_attestation_report(
                Some(MODEL.to_string()),
                Some("ed25519".to_string()),
                nonce.map(str::to_string),
                None,
                false,
                Some(inference_providers::ProviderFilter::Near),
                ModelCaller::Anonymous,
            )
            .await
            .expect("fleet report");
        assert_eq!(report.model_attestations.len(), 2);
        assert_eq!(report.model_attestations[0]["instance_id"], "a");
        assert_eq!(report.model_attestations[1]["instance_id"], "b");
        assert_eq!(report.model_attestations[0]["dstack_attestation"], "0003");
        assert_eq!(report.model_attestations[0]["intel_quote"], "");
        assert!(report.model_attestations[1]
            .get("dstack_attestation")
            .is_none());
        assert_eq!(report.model_attestations[1]["intel_quote"], "abcd");
        for entry in &report.model_attestations {
            assert_eq!(entry["request_nonce"], serde_json::json!(nonce));
        }
        assert_eq!(calls.load(Ordering::SeqCst), expected_calls);
    }
}

#[tokio::test]
async fn mixed_case_signing_algo_is_forwarded_lowercased() {
    let (service, provider, baseline) = report_service().await;

    for (requested, normalized) in [("ECDSA", "ecdsa"), ("Ed25519", "ed25519")] {
        let report = service
            .get_attestation_report(
                Some(MODEL.to_string()),
                Some(requested.to_string()),
                Some(NONCE.to_string()),
                None,
                false,
                None,
                ModelCaller::Anonymous,
            )
            .await
            .unwrap_or_else(|e| panic!("{requested} must be accepted: {e}"));

        assert_eq!(report.gateway_attestation.signing_algo, normalized);
        assert_eq!(report.model_attestations.len(), 1);
    }
    assert_eq!(
        backend_algos_since(&provider, baseline),
        vec![Some("ecdsa".to_string()), Some("ed25519".to_string())]
    );
}

#[tokio::test]
async fn omitted_signing_algo_leaves_the_backend_default() {
    let (service, provider, baseline) = report_service().await;

    let report = service
        .get_attestation_report(
            Some(MODEL.to_string()),
            None,
            Some(NONCE.to_string()),
            None,
            false,
            None,
            ModelCaller::Anonymous,
        )
        .await
        .expect("omitted signing_algo must succeed");

    assert_eq!(report.gateway_attestation.signing_algo, "ed25519");
    assert_eq!(backend_algos_since(&provider, baseline), vec![None]);
}

#[tokio::test]
async fn unknown_model_is_a_client_error_and_skips_the_backend() {
    let (service, provider, baseline) = report_service().await;

    for nonce in [Some(NONCE.to_string()), None] {
        let error = match service
            .get_attestation_report(
                Some("test-org/no-such-model".to_string()),
                Some("ecdsa".to_string()),
                nonce,
                None,
                false,
                None,
                ModelCaller::Anonymous,
            )
            .await
        {
            Ok(_) => panic!("unknown model must be rejected"),
            Err(error) => error,
        };

        assert!(
            matches!(&error, AttestationError::UnknownModel(model) if model == "test-org/no-such-model"),
            "unexpected error: {error:?}"
        );
        assert_eq!(
            error.to_string(),
            "Model 'test-org/no-such-model' not found. It's not a valid model name or alias."
        );
    }
    assert!(backend_algos_since(&provider, baseline).is_empty());
}

#[tokio::test]
async fn unsupported_signing_algo_is_rejected_before_the_backend() {
    let (service, provider, baseline) = report_service().await;

    let result = service
        .get_attestation_report(
            Some(MODEL.to_string()),
            Some("RSA".to_string()),
            Some(NONCE.to_string()),
            None,
            false,
            None,
            ModelCaller::Anonymous,
        )
        .await;

    assert!(matches!(result, Err(AttestationError::InvalidParameter(_))));
    assert!(backend_algos_since(&provider, baseline).is_empty());
}

#[tokio::test]
async fn no_nonce_cache_is_keyed_on_the_normalized_algo() {
    let (mut service, provider, baseline) = report_service().await;
    service.report_cache = Some(
        moka::future::Cache::builder()
            .max_capacity(16)
            .time_to_live(Duration::from_secs(60))
            .build(),
    );
    let fetch = |algo: Option<&str>| {
        service.get_attestation_report(
            Some(MODEL.to_string()),
            algo.map(str::to_string),
            None,
            None,
            false,
            None,
            ModelCaller::Anonymous,
        )
    };

    // Casings of one algorithm share an entry: one backend call.
    fetch(Some("ECDSA")).await.expect("ECDSA report");
    fetch(Some("ecdsa")).await.expect("ecdsa report");
    // An omitted algorithm gets the backend default, so a later explicit
    // ed25519 request must not be served that report.
    fetch(None).await.expect("default-algo report");
    let ed25519 = fetch(Some("ed25519")).await.expect("ed25519 report");

    assert_eq!(
        backend_algos_since(&provider, baseline),
        vec![Some("ecdsa".to_string()), None, Some("ed25519".to_string())]
    );
    assert_eq!(
        ed25519.model_attestations[0]["signing_public_key"],
        MOCK_ED25519_PUBLIC_KEY
    );
}

/// A nonce-less report cached before a source is disabled must not be served
/// afterwards: a cache hit skips the pool's disabled-source filter, so the
/// live policy is part of the cache key. Covers filtered and unfiltered.
#[tokio::test]
async fn no_nonce_cache_does_not_serve_a_source_disabled_after_caching() {
    use inference_providers::{DisabledSources, ProviderFilter, ProviderSource, ProviderTier};

    let pool = Arc::new(InferenceProviderPool::new(
        None,
        ExternalProvidersConfig::default(),
    ));
    let chutes: Arc<dyn inference_providers::InferenceProvider + Send + Sync> = Arc::new(
        MockProvider::new()
            .with_tier(ProviderTier::Attested3p)
            .with_provider_source(ProviderSource::Chutes),
    );
    pool.register_pinned_secondary_provider(MODEL.to_string(), chutes, None)
        .await;
    let disabled = pool.attested_3p_disabled();

    let mut service = lifecycle_service(Arc::new(RecordingRepository::default()), pool);
    service.models_repository = Arc::new(SingleModelRepository);
    service.gateway_quote_collector = Arc::new(EchoGatewayQuoteCollector);
    service.report_cache = Some(
        moka::future::Cache::builder()
            .max_capacity(16)
            .time_to_live(Duration::from_secs(60))
            .build(),
    );
    let fetch = |filter: Option<ProviderFilter>| {
        service.get_attestation_report(
            Some(MODEL.to_string()),
            None,
            None,
            None,
            false,
            filter,
            ModelCaller::Anonymous,
        )
    };
    let chutes_filter = Some(ProviderFilter::Source(ProviderSource::Chutes));

    // Warm both cache entries while Chutes is enabled.
    fetch(None).await.expect("unfiltered report while enabled");
    fetch(chutes_filter)
        .await
        .expect("filtered report while enabled");

    let mut d = DisabledSources::default();
    d.insert(ProviderSource::Chutes);
    disabled.store(Arc::new(d));

    assert!(
        fetch(None).await.is_err(),
        "unfiltered request must not be served a cached report from a disabled source"
    );
    assert!(
        fetch(chutes_filter).await.is_err(),
        "filtered request must not be served a cached report from a disabled source"
    );
}

/// An unlisted model has a report only for a caller that may use it. The
/// check must not depend on the no-nonce cache: a report cached for an
/// organization that holds a grant must not be served to anyone else.
#[tokio::test]
async fn unlisted_model_report_is_refused_like_an_unknown_model_even_when_cached() {
    let (mut service, provider, baseline) = report_service().await;
    let granted = Uuid::new_v4();
    service.models_repository = Arc::new(RestrictedModelRepository {
        granted_organization: granted,
    });
    service.report_cache = Some(
        moka::future::Cache::builder()
            .max_capacity(16)
            .time_to_live(Duration::from_secs(60))
            .build(),
    );
    let fetch = |model: &str, nonce: Option<&str>, caller: ModelCaller| {
        service.get_attestation_report(
            Some(model.to_string()),
            None,
            nonce.map(str::to_string),
            None,
            false,
            None,
            caller,
        )
    };

    // The granted organization gets the report, and the nonce-less one is
    // now cached.
    let granted_caller = ModelCaller::Organization(granted);
    fetch(MODEL, None, granted_caller)
        .await
        .expect("no-nonce report for the granted organization");
    fetch(MODEL, Some(NONCE), granted_caller)
        .await
        .expect("nonce report for the granted organization");
    let backend_calls = backend_algos_since(&provider, baseline).len();
    assert_eq!(backend_calls, 2);

    for caller in [
        ModelCaller::Organization(Uuid::new_v4()),
        ModelCaller::Anonymous,
    ] {
        for nonce in [None, Some(NONCE)] {
            let refused = match fetch(MODEL, nonce, caller).await {
                Ok(_) => panic!("{caller:?} must not get a report for the unlisted model"),
                Err(error) => error,
            };
            let unknown = match fetch("test-org/no-such-model", nonce, caller).await {
                Ok(_) => panic!("unknown model must be rejected"),
                Err(error) => error,
            };
            assert!(matches!(refused, AttestationError::UnknownModel(_)));
            assert!(matches!(unknown, AttestationError::UnknownModel(_)));
            assert_eq!(
                refused.to_string().replace(MODEL, "<model>"),
                unknown
                    .to_string()
                    .replace("test-org/no-such-model", "<model>")
            );
        }
    }
    // The refusals never reached the backend.
    assert_eq!(
        backend_algos_since(&provider, baseline).len(),
        backend_calls
    );

    // The cached report still serves the organization that may read it.
    fetch(MODEL, None, granted_caller)
        .await
        .expect("cached report for the granted organization");
    assert_eq!(
        backend_algos_since(&provider, baseline).len(),
        backend_calls
    );
}

/// Blocks fake for the long-context tests: `blocked` is blocked from every
/// model's long tier.
struct BlockedOrganization {
    blocked: Uuid,
}

#[async_trait]
impl crate::long_context_access::LongContextAccessRepository for BlockedOrganization {
    async fn blocked_model_names(&self, organization_id: Uuid) -> anyhow::Result<Vec<String>> {
        Ok(if organization_id == self.blocked {
            vec![MODEL.to_string()]
        } else {
            Vec::new()
        })
    }

    async fn organization_settings(&self, _: Uuid) -> anyhow::Result<Option<serde_json::Value>> {
        Ok(None)
    }
}

/// An organization that may not use the model's long-context tier gets a
/// report without it, and the no-nonce cache never crosses the two answers,
/// in either order.
#[tokio::test]
async fn long_context_restricted_report_excludes_the_long_tier_and_is_cached_apart() {
    use inference_providers::ProviderTier;
    use std::sync::atomic::{AtomicUsize, Ordering};

    let blocked = Uuid::new_v4();
    let restricted = ModelCaller::Organization(blocked);
    let unrestricted = ModelCaller::Organization(Uuid::new_v4());

    for order in [
        [restricted, unrestricted, ModelCaller::Anonymous],
        [unrestricted, ModelCaller::Anonymous, restricted],
    ] {
        let pool = Arc::new(InferenceProviderPool::new(
            None,
            ExternalProvidersConfig::default(),
        ));
        let calls = Arc::new(AtomicUsize::new(0));
        for (fleet, capacity) in [("base", 1_000), ("long", 10_000)] {
            let calls = calls.clone();
            let provider = Arc::new(
                MockProvider::new()
                    .with_tier(ProviderTier::Near)
                    .with_attestation_reports_handler(move |_, _, _, _, _| {
                        calls.fetch_add(1, Ordering::SeqCst);
                        Ok(vec![serde_json::json!({ "fleet": fleet })
                            .as_object()
                            .unwrap()
                            .clone()])
                    }),
            );
            pool.register_pinned_provider(
                MODEL.to_string(),
                provider,
                Some(capacity),
                crate::inference_provider_pool::ProviderPoolRole::Primary,
            )
            .await;
        }
        // The catalog row's configured tiers, as an inference-URL load records.
        pool.record_long_context_boundaries(&[
            (
                MODEL.to_string(),
                "https://base.example".to_string(),
                Some(1_000),
            ),
            (
                MODEL.to_string(),
                "https://long.example".to_string(),
                Some(10_000),
            ),
        ]);
        let access = Arc::new(crate::long_context_access::LongContextAccess::new(
            Arc::new(BlockedOrganization { blocked }),
        ));

        let mut service = lifecycle_service(Arc::new(RecordingRepository::default()), pool)
            .with_long_context_access(access);
        service.models_repository = Arc::new(SingleModelRepository);
        service.gateway_quote_collector = Arc::new(EchoGatewayQuoteCollector);
        service.report_cache = Some(
            moka::future::Cache::builder()
                .max_capacity(16)
                .time_to_live(Duration::from_secs(60))
                .build(),
        );

        let mut restricted_misses = 0;
        for caller in order {
            let before = calls.load(Ordering::SeqCst);
            let report = service
                .get_attestation_report(
                    Some(MODEL.to_string()),
                    None,
                    None,
                    None,
                    false,
                    None,
                    caller,
                )
                .await
                .expect("report");
            let fleets: Vec<&str> = report
                .model_attestations
                .iter()
                .map(|entry| entry["fleet"].as_str().unwrap())
                .collect();
            if caller == restricted {
                assert_eq!(fleets, vec!["base"], "order: {order:?}");
                restricted_misses += calls.load(Ordering::SeqCst) - before;
            } else {
                assert_eq!(fleets, vec!["base", "long"], "order: {order:?}");
            }
        }
        // The restricted report was built for the restricted caller (one
        // fleet collected), never taken from the shared entry.
        assert_eq!(restricted_misses, 1, "order: {order:?}");
        // One build for the shared entry, one for the restricted one.
        assert_eq!(calls.load(Ordering::SeqCst), 3, "order: {order:?}");
    }
}

/// The report cache is partitioned by the caller's access policy alone, also
/// while the model has no long tier: when a refresh adds one, a restricted
/// caller can never be served a report cached for an unrestricted caller.
#[tokio::test]
async fn long_context_report_partition_follows_the_policy_across_a_refresh() {
    use inference_providers::ProviderTier;
    use std::sync::atomic::{AtomicUsize, Ordering};

    let blocked = Uuid::new_v4();
    let restricted = ModelCaller::Organization(blocked);
    let unrestricted = ModelCaller::Organization(Uuid::new_v4());
    let pool = Arc::new(InferenceProviderPool::new(
        None,
        ExternalProvidersConfig::default(),
    ));
    let calls = Arc::new(AtomicUsize::new(0));
    let fleet = |name: &'static str| {
        let calls = calls.clone();
        Arc::new(
            MockProvider::new()
                .with_tier(ProviderTier::Near)
                .with_attestation_reports_handler(move |_, _, _, _, _| {
                    calls.fetch_add(1, Ordering::SeqCst);
                    Ok(vec![serde_json::json!({ "fleet": name })
                        .as_object()
                        .unwrap()
                        .clone()])
                }),
        )
    };
    // Single tier: only the base fleet, no boundary.
    pool.register_pinned_provider(
        MODEL.to_string(),
        fleet("base"),
        Some(1_000),
        crate::inference_provider_pool::ProviderPoolRole::Primary,
    )
    .await;
    let access = Arc::new(crate::long_context_access::LongContextAccess::new(
        Arc::new(BlockedOrganization { blocked }),
    ));
    let mut service = lifecycle_service(Arc::new(RecordingRepository::default()), pool.clone())
        .with_long_context_access(access);
    service.models_repository = Arc::new(SingleModelRepository);
    service.gateway_quote_collector = Arc::new(EchoGatewayQuoteCollector);
    service.report_cache = Some(
        moka::future::Cache::builder()
            .max_capacity(16)
            .time_to_live(Duration::from_secs(60))
            .build(),
    );
    let fleets = |caller: ModelCaller| {
        let service = &service;
        async move {
            service
                .get_attestation_report(
                    Some(MODEL.to_string()),
                    None,
                    None,
                    None,
                    false,
                    None,
                    caller,
                )
                .await
                .expect("report")
                .model_attestations
                .iter()
                .map(|entry| entry["fleet"].as_str().unwrap().to_string())
                .collect::<Vec<_>>()
        }
    };

    // Before the refresh the restricted caller already has its own entry.
    assert_eq!(fleets(restricted).await, vec!["base"]);
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    assert_eq!(fleets(unrestricted).await, vec!["base"]);
    assert_eq!(calls.load(Ordering::SeqCst), 2, "separate cache entries");

    // The refresh adds the long tier and its boundary. Entries cached so far
    // never mixed the two callers; a new entry for each caller follows the
    // policy (the unrestricted one cached first, then the restricted one).
    pool.register_pinned_provider(
        MODEL.to_string(),
        fleet("long"),
        Some(10_000),
        crate::inference_provider_pool::ProviderPoolRole::Primary,
    )
    .await;
    pool.record_long_context_boundaries(&[
        (
            MODEL.to_string(),
            "https://base.example".to_string(),
            Some(1_000),
        ),
        (
            MODEL.to_string(),
            "https://long.example".to_string(),
            Some(10_000),
        ),
    ]);
    service.report_cache.as_ref().unwrap().invalidate_all();
    assert_eq!(fleets(unrestricted).await, vec!["base", "long"]);
    assert_eq!(fleets(restricted).await, vec!["base"]);
}
