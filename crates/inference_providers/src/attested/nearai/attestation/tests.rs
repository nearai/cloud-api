use super::*;
use crate::attested::nearai::{Config, Provider};
use crate::InferenceProvider;
use serde_json::json;
use std::sync::atomic::{AtomicUsize, Ordering};
use wiremock::matchers::{header, method, path, query_param};
use wiremock::{Mock, MockServer, Request, Respond, ResponseTemplate};

fn harness(server: &MockServer) -> (Provider, Client, UrlParts) {
    let base_url = format!("http://model.mock.test:{}", server.address().port());
    let provider = Provider::new(Config {
        base_url: base_url.clone(),
        api_key: Some("test-backend-token".into()),
        completion_timeout_seconds: 5,
        control_timeout_seconds: 5,
    });
    let mut builder = Client::builder()
        .no_proxy()
        .resolve("mock.test", *server.address());
    for index in 0..3 {
        builder = builder.resolve(&format!("model-i{index}.mock.test"), *server.address());
    }
    (
        provider,
        builder.build().unwrap(),
        rotation::split_inference_url(&url::Url::parse(&base_url).unwrap()).unwrap(),
    )
}

fn query(address: Option<&str>) -> ReportQuery {
    ReportQuery::new(
        "test/model".into(),
        Some("ed25519".into()),
        Some("aa".repeat(32)),
        address.map(str::to_owned),
        true,
    )
}

async fn count_mock(server: &MockServer, count: usize) {
    Mock::given(method("GET"))
        .and(path("/backends/count"))
        .and(query_param("domain", "model.mock.test"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "healthy": count,
            "total": count,
        })))
        .mount(server)
        .await;
}

async fn report_mock(server: &MockServer, index: usize, status: u16) {
    Mock::given(method("GET"))
        .and(path("/v1/attestation/report"))
        .and(header(
            "host",
            format!("model-i{index}.mock.test:{}", server.address().port()),
        ))
        .respond_with(ResponseTemplate::new(status).set_body_json(json!({
            "model_name": "test/model",
            "signing_public_key": "shared-key",
            "request_nonce": "aa".repeat(32),
            "intel_quote": format!("quote-{index}"),
            "compose_manager_attestation": { "actions": [{ "node": index }] },
        })))
        .mount(server)
        .await;
}

#[tokio::test]
async fn attestation_collection_keeps_all_shared_signer_reports_and_parameters() {
    let server = MockServer::start().await;
    count_mock(&server, 3).await;
    for index in 0..3 {
        report_mock(&server, index, 200).await;
    }
    let (provider, client, parts) = harness(&server);
    let reports = provider
        .fleet
        .collect_indexed_reports(&parts, &client, &client, &query(None))
        .await
        .unwrap();

    assert_eq!(reports.len(), 3);
    for (index, report) in reports.iter().enumerate() {
        assert_eq!(report["intel_quote"], format!("quote-{index}"));
        assert_eq!(report["signing_public_key"], "shared-key");
        assert_eq!(
            report["compose_manager_attestation"]["actions"][0]["node"],
            index
        );
    }
    let requests = server.received_requests().await.unwrap();
    assert_eq!(requests.len(), 5, "two count reads plus three backends");
    for request in requests {
        if request.url.path() == "/backends/count" {
            assert!(!request.headers.contains_key("authorization"));
        } else {
            let params: std::collections::HashMap<_, _> = request.url.query_pairs().collect();
            assert_eq!(params["model"], "test/model");
            assert_eq!(params["signing_algo"], "ed25519");
            assert_eq!(params["nonce"], "aa".repeat(32));
            assert_eq!(params["include_tls_fingerprint"], "true");
            assert_eq!(
                request.headers["authorization"],
                "Bearer test-backend-token"
            );
        }
    }
}

#[tokio::test]
async fn attestation_collection_finds_requested_signer_on_later_backend() {
    let server = MockServer::start().await;
    count_mock(&server, 3).await;
    for (index, status) in [404, 200, 404].into_iter().enumerate() {
        report_mock(&server, index, status).await;
    }
    let (provider, client, parts) = harness(&server);
    let reports = provider
        .fleet
        .collect_indexed_reports(&parts, &client, &client, &query(Some("target")))
        .await
        .unwrap();
    assert_eq!(reports.len(), 1);
    assert_eq!(reports[0]["intel_quote"], "quote-1");
    for request in server.received_requests().await.unwrap() {
        if request.url.path() == "/v1/attestation/report" {
            assert!(request
                .url
                .query_pairs()
                .any(|(key, value)| key == "signing_address" && value == "target"));
        }
    }
}

