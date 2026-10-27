use super::{Config, Provider};
use crate::{InferenceProvider, SystemOneRequest};
use serde_json::json;
use std::collections::HashMap;
use wiremock::{
    matchers::{header, method, path},
    Mock, MockServer, ResponseTemplate,
};

#[test]
fn systemone_distributes_requests_and_spills_concurrent_duplicates() {
    for count in 1..=4 {
        let provider = Provider::new(Config {
            base_url: "http://jev.fleet.test".into(),
            api_key: None,
            completion_timeout_seconds: 1,
            control_timeout_seconds: 1,
        });
        provider.set_backend_count(count);
        let mut reached = std::collections::BTreeSet::new();
        for request in 0..256 {
            let lease = provider
                .fleet
                .acquire_systemone_index(&request.to_string(), None)
                .unwrap();
            reached.insert(lease.index());
        }
        assert_eq!(reached.len(), count);
        let leases: Vec<_> = (0..4 * count)
            .map(|_| {
                provider
                    .fleet
                    .acquire_systemone_index("same-request", None)
                    .unwrap()
            })
            .collect();
        for index in 0..count {
            assert_eq!(
                leases.iter().filter(|lease| lease.index() == index).count(),
                4
            );
        }
        drop(leases);
        assert_eq!(provider.fleet.active_prefix_loads(), 0);
    }
}

#[tokio::test]
async fn systemone_fleet_fails_over_and_pins_receipt_to_serving_backend() {
    for status in [503, 429, 408] {
        fleet_case(status, false, false).await;
    }
}

#[tokio::test]
async fn systemone_fleet_does_not_replay_invalid_responses_or_read_timeouts() {
    fleet_case(200, true, false).await;
    fleet_case(200, false, true).await;
    fleet_case(422, false, false).await;
}

#[tokio::test]
async fn systemone_fleet_uses_chat_key_policy_for_requests_and_retries() {
    for keys in [
        HashMap::from([("selected-key".into(), vec![0, 2])]),
        HashMap::from([("selected-key".into(), vec![1])]),
        HashMap::new(),
        HashMap::from([("other-key".into(), vec![2])]),
        HashMap::from([("selected-key".into(), vec![3])]),
    ] {
        let server = MockServer::start().await;
        let port = server.address().port();
        let provider = fleet_provider(&server);
        provider.fleet.set_backend_keys(keys);
        let first = provider
            .fleet
            .acquire_index(&[], Some("selected-key"))
            .unwrap()
            .index();
        let allowed: Vec<_> = std::iter::once(first)
            .chain(
                provider
                    .fleet
                    .fallback_indices_for(first, Some("selected-key")),
            )
            .collect();
        for index in 0..3 {
            Mock::given(method("POST"))
                .and(path("/v1/systemone"))
                .and(header("host", format!("jev-i{index}.fleet.test:{port}")))
                .respond_with(ResponseTemplate::new(503))
                .expect(u64::from(allowed.contains(&index)))
                .mount(&server)
                .await;
        }
        let request: SystemOneRequest = serde_json::from_value(json!({
            "model":"jev", "state":"x",
            "questions":{"q":{"type":"noul","instructions":"Is this about billing?"}}
        }))
        .unwrap();
        let response = provider
            .systemone(request, "hash".into(), Some("selected-key"))
            .await;
        assert!(matches!(
            response,
            Err(crate::CompletionError::HttpError {
                status_code: 503,
                ..
            })
        ));
        assert_eq!(provider.fleet.active_prefix_loads(), 0);
    }
}

fn fleet_provider(server: &MockServer) -> Provider {
    let port = server.address().port();
    let provider = Provider::new(Config {
        base_url: format!("http://jev.fleet.test:{port}"),
        api_key: None,
        completion_timeout_seconds: 1,
        control_timeout_seconds: 1,
    });
    provider.set_backend_count(3);
    let mut builder = reqwest::Client::builder().no_proxy();
    for index in 0..3 {
        builder = builder.resolve(&format!("jev-i{index}.fleet.test"), *server.address());
    }
    let client = builder.build().unwrap();
    for index in 0..3 {
        *provider.fleet.index_clients[index].lock().unwrap() = Some(client.clone());
    }
    provider
}

async fn fleet_case(status: u16, malformed: bool, timeout: bool) {
    let server = MockServer::start().await;
    let port = server.address().port();
    let provider = fleet_provider(&server);
    let first = provider
        .fleet
        .acquire_systemone_index("original-hash", None)
        .unwrap()
        .index();
    let next = provider.fleet.fallback_indices_for(first, None)[0];
    let retry = matches!(status, 408 | 429 | 503);
    for index in 0..3 {
        let id = format!("decision:tee.{index}/receipt?version=1");
        let value = json!({"id":id,"model":"jev","answers":{"q":{"type":"noul","noul":0.8}},
            "usage":{"input_tokens":7,"output_tokens":1}});
        let mut template = ResponseTemplate::new(if index == first { status } else { 200 })
            .set_body_json(if index == first && malformed {
                json!({"usage":null})
            } else {
                value
            });
        if index == first && timeout {
            template = template.set_delay(std::time::Duration::from_secs(2));
        }
        Mock::given(method("POST"))
            .and(path("/v1/systemone"))
            .and(header("host", format!("jev-i{index}.fleet.test:{port}")))
            .and(header("x-request-hash", "original-hash"))
            .respond_with(template)
            .expect(u64::from(index == first || (retry && index == next)))
            .mount(&server)
            .await;
    }
    let request: SystemOneRequest = serde_json::from_value(json!({"model":"jev","state":"x",
        "questions":{"q":{"type":"noul","instructions":"Is this about billing?"}}}))
    .unwrap();
    let response = provider
        .systemone(request, "original-hash".into(), Some("selected-key"))
        .await;
    if retry {
        let id = format!("decision:tee.{next}/receipt?version=1");
        assert_eq!(response.unwrap().provider_decision_id().unwrap(), id);
        assert_eq!(
            provider.fleet.signature_rotation.lock().unwrap().get(&id),
            Some(&(next as u64))
        );
        Mock::given(method("GET"))
            .and(path(format!("/v1/signature/{}", urlencoding::encode(&id))))
            .and(header("host", format!("jev-i{next}.fleet.test:{port}")))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(json!({"text":"hash:hash",
                "signature":"signature","signing_address":"address","signing_algo":"ecdsa"})),
            )
            .expect(1)
            .mount(&server)
            .await;
        provider
            .get_signature(&id, Some("ecdsa".into()))
            .await
            .unwrap();
        provider.unpin_chat_connection(&id);
    } else {
        let error = response.unwrap_err();
        assert!(matches!(
            (status, timeout, malformed, error),
            (200, true, _, crate::CompletionError::Timeout { .. })
                | (200, false, true, crate::CompletionError::InvalidResponse(_))
                | (
                    422,
                    _,
                    _,
                    crate::CompletionError::HttpError {
                        status_code: 422,
                        ..
                    },
                )
        ));
    }
    assert!(provider.fleet.signature_rotation.lock().unwrap().is_empty());
    assert_eq!(provider.fleet.active_prefix_loads(), 0);
}
