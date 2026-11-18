// E2E tests for GET /v1/attestation/report parameter handling:
// - `signing_algo` is case-insensitive, and the backend receives the lowercase
//   value (the mock backend, like inference-proxy, rejects anything else);
// - an unknown model is reported exactly like /v1/chat/completions does
//   (400 invalid_request_error, param "model"), not as a 503 provider error.

use crate::common::*;

fn report_url(model: &str) -> String {
    let encoded = url::form_urlencoded::byte_serialize(model.as_bytes()).collect::<String>();
    format!("/v1/attestation/report?model={encoded}")
}

/// Exercise the real HTTP handler, attestation service/cache, and provider pool.
/// The provider represents three CVMs sharing one signer; the Gateway must keep
/// each CVM's evidence instead of selecting one report or deduplicating the key.
#[tokio::test]
async fn test_attestation_report_returns_all_near_backends_with_original_envelope() {
    use inference_providers::{mock::MockProvider, ProviderTier};
    use std::sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    };

    let (server, _router, pool, _mock_provider, _database) =
        setup_test_server_with_pool_and_router().await;
    let model = setup_qwen_model(&server).await;
    let backend_calls = Arc::new(AtomicUsize::new(0));
    let recorded_calls = backend_calls.clone();
    let expected_model = model.clone();
    let cached_nonce = "00".repeat(32);
    let expected_cached_nonce = cached_nonce.clone();
    let provider = MockProvider::new()
        .with_tier(ProviderTier::Near)
        .with_attestation_reports_handler(move |model, algo, nonce, address, include_tls| {
            assert_eq!(model, expected_model);
            assert_eq!(algo.as_deref(), Some("ed25519"));
            assert!(address.is_none());
            assert!(!include_tls);
            recorded_calls.fetch_add(1, Ordering::SeqCst);
            let nonce = nonce.as_deref().unwrap_or(&cached_nonce);
            Ok(["cvm-a", "cvm-b", "cvm-c"]
                .into_iter()
                .map(|instance| {
                    serde_json::json!({
                        "model_name": model,
                        "instance_id": instance,
                        "signing_algo": "ed25519",
                        "signing_public_key": "shared-model-key",
                        "request_nonce": nonce,
                        "intel_quote": format!("mock-quote-{instance}"),
                        "compose_manager_attestation": {
                            "actions": [{"instance": instance}]
                        },
                        "all_attestations": [{"instance_id": instance}]
                    })
                    .as_object()
                    .unwrap()
                    .clone()
                })
                .collect())
        });
    // Registration replaces the model's default mock provider. Its singular
    // key-discovery calls do not invoke the collection handler above.
    pool.register_provider(model.clone(), Arc::new(provider))
        .await;
    assert_eq!(backend_calls.load(Ordering::SeqCst), 0);

    let nonce = "deadbeef".repeat(8);
    for (challenge, expected_calls) in [
        (None, 1),
        (None, 1),
        (Some(nonce.as_str()), 2),
        (Some(nonce.as_str()), 3),
    ] {
        let mut url = format!("{}&provider=near&signing_algo=ed25519", report_url(&model));
        if let Some(challenge) = challenge {
            url.push_str(&format!("&nonce={challenge}"));
        }
        let response = server.get(&url).await;
        assert_eq!(response.status_code(), 200, "{url}: {}", response.text());
        let body: serde_json::Value = response.json();
        assert!(body["gateway_attestation"].is_object(), "{body}");
        assert_eq!(body["gateway_attestation"]["signing_algo"], "ed25519");
        if let Some(challenge) = challenge {
            assert_eq!(body["gateway_attestation"]["request_nonce"], challenge);
        }
        assert!(body.get("all_attestations").is_none());

        let reports = body["model_attestations"].as_array().unwrap();
        assert_eq!(reports.len(), 3, "{body}");
        for (report, instance) in reports.iter().zip(["cvm-a", "cvm-b", "cvm-c"]) {
            assert_eq!(report["model_name"], model);
            assert_eq!(report["instance_id"], instance);
            assert_eq!(report["signing_public_key"], "shared-model-key");
            assert_eq!(
                report["request_nonce"],
                challenge.unwrap_or(&expected_cached_nonce)
            );
            assert_eq!(
                report["compose_manager_attestation"]["actions"][0]["instance"],
                instance
            );
            assert!(report.get("all_attestations").is_none());
        }
        assert_eq!(backend_calls.load(Ordering::SeqCst), expected_calls);
    }
}

