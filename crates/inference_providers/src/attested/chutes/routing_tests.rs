use super::e2ee::test_support::{
    instance_keypair, instance_open_request, instance_seal_response, instance_stream,
};
use super::evidence::InstanceEvidence;
use super::verifier_port::VerifiedInstanceInfo;
use super::*;
use crate::attested::nearai::encryption_headers::MODEL_PUB_KEY;
use std::sync::Mutex;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockGuard, MockServer, Request, ResponseTemplate};

#[derive(Default)]
struct RecordingVerifier {
    calls: Mutex<Vec<(String, String)>>,
    reject: bool,
}

#[async_trait]
impl ChutesInstanceVerifier for RecordingVerifier {
    async fn attest_instance(
        &self,
        evidence: &InstanceEvidence,
        boot_nonce: &str,
        e2e_pubkey: &str,
    ) -> Result<VerifiedInstanceInfo, String> {
        assert_eq!(boot_nonce.len(), 64);
        self.calls
            .lock()
            .unwrap()
            .push((evidence.instance_id.clone(), e2e_pubkey.to_string()));
        if self.reject {
            return Err("test attestation rejection".to_string());
        }
        Ok(VerifiedInstanceInfo {
            instance_id: evidence.instance_id.clone(),
            e2e_pubkey: e2e_pubkey.to_string(),
            measurement_config: "test".into(),
            tcb_status: "UpToDate".into(),
            gpu_verdict: "PASS".into(),
        })
    }
}

fn b64(bytes: &[u8]) -> String {
    base64::engine::general_purpose::STANDARD.encode(bytes)
}

fn discovery(instances: &[(&str, &str)]) -> ResponseTemplate {
    ResponseTemplate::new(200).set_body_json(json!({
        "nonce_expires_in": 120,
        "instances": instances.iter().map(|(id, key)| json!({
            "instance_id": id, "e2e_pubkey": key, "nonces": [format!("nonce-{id}")]
        })).collect::<Vec<_>>()
    }))
}

async fn fixture(
    instances: &[(&str, &str)],
    verifier: Arc<RecordingVerifier>,
) -> (Provider, MockServer) {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/v1/models"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "data": [{"id": "upstream", "chute_id": "chute"}]
        })))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/e2e/instances/chute"))
        .respond_with(discovery(instances))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/chutes/chute/evidence"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "evidence": instances.iter().map(|(id, _)| json!({
                "instance_id": id, "quote": "test", "certificate": "test"
            })).collect::<Vec<_>>()
        })))
        .mount(&server)
        .await;
    let provider = Provider::new(
        Config::new("synthetic-test-key".into(), "upstream".into(), 5)
            .with_canonical_id("model")
            .with_streaming(true)
            .with_hosts(server.uri(), server.uri()),
        verifier,
    )
    .unwrap();
    (provider, server)
}

async fn mount_invoke(
    server: &MockServer,
    instance_id: &'static str,
    instance_dk: ml_kem::DecapsulationKey768,
    streaming: bool,
) -> MockGuard {
    Mock::given(method("POST"))
        .and(path("/e2e/invoke"))
        .respond_with(move |request: &Request| {
            assert_eq!(request.headers["X-Instance-Id"], instance_id);
            assert_eq!(request.headers["X-Chute-Id"], "chute");
            assert_eq!(
                request.headers["X-E2E-Nonce"],
                format!("nonce-{instance_id}")
            );
            assert_eq!(request.headers["X-E2E-Stream"], streaming.to_string());
            // Successful decryption proves encapsulation used the selected key.
            let (body, response_key) = instance_open_request(&instance_dk, &request.body);
            assert_eq!(body["model"], "upstream");
            assert_eq!(body["stream"], streaming);
            assert!(body.get(MODEL_PUB_KEY).is_none());
            if streaming {
                let (init, frame) = instance_stream(&response_key, b"data: [DONE]");
                ResponseTemplate::new(200)
                    .insert_header("content-type", "text/event-stream")
                    .set_body_string(format!(
                        "data: {}\n\ndata: {}\n\n",
                        json!({"e2e_init": b64(&init)}),
                        json!({"e2e": b64(&frame)})
                    ))
            } else {
                ResponseTemplate::new(200).set_body_bytes(instance_seal_response(
                    &response_key,
                    &json!({"id": "chat-test", "object": "chat.completion", "created": 0,
                            "model": "upstream", "choices": [],
                            "usage": {"prompt_tokens": 0, "completion_tokens": 0, "total_tokens": 0}}),
                ))
            }
        })
        .expect(1)
        .mount_as_scoped(server)
        .await
}