#[tokio::test]
async fn attestation_collection_reports_missing_signer_after_searching_backends() {
    let server = MockServer::start().await;
    count_mock(&server, 3).await;
    for index in 0..3 {
        report_mock(&server, index, 404).await;
    }
    let (provider, client, parts) = harness(&server);
    let result = provider
        .fleet
        .collect_indexed_reports(&parts, &client, &client, &query(Some("missing")))
        .await;
    assert!(matches!(
        result,
        Err(AttestationError::SigningAddressNotFound(address)) if address == "missing"
    ));
    assert_eq!(server.received_requests().await.unwrap().len(), 5);
}

#[tokio::test]
async fn attestation_collection_does_not_return_partial_reports_on_backend_failure() {
    for failed_status in [404, 503] {
        let server = MockServer::start().await;
        count_mock(&server, 3).await;
        for (index, status) in [200, failed_status, 200].into_iter().enumerate() {
            report_mock(&server, index, status).await;
        }
        let (provider, client, parts) = harness(&server);
        let result = provider
            .fleet
            .collect_indexed_reports(&parts, &client, &client, &query(None))
            .await;
        let Err(AttestationError::FetchError(message)) = result else {
            panic!("backend failure must reject the entire collection");
        };
        if failed_status == 404 {
            assert!(message.contains("Backend index 1"));
            assert!(message.contains("no attestation report for model test/model (HTTP 404)"));
            assert!(!message.contains("Signing address"));
        }
    }
}

struct ChangingCount(AtomicUsize);

impl Respond for ChangingCount {
    fn respond(&self, _: &Request) -> ResponseTemplate {
        let count = if self.0.fetch_add(1, Ordering::Relaxed) == 0 {
            3
        } else {
            2
        };
        ResponseTemplate::new(200).set_body_json(json!({ "healthy": count }))
    }
}

#[tokio::test]
async fn attestation_collection_rejects_count_change() {
    let server = MockServer::start().await;
    Mock::given(path("/backends/count"))
        .respond_with(ChangingCount(AtomicUsize::new(0)))
        .mount(&server)
        .await;
    for index in 0..3 {
        report_mock(&server, index, 200).await;
    }
    let (provider, client, parts) = harness(&server);
    let result = provider
        .fleet
        .collect_indexed_reports(&parts, &client, &client, &query(None))
        .await;
    assert!(matches!(
        result,
        Err(AttestationError::FetchError(message)) if message.contains("count changed")
    ));
}

struct UnavailableRecheck(AtomicUsize);

impl Respond for UnavailableRecheck {
    fn respond(&self, _: &Request) -> ResponseTemplate {
        if self.0.fetch_add(1, Ordering::Relaxed) == 0 {
            ResponseTemplate::new(200).set_body_json(json!({ "healthy": 3 }))
        } else {
            ResponseTemplate::new(503)
        }
    }
}

#[tokio::test]
async fn attestation_collection_rejects_unavailable_recheck_with_context() {
    let server = MockServer::start().await;
    Mock::given(path("/backends/count"))
        .respond_with(UnavailableRecheck(AtomicUsize::new(0)))
        .mount(&server)
        .await;
    for index in 0..3 {
        report_mock(&server, index, 200).await;
    }
    let (provider, client, parts) = harness(&server);
    let result = provider
        .fleet
        .collect_indexed_reports(&parts, &client, &client, &query(None))
        .await;
    assert!(matches!(
        result,
        Err(AttestationError::FetchError(message))
            if message.contains("count recheck failed after querying 3 backends")
                && message.contains("count_status: 503")
    ));
    assert_eq!(server.received_requests().await.unwrap().len(), 5);
}

#[tokio::test]
async fn attestation_collection_rejects_empty_or_oversized_count_without_probing() {
    for count in [0, rotation::MAX_FANOUT + 1] {
        let server = MockServer::start().await;
        count_mock(&server, count).await;
        let (provider, client, parts) = harness(&server);
        assert!(provider
            .fleet
            .collect_indexed_reports(&parts, &client, &client, &query(None))
            .await
            .is_err());
        assert_eq!(server.received_requests().await.unwrap().len(), 1);
    }
}