#[tokio::test]
async fn test_attestation_report_accepts_mixed_case_signing_algo() {
    let (server, _router, _pool, mock_provider, _database) =
        setup_test_server_with_pool_and_router().await;
    let model = setup_qwen_model(&server).await;
    let org = setup_org_with_credits(&server, 10_000_000_000i64).await;
    let api_key = get_api_key_for_org(&server, org.id).await;
    let nonce = "deadbeef".repeat(8);

    for (requested, normalized) in [("ECDSA", "ecdsa"), ("Ed25519", "ed25519")] {
        // With a nonce the request bypasses the report cache; without one it
        // goes through it. Each no-nonce key is requested once, so both paths
        // reach the backend exactly once.
        for nonce_param in [format!("&nonce={nonce}"), String::new()] {
            let url = format!(
                "{}&signing_algo={requested}{nonce_param}",
                report_url(&model)
            );
            let backend_calls_before = mock_provider.attestation_signing_algos().len();

            let response = server
                .get(&url)
                .add_header("Authorization", format!("Bearer {api_key}"))
                .await;

            assert_eq!(
                response.status_code(),
                200,
                "{url} must succeed: {}",
                response.text()
            );
            let body: serde_json::Value = response.json();
            assert_eq!(
                body["gateway_attestation"]["signing_algo"], normalized,
                "{url}: {body}"
            );
            let forwarded =
                mock_provider.attestation_signing_algos()[backend_calls_before..].to_vec();
            assert_eq!(forwarded, vec![Some(normalized.to_string())], "{url}");
        }
    }
}

#[tokio::test]
async fn test_attestation_report_unknown_model_matches_chat_completions() {
    let server = setup_test_server().await;
    let org = setup_org_with_credits(&server, 10_000_000_000i64).await;
    let api_key = get_api_key_for_org(&server, org.id).await;
    let unknown = format!("test-unknown/model-{}", uuid::Uuid::new_v4());

    let chat = server
        .post("/v1/chat/completions")
        .add_header("Authorization", format!("Bearer {api_key}"))
        .json(&serde_json::json!({
            "model": unknown,
            "messages": [{ "role": "user", "content": "Hello" }],
            "max_tokens": 10
        }))
        .await;
    assert_eq!(chat.status_code(), 400, "{}", chat.text());
    let chat_body: serde_json::Value = chat.json();
    assert_eq!(chat_body["error"]["type"], "invalid_request_error");
    assert_eq!(chat_body["error"]["param"], "model");

    let nonce = "deadbeef".repeat(8);
    for query in [format!("&nonce={nonce}"), String::new()] {
        // x-no-aliasing only rejects aliases; an unknown model gets the same
        // error with or without it.
        for no_aliasing in [false, true] {
            let url = format!("{}{query}", report_url(&unknown));
            let mut request = server
                .get(&url)
                .add_header("Authorization", format!("Bearer {api_key}"));
            if no_aliasing {
                request = request.add_header("x-no-aliasing", "true");
            }
            let response = request.await;

            assert_eq!(
                response.status_code(),
                400,
                "{url} (x-no-aliasing={no_aliasing}): {}",
                response.text()
            );
            let body: serde_json::Value = response.json();
            assert_eq!(
                body, chat_body,
                "{url} (x-no-aliasing={no_aliasing}) must match the chat completions error"
            );
        }
    }
}