async fn chat(
    provider: &Provider,
    key: Option<&str>,
    streaming: bool,
) -> Result<(), CompletionError> {
    let mut params: ChatCompletionParams =
        serde_json::from_value(json!({"model": "model", "messages": [], "stream": streaming}))
            .unwrap();
    if let Some(key) = key {
        params.extra.insert(MODEL_PUB_KEY.into(), json!(key));
    }
    if streaming {
        let mut stream = provider
            .chat_completion_stream(params, "test".into())
            .await?;
        let mut done = false;
        while let Some(event) = stream.next().await {
            done |= event?.is_done_marker();
        }
        assert!(done);
    } else {
        let response = provider.chat_completion(params, "test".into()).await?;
        assert_eq!(response.response.model, "model");
    }
    Ok(())
}

#[tokio::test]
async fn pinned_chat_verifies_encrypts_and_invokes_only_the_matching_key() {
    for streaming in [false, true] {
        let (dk, pk) = instance_keypair();
        let key = b64(&pk);
        let (_, other_pk) = instance_keypair();
        let verifier = Arc::new(RecordingVerifier::default());
        let (provider, server) = fixture(
            &[("other", &b64(&other_pk)), ("pinned", &format!(" {key} "))],
            verifier.clone(),
        )
        .await;
        let _invoke = mount_invoke(&server, "pinned", dk, streaming).await;
        chat(&provider, Some(&key), streaming).await.unwrap();
        assert_eq!(*verifier.calls.lock().unwrap(), [("pinned".into(), key)]);
    }
}

#[tokio::test]
async fn multiple_instances_with_the_same_key_remain_eligible() {
    let (dk, pk) = instance_keypair();
    let key = b64(&pk);
    let verifier = Arc::new(RecordingVerifier::default());
    let (provider, _server) = fixture(&[("first", &key), ("second", &key)], verifier.clone()).await;
    let body = json!({"model": "upstream", "messages": []});
    let first = provider
        .verify_and_prepare(&body, Some(&key))
        .await
        .unwrap();
    let second = provider
        .verify_and_prepare(&body, Some(&key))
        .await
        .unwrap();
    assert_ne!(first.instance_id, second.instance_id);
    for prepared in [first, second] {
        assert!(["first", "second"].contains(&prepared.instance_id.as_str()));
        let (decrypted, _) = instance_open_request(&dk, &prepared.blob);
        assert_eq!(decrypted["model"], "upstream");
    }
    let calls = verifier.calls.lock().unwrap();
    assert_eq!(calls.len(), 2);
    assert!(calls.iter().all(|(_, verified_key)| verified_key == &key));
}

#[tokio::test]
async fn unknown_or_case_changed_pin_never_verifies_or_invokes_another_key() {
    let (_, pk) = instance_keypair();
    let key = b64(&pk);
    let (_, unknown_pk) = instance_keypair();
    let changed_case = key.to_ascii_lowercase();
    assert_ne!(changed_case, key);
    for streaming in [false, true] {
        let verifier = Arc::new(RecordingVerifier::default());
        let (provider, server) = fixture(&[("other", &key)], verifier.clone()).await;
        for unknown in [b64(&unknown_pk), changed_case.clone()] {
            assert!(matches!(
                chat(&provider, Some(&unknown), streaming).await,
                Err(CompletionError::NoPubKeyProvider(_))
            ));
        }
        assert!(verifier.calls.lock().unwrap().is_empty());
        assert!(server
            .received_requests()
            .await
            .unwrap()
            .iter()
            .all(|r| { !r.url.path().contains("evidence") && r.url.path() != "/e2e/invoke" }));
    }
}

