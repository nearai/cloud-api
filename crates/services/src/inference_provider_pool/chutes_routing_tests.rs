use super::*;
use base64::Engine;
use inference_providers::attested::chutes::{
    evidence::InstanceEvidence,
    verifier_port::{ChutesInstanceVerifier, VerifiedInstanceInfo},
    Config, Provider,
};
use inference_providers::mock::MockProvider;
use inference_providers::ProviderTier;
use serde_json::json;
use std::sync::atomic::{AtomicUsize, Ordering};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, Request, ResponseTemplate};

const MODEL: &str = "test-model";
const NEAR_KEY: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

fn chutes_key(byte: u8) -> String {
    base64::engine::general_purpose::STANDARD.encode([byte; 1184])
}

struct PinnedVerifier;

#[async_trait::async_trait]
impl ChutesInstanceVerifier for PinnedVerifier {
    async fn attest_instance(
        &self,
        evidence: &InstanceEvidence,
        _boot_nonce: &str,
        e2e_pubkey: &str,
    ) -> Result<VerifiedInstanceInfo, String> {
        assert_eq!(evidence.instance_id, "pinned");
        assert_eq!(e2e_pubkey, chutes_key(0));
        Ok(VerifiedInstanceInfo {
            instance_id: evidence.instance_id.clone(),
            e2e_pubkey: e2e_pubkey.into(),
            measurement_config: "test".into(),
            tcb_status: "UpToDate".into(),
            gpu_verdict: "PASS".into(),
        })
    }
}

async fn pool_with_chutes(
    server: &MockServer,
) -> (
    InferenceProviderPool,
    Arc<MockProvider>,
    Arc<InferenceProviderTrait>,
) {
    let pool = InferenceProviderPool::new(None, ExternalProvidersConfig::default());
    let near = Arc::new(MockProvider::new().with_tier(ProviderTier::Near));
    pool.register_provider(MODEL.into(), near.clone()).await;
    let chutes: Arc<InferenceProviderTrait> = Arc::new(
        Provider::new(
            Config::new("synthetic-test-key".into(), "upstream".into(), 5)
                .with_canonical_id(MODEL)
                .with_streaming(true)
                .with_hosts(server.uri(), server.uri()),
            Arc::new(PinnedVerifier),
        )
        .unwrap(),
    );
    pool.register_pinned_secondary_provider(MODEL.into(), chutes.clone(), None)
        .await;
    (pool, near, chutes)
}

#[tokio::test]
async fn chutes_pin_requires_the_model_and_survives_pool_discovery_refresh() {
    let server = MockServer::start().await;
    let (pool, near, chutes) = pool_with_chutes(&server).await;
    pool.register_provider("other-model".into(), near.clone())
        .await;
    let key = chutes_key(0);
    let hints = ChatRoutingHints::default();

    assert!(pool
        .get_providers_with_fallback("other-model", Some(&key), &hints)
        .await
        .is_none());
    let selected = pool
        .get_providers_with_fallback(MODEL, Some(&key), &hints)
        .await
        .unwrap();
    assert_eq!(selected.len(), 1);
    assert!(Arc::ptr_eq(&selected[0], &chutes));

    // A complete discovery refresh removes discovered backends and reattaches
    // the configured Chutes provider. Its key routing must not need a stale map.
    pool.sync_inference_url_models(Vec::new()).await;
    let selected = pool
        .get_providers_with_fallback(MODEL, Some(&key), &hints)
        .await
        .unwrap();
    assert_eq!(selected.len(), 1);
    assert!(Arc::ptr_eq(&selected[0], &chutes));
    assert!(server.received_requests().await.unwrap().is_empty());
}

#[tokio::test]
async fn near_hex_pin_and_unconstrained_tier_order_are_unchanged() {
    let server = MockServer::start().await;
    let (pool, near, chutes) = pool_with_chutes(&server).await;
    let near: Arc<InferenceProviderTrait> = near;
    let hints = ChatRoutingHints::default();
    let selected = pool
        .get_providers_with_fallback(MODEL, Some(NEAR_KEY), &hints)
        .await
        .unwrap();
    assert_eq!(selected.len(), 1);
    assert!(Arc::ptr_eq(&selected[0], &near));
    assert!(pool
        .get_providers_with_fallback(MODEL, Some(&"ab".repeat(32)), &hints)
        .await
        .is_none());
    let selected = pool
        .get_providers_with_fallback(MODEL, None, &hints)
        .await
        .unwrap();
    assert_eq!(selected.len(), 2);
    assert!(Arc::ptr_eq(&selected[0], &near));
    assert!(Arc::ptr_eq(&selected[1], &chutes));
}