#[tokio::test]
async fn attestation_collection_fails_when_discovery_is_unavailable() {
    let server = MockServer::start().await;
    Mock::given(path("/backends/count"))
        .respond_with(ResponseTemplate::new(503))
        .mount(&server)
        .await;
    let (provider, client, parts) = harness(&server);
    let result = provider
        .fleet
        .collect_indexed_reports(&parts, &client, &client, &query(None))
        .await;
    assert!(matches!(result, Err(AttestationError::FetchError(_))));
    assert_eq!(server.received_requests().await.unwrap().len(), 1);
}

#[tokio::test]
async fn attestation_collection_deadline_includes_discovery_and_reports() {
    let server = MockServer::start().await;
    Mock::given(path("/backends/count"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(json!({ "healthy": 1 }))
                .set_delay(Duration::from_millis(600)),
        )
        .mount(&server)
        .await;
    Mock::given(path("/v1/attestation/report"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(json!({ "intel_quote": "report" }))
                .set_delay(Duration::from_millis(600)),
        )
        .mount(&server)
        .await;
    let (mut provider, client, parts) = harness(&server);
    Arc::get_mut(&mut provider.fleet)
        .unwrap()
        .config
        .control_timeout_seconds = 1;
    let result = provider
        .fleet
        .collect_indexed_reports(&parts, &client, &client, &query(None))
        .await;
    assert!(matches!(
        result,
        Err(AttestationError::FetchError(message)) if message == "Attestation collection timed out"
    ));
}

#[tokio::test]
async fn attestation_collection_preserves_nonrotation_provider() {
    let server = MockServer::start().await;
    Mock::given(path("/v1/attestation/report"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "intel_quote": "local-report"
        })))
        .expect(1)
        .mount(&server)
        .await;
    let provider = Provider::new(Config {
        base_url: server.uri(),
        api_key: None,
        completion_timeout_seconds: 5,
        control_timeout_seconds: 5,
    });
    let reports = provider
        .get_attestation_reports("model".into(), None, None, None, false)
        .await
        .unwrap();
    assert_eq!(reports.len(), 1);
    assert_eq!(reports[0]["intel_quote"], "local-report");
    server.verify().await;
}

#[tokio::test]
async fn attestation_collection_preserves_direct_fqdn_without_registry_probe() {
    let server = MockServer::start().await;
    Mock::given(path("/v1/attestation/report"))
        .and(header(
            "host",
            format!("host.docker.internal:{}", server.address().port()),
        ))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "intel_quote": "direct-report"
        })))
        .expect(1)
        .mount(&server)
        .await;
    let mut provider = Provider::new(Config {
        base_url: format!("http://host.docker.internal:{}", server.address().port()),
        api_key: None,
        completion_timeout_seconds: 5,
        control_timeout_seconds: 5,
    });
    Arc::get_mut(&mut provider.fleet).unwrap().client = Client::builder()
        .no_proxy()
        .resolve("host.docker.internal", *server.address())
        .build()
        .unwrap();
    let reports = provider
        .get_attestation_reports("model".into(), None, None, None, false)
        .await
        .unwrap();
    assert_eq!(reports.len(), 1);
    assert_eq!(reports[0]["intel_quote"], "direct-report");
    assert_eq!(server.received_requests().await.unwrap().len(), 1);
    server.verify().await;
}

#[test]
fn attestation_collection_requires_known_or_discovered_proxy_capability() {
    let provider_for = |base_url: &str| {
        Provider::new(Config {
            base_url: base_url.into(),
            api_key: None,
            completion_timeout_seconds: 5,
            control_timeout_seconds: 5,
        })
    };
    let builtin = provider_for("https://glm-5-3-flash.completions.near.ai");
    assert_eq!(builtin.fleet.backend_count(), 0);
    assert!(builtin.fleet.attestation_rotation_parts().is_some());

    let lookalike = provider_for("https://model.completions.near.ai.example.org");
    assert!(lookalike.fleet.attestation_rotation_parts().is_none());

    let custom = provider_for("https://model.custom-proxy.example.org");
    assert!(custom.fleet.attestation_rotation_parts().is_none());
    custom.set_backend_count(2);
    assert_eq!(
        custom.fleet.attestation_rotation_parts().unwrap().host,
        "model.custom-proxy.example.org"
    );
    custom.set_backend_count(0);
    assert!(
        custom.fleet.attestation_rotation_parts().is_some(),
        "an empty or unavailable registry must not downgrade a known proxy to singleton"
    );
}