#[tokio::test]
async fn discovery_refreshes_when_only_other_keys_have_cached_nonces() {
    for streaming in [false, true] {
        let (dk, pk) = instance_keypair();
        let key = b64(&pk);
        let (provider, server) = fixture(&[("pinned", &key)], Arc::default()).await;
        *provider.chute_cache("chute").lock().await = CachedInstances {
            instances: vec![client::E2eInstance {
                instance_id: "other".into(),
                e2e_pubkey: b64(&[42; 1184]),
                nonces: vec!["unused".into()],
            }],
            expires_at: std::time::Instant::now() + std::time::Duration::from_secs(60),
        };
        let _invoke = mount_invoke(&server, "pinned", dk, streaming).await;
        chat(&provider, Some(&key), streaming).await.unwrap();
    }
}

#[tokio::test]
async fn key_rotation_fails_closed_but_unconstrained_chat_can_use_the_new_key() {
    for streaming in [false, true] {
        let (old_dk, old_pk) = instance_keypair();
        let old_key = b64(&old_pk);
        let (new_dk, new_pk) = instance_keypair();
        let new_key = b64(&new_pk);
        let verifier = Arc::new(RecordingVerifier::default());
        let (provider, server) = fixture(&[("pinned", &old_key)], verifier.clone()).await;
        {
            let _invoke = mount_invoke(&server, "pinned", old_dk, streaming).await;
            chat(&provider, Some(&old_key), streaming).await.unwrap();
        }
        // The old key's only nonce was consumed. The next request must refresh
        // discovery, where the SAME instance id now advertises a different key.
        Mock::given(path("/e2e/instances/chute"))
            .respond_with(discovery(&[("pinned", &new_key)]))
            .with_priority(1)
            .mount(&server)
            .await;
        assert!(matches!(
            chat(&provider, Some(&old_key), streaming).await,
            Err(CompletionError::NoPubKeyProvider(_))
        ));
        assert_eq!(verifier.calls.lock().unwrap().len(), 1);
        let _invoke = mount_invoke(&server, "pinned", new_dk, streaming).await;
        chat(&provider, None, streaming).await.unwrap();
        assert_eq!(verifier.calls.lock().unwrap()[1].1, new_key);
    }
}

#[tokio::test]
async fn matching_key_does_not_bypass_attestation() {
    for streaming in [false, true] {
        let (_, pk) = instance_keypair();
        let key = b64(&pk);
        let verifier = Arc::new(RecordingVerifier {
            reject: true,
            ..Default::default()
        });
        let (provider, server) = fixture(
            &[("pinned", &key), ("other", &b64(&[42; 1184]))],
            verifier.clone(),
        )
        .await;
        assert!(matches!(
            chat(&provider, Some(&key), streaming).await,
            Err(CompletionError::CompletionError(_))
        ));
        assert_eq!(*verifier.calls.lock().unwrap(), [("pinned".into(), key)]);
        assert!(server
            .received_requests()
            .await
            .unwrap()
            .iter()
            .all(|r| r.url.path() != "/e2e/invoke"));
    }
}

#[tokio::test]
async fn nonce_consumption_rechecks_key_after_concurrent_cache_rotation() {
    let (provider, _server) = fixture(&[], Arc::default()).await;
    *provider.chute_cache("chute").lock().await = CachedInstances {
        instances: vec![client::E2eInstance {
            instance_id: "same-id".into(),
            e2e_pubkey: "new-key".into(),
            nonces: vec!["new-nonce".into()],
        }],
        expires_at: std::time::Instant::now() + std::time::Duration::from_secs(60),
    };
    assert!(provider
        .take_nonce("chute", "same-id", "old-key")
        .await
        .is_none());
    assert_eq!(
        provider
            .take_nonce("chute", "same-id", "new-key")
            .await
            .as_deref(),
        Some("new-nonce")
    );
}