async fn mount_discovery(server: &MockServer, rotate: bool, unknown: bool) {
    Mock::given(method("GET"))
        .and(path("/v1/models"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "data": [{"id": "upstream", "chute_id": "chute"}]
        })))
        .mount(server)
        .await;
    let discoveries = AtomicUsize::new(0);
    Mock::given(method("GET"))
        .and(path("/e2e/instances/chute"))
        .respond_with(move |_: &Request| {
            let attempt = discoveries.fetch_add(1, Ordering::SeqCst);
            let key = if unknown || (rotate && attempt > 0) { chutes_key(1) } else { chutes_key(0) };
            ResponseTemplate::new(200).set_body_json(json!({
                "nonce_expires_in": 120,
                "instances": [
                    {"instance_id": "other", "e2e_pubkey": chutes_key(1), "nonces": ["unused"]},
                    {"instance_id": "pinned", "e2e_pubkey": key, "nonces": [format!("nonce-{attempt}")]}
                ]
            }))
        })
        .expect(if unknown { 1 } else { 2 })
        .mount(server).await;
    Mock::given(method("GET"))
        .and(path("/chutes/chute/evidence"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "evidence": [
                {"instance_id": "pinned", "quote": "test", "certificate": "test"},
                {"instance_id": "other", "quote": "test", "certificate": "test"}
            ]
        })))
        .mount(server)
        .await;
}

async fn pinned_chat(pool: &InferenceProviderPool, streaming: bool) -> Result<(), CompletionError> {
    let mut params: ChatCompletionParams = serde_json::from_value(json!({
        "model": MODEL, "messages": [], "stream": streaming
    }))
    .unwrap();
    params.extra.insert(
        encryption_headers::MODEL_PUB_KEY.into(),
        json!(chutes_key(0)),
    );
    if streaming {
        pool.chat_completion_stream(params, "test".into(), ChatRoutingHints::default())
            .await
            .map(|_| ())
    } else {
        pool.chat_completion(params, "test".into())
            .await
            .map(|_| ())
    }
}

#[tokio::test]
async fn pinned_retries_refresh_matching_nonces_without_falling_back_to_near() {
    for streaming in [false, true] {
        let server = MockServer::start().await;
        let (pool, near, _) = pool_with_chutes(&server).await;
        mount_discovery(&server, false, false).await;
        let invokes = AtomicUsize::new(0);
        Mock::given(method("POST"))
            .and(path("/e2e/invoke"))
            .respond_with(move |request: &Request| {
                let attempt = invokes.fetch_add(1, Ordering::SeqCst);
                assert_eq!(request.headers["X-Instance-Id"], "pinned");
                assert_eq!(request.headers["X-E2E-Nonce"], format!("nonce-{attempt}"));
                assert_eq!(request.headers["X-E2E-Stream"], streaming.to_string());
                ResponseTemplate::new(if attempt == 0 { 503 } else { 400 })
            })
            .expect(2)
            .mount(&server)
            .await;
        let error = pinned_chat(&pool, streaming).await.unwrap_err();
        assert!(
            matches!(&error, CompletionError::CompletionError(message) if message.contains("HTTP 400")),
            "unexpected terminal error: {error:?}"
        );
        assert!(near.last_chat_params().await.is_none());
    }
}

#[tokio::test]
async fn unknown_and_rotated_keys_fail_closed_even_when_near_is_healthy() {
    for streaming in [false, true] {
        for unknown in [false, true] {
            let server = MockServer::start().await;
            let (pool, near, _) = pool_with_chutes(&server).await;
            mount_discovery(&server, true, unknown).await;
            Mock::given(method("POST"))
                .and(path("/e2e/invoke"))
                .respond_with(|request: &Request| {
                    assert_eq!(request.headers["X-Instance-Id"], "pinned");
                    ResponseTemplate::new(503)
                })
                .expect(if unknown { 0 } else { 1 })
                .mount(&server)
                .await;
            assert!(matches!(
                pinned_chat(&pool, streaming).await,
                Err(CompletionError::NoPubKeyProvider(_))
            ));
            assert!(near.last_chat_params().await.is_none());
        }
    }
}
