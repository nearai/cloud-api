use super::*;
use inference_providers::mock::MockProvider;
use inference_providers::{DisabledSources, ProviderFilter, ProviderSource, ProviderTier};
use serde_json::{json, Map, Value};

const MODEL: &str = "test/model";
const NONCE: &str = "client-challenge";

fn report(instance: &str) -> Map<String, Value> {
    json!({
        "model_name": MODEL,
        "instance_id": instance,
        "signing_public_key": "shared-key",
        "compose_manager_attestation": {"instance": instance},
        "all_attestations": [{"instance_id": instance}],
    })
    .as_object()
    .unwrap()
    .clone()
}

fn near(reports: Vec<Map<String, Value>>) -> Arc<MockProvider> {
    Arc::new(
        MockProvider::new()
            .with_tier(ProviderTier::Near)
            .with_attestation_reports_handler(move |_, _, _, _, _| Ok(reports.clone())),
    )
}

async fn pool_with(providers: Vec<Arc<MockProvider>>) -> InferenceProviderPool {
    let pool = InferenceProviderPool::new(None, ExternalProvidersConfig::default());
    for provider in providers {
        pool.register_pinned_secondary_provider(MODEL.into(), provider, None)
            .await;
    }
    pool
}

async fn fetch(
    pool: &InferenceProviderPool,
    filter: Option<ProviderFilter>,
    address: Option<String>,
) -> Result<Vec<Map<String, Value>>, AttestationError> {
    pool.get_attestation_report(
        MODEL.into(),
        Some("ed25519".into()),
        Some(NONCE.into()),
        address,
        true,
        filter,
    )
    .await
}

#[tokio::test]
async fn gateway_attestation_preserves_all_fleet_reports_and_each_manager_evidence() {
    let provider = MockProvider::new()
        .with_tier(ProviderTier::Near)
        .with_attestation_reports_handler(|model, algo, nonce, address, include_tls| {
            assert_eq!(model, MODEL);
            assert_eq!(algo.as_deref(), Some("ed25519"));
            assert_eq!(nonce.as_deref(), Some(NONCE));
            assert_eq!(address.as_deref(), Some("requested-address"));
            assert!(include_tls);
            Ok(vec![report("a"), report("b"), report("c")])
        });
    let pool = pool_with(vec![Arc::new(provider)]).await;
    let reports = fetch(
        &pool,
        Some(ProviderFilter::Near),
        Some("requested-address".into()),
    )
    .await
    .unwrap();

    assert_eq!(reports.len(), 3);
    for (report, instance) in reports.iter().zip(["a", "b", "c"]) {
        assert_eq!(report["instance_id"], instance);
        assert_eq!(report["signing_public_key"], "shared-key");
        assert_eq!(report["compose_manager_attestation"]["instance"], instance);
        assert!(!report.contains_key("all_attestations"));
    }
}

#[tokio::test]
async fn gateway_attestation_collects_all_near_fleets_without_signer_deduplication() {
    let unexpected_third_party = Arc::new(
        MockProvider::new()
            .with_tier(ProviderTier::Attested3p)
            .with_attestation_reports_handler(|_, _, _, _, _| {
                panic!("a successful NEAR collection must not query another source")
            }),
    );
    let pool = pool_with(vec![
        near(vec![report("short-context-a"), report("short-context-b")]),
        unexpected_third_party,
        near(vec![report("long-context")]),
    ])
    .await;

    let reports = fetch(&pool, None, None).await.unwrap();
    let instances: Vec<_> = reports
        .iter()
        .map(|r| r["instance_id"].as_str().unwrap())
        .collect();
    assert_eq!(
        instances,
        ["short-context-a", "short-context-b", "long-context"]
    );
}

#[tokio::test]
async fn gateway_attestation_discards_partial_near_collection_before_source_fallback() {
    let broken = Arc::new(
        MockProvider::new()
            .with_tier(ProviderTier::Near)
            .with_attestation_reports_handler(|_, _, _, _, _| {
                Err(AttestationError::FetchError("backend timed out".into()))
            }),
    );
    let fallback = Arc::new(
        MockProvider::new()
            .with_tier(ProviderTier::Attested3p)
            .with_attestation_reports_handler(|_, _, _, _, _| Ok(vec![report("chutes")])),
    );
    let pool = pool_with(vec![near(vec![report("near-a")]), broken, fallback]).await;

    let reports = fetch(&pool, None, None).await.unwrap();
    assert_eq!(reports.len(), 1);
    assert_eq!(reports[0]["instance_id"], "chutes");
    assert!(fetch(&pool, Some(ProviderFilter::Near), None)
        .await
        .is_err());
}

#[tokio::test]
async fn gateway_attestation_address_filter_skips_nonmatching_fleets() {
    let missing = Arc::new(
        MockProvider::new()
            .with_tier(ProviderTier::Near)
            .with_attestation_reports_handler(|_, _, _, address, _| {
                Err(AttestationError::SigningAddressNotFound(address.unwrap()))
            }),
    );
    let pool = pool_with(vec![missing.clone(), near(vec![report("matching")])]).await;
    let reports = fetch(&pool, Some(ProviderFilter::Near), Some("wanted".into()))
        .await
        .unwrap();
    assert_eq!(reports.len(), 1);
    assert_eq!(reports[0]["instance_id"], "matching");

    let empty_pool = pool_with(vec![missing]).await;
    assert!(fetch(
        &empty_pool,
        Some(ProviderFilter::Near),
        Some("wanted".into())
    )
    .await
    .is_err());
}

#[tokio::test]
async fn gateway_attestation_keeps_third_party_selection_and_disabled_source_filter() {
    let near = near(vec![report("near")]);
    let chutes = Arc::new(
        MockProvider::new()
            .with_tier(ProviderTier::Attested3p)
            .with_provider_source(ProviderSource::Chutes)
            .with_attestation_reports_handler(|_, _, _, _, _| Ok(vec![report("chutes-first")])),
    );
    let second_chutes = Arc::new(
        MockProvider::new()
            .with_tier(ProviderTier::Attested3p)
            .with_provider_source(ProviderSource::Chutes)
            .with_attestation_reports_handler(|_, _, _, _, _| {
                panic!("third-party selection must still stop after the first success")
            }),
    );
    let pool = pool_with(vec![near, chutes, second_chutes]).await;
    let filter = Some(ProviderFilter::Source(ProviderSource::Chutes));
    let reports = fetch(&pool, filter, None).await.unwrap();
    assert_eq!(reports.len(), 1);
    assert_eq!(reports[0]["instance_id"], "chutes-first");

    let mut disabled = DisabledSources::default();
    disabled.insert(ProviderSource::Chutes);
    pool.attested_3p_disabled().store(Arc::new(disabled));
    assert!(matches!(
        fetch(&pool, filter, None).await,
        Err(AttestationError::ProviderNotFound(_))
    ));
}

#[tokio::test]
async fn gateway_attestation_default_plural_method_keeps_single_report_providers_working() {
    let pool = pool_with(vec![Arc::new(
        MockProvider::new().with_tier(ProviderTier::Attested3p),
    )])
    .await;
    let reports = fetch(&pool, None, None).await.unwrap();
    assert_eq!(reports.len(), 1);
    assert_eq!(reports[0]["model"], MODEL);
}
