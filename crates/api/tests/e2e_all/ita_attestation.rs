use api::models::BatchUpdateModelApiRequest;
use axum::http::header::RETRY_AFTER;
use serde_json::{json, Value};

use crate::common::{fake_ita::FakeIta, fake_ita::FakeItaMode, fake_ita::ObservedAttest, *};

const ITA_TOKEN_PATH: &str = "/v1/attestation/ita-token";
const NONCE: &str = "0000000000000000000000000000000000000000000000000000000000000001";
const HEADER_MODEL_ALIAS_RESOLVED: &str = "x-model-alias-resolved";
const HEADER_NO_ALIASING: &str = "x-no-aliasing";
const POLICY_A: &str = "11111111-1111-4111-8111-111111111111";
const ENV_POLICY: &str = "22222222-2222-4222-8222-222222222222";

#[tokio::test]
async fn ita_token_is_public_and_returns_gateway_token_when_fake_ita_succeeds() {
    // Given: ITA is enabled against a local fake upstream.
    let fake_ita = FakeIta::start(FakeItaMode::Success).await;
    let server = setup_ita_server(&fake_ita, ItaServerMode::Enabled { max_retries: 0 }).await;

    // When: a caller requests an ITA token without an Authorization header.
    let response = server
        .get(&format!(
            "{ITA_TOKEN_PATH}?nonce={NONCE}&policy_ids={POLICY_A}&policy_must_match=true"
        ))
        .await;

    // Then: the public endpoint returns the gateway token and calls ITA nonce before attest.
    assert_eq!(response.status_code(), 200, "{}", response.text());
    let body: Value = response.json();
    assert_eq!(body["gateway"]["token"], "gateway.jwt");
    assert_eq!(body["gateway"]["token_type"], "JWT");
    assert_eq!(body["models"].as_array().map(Vec::len), Some(0));
    assert_eq!(
        body["jwks_url"], "https://portal.example.test/certs",
        "JWKS URL should point at the configured ITA portal certs surface"
    );
    assert_eq!(
        fake_ita.paths(),
        vec!["/appraisal/v2/nonce", "/appraisal/v2/attest"]
    );
}

#[tokio::test]
async fn ita_token_policy_query_overrides_env_defaults() {
    // Given: env defaults exist, but the caller supplies a policy override.
    let fake_ita = FakeIta::start(FakeItaMode::Success).await;
    let server = setup_ita_server_with_env_policy(&fake_ita, ENV_POLICY).await;

    // When: the caller supplies policy query parameters.
    let response = server
        .get(&format!(
            "{ITA_TOKEN_PATH}?nonce={NONCE}&policy_ids={POLICY_A}&policy_must_match=true"
        ))
        .await;

    // Then: the API response and fake ITA request use the query policy, not the env default.
    assert_eq!(response.status_code(), 200, "{}", response.text());
    let body: Value = response.json();
    assert_eq!(body["policy_ids"], json!([POLICY_A]));
    assert_eq!(body["policy_must_match"], true);
    assert_eq!(
        fake_ita.attest_observations(),
        vec![ObservedAttest {
            policy_ids: vec![POLICY_A.to_string()],
            policy_must_match: true,
        }]
    );
}

#[tokio::test]
async fn ita_token_disabled_returns_service_unavailable_without_auth() {
    // Given: ITA is disabled.
    let fake_ita = FakeIta::start(FakeItaMode::Success).await;
    let server = setup_ita_server(&fake_ita, ItaServerMode::Disabled).await;

    // When: a caller requests the public endpoint without an Authorization header.
    let response = server.get(&format!("{ITA_TOKEN_PATH}?nonce={NONCE}")).await;

    // Then: the route reaches ITA config handling instead of API-key auth.
    assert_eq!(response.status_code(), 503, "{}", response.text());
    assert!(fake_ita.paths().is_empty());
}

#[tokio::test]
async fn ita_token_rate_limit_preserves_retry_after_header() {
    // Given: ITA rate-limits appraisal and asks the caller to retry after seven seconds
    // (a value distinct from the router's default of 2, so preservation is provable).
    let fake_ita = FakeIta::start(FakeItaMode::RateLimited).await;
    let server = setup_ita_server(&fake_ita, ItaServerMode::Enabled { max_retries: 0 }).await;

    // When: a caller requests an ITA token through the HTTP surface.
    let response = server.get(&format!("{ITA_TOKEN_PATH}?nonce={NONCE}")).await;

    // Then: the endpoint returns 429 and preserves the upstream Retry-After.
    assert_eq!(response.status_code(), 429, "{}", response.text());
    let retry_after = response
        .headers()
        .get(RETRY_AFTER)
        .and_then(|value| value.to_str().ok());
    assert_eq!(retry_after, Some("7"));
}

#[tokio::test]
async fn ita_token_rejects_alias_when_no_aliasing_is_requested() {
    // Given: a model alias exists and ITA is configured.
    let fake_ita = FakeIta::start(FakeItaMode::Success).await;
    let server = setup_ita_server(&fake_ita, ItaServerMode::Enabled { max_retries: 0 }).await;
    let alias = setup_deprecated_alias(&server).await;
    let encoded_alias = url::form_urlencoded::byte_serialize(alias.as_bytes()).collect::<String>();

    // When: the caller opts into strict no-aliasing behavior.
    let response = server
        .get(&format!(
            "{ITA_TOKEN_PATH}?model={encoded_alias}&nonce={NONCE}"
        ))
        .add_header(HEADER_NO_ALIASING, "true")
        .await;

    // Then: the route rejects before calling fake ITA, preserving fail-closed alias semantics.
    assert_eq!(response.status_code(), 400, "{}", response.text());
    assert!(
        response.text().contains(E2E_QWEN_MODEL_NAME),
        "rejection should name canonical model: {}",
        response.text()
    );
    assert!(fake_ita.paths().is_empty());
}

#[tokio::test]
async fn ita_token_announces_alias_when_alias_is_served() {
    // Given: a model alias exists and ITA is configured.
    let fake_ita = FakeIta::start(FakeItaMode::Success).await;
    let server = setup_ita_server(&fake_ita, ItaServerMode::Enabled { max_retries: 0 }).await;
    let alias = setup_deprecated_alias(&server).await;
    let encoded_alias = url::form_urlencoded::byte_serialize(alias.as_bytes()).collect::<String>();

    // When: the caller requests through the alias without strict rejection.
    let response = server
        .get(&format!(
            "{ITA_TOKEN_PATH}?model={encoded_alias}&nonce={NONCE}"
        ))
        .await;

    // Then: the endpoint announces the alias resolution on a served response.
    assert_eq!(response.status_code(), 200, "{}", response.text());
    let header = response
        .headers()
        .get(HEADER_MODEL_ALIAS_RESOLVED)
        .and_then(|value| value.to_str().ok());
    let expected_header = format!("{alias} -> {E2E_QWEN_MODEL_NAME}");
    assert_eq!(header, Some(expected_header.as_str()));
    let body: Value = response.json();
    assert_eq!(body["models"].as_array().map(Vec::len), Some(1));
    assert_eq!(body["models"][0]["model"], E2E_QWEN_MODEL_NAME);
    assert_eq!(body["models"][0]["token"], "gateway.jwt");
}

#[tokio::test]
async fn ita_token_manual_qa_curl_style_success_and_rate_limit() {
    // Given: local fake ITA upstreams exercise success and rate-limit surfaces.
    let success_ita = FakeIta::start(FakeItaMode::Success).await;
    let success_server =
        setup_ita_server(&success_ita, ItaServerMode::Enabled { max_retries: 0 }).await;
    let rate_limited_ita = FakeIta::start(FakeItaMode::RateLimited).await;
    let rate_limited_server =
        setup_ita_server(&rate_limited_ita, ItaServerMode::Enabled { max_retries: 0 }).await;

    // When: the endpoint is driven through the HTTP test harness in curl-style scenarios.
    let success_path =
        format!("{ITA_TOKEN_PATH}?nonce={NONCE}&policy_ids={POLICY_A}&policy_must_match=true");
    let success = success_server.get(&success_path).await;
    print_curl_style_response(&success_path, &success);

    let rate_limited_path = format!("{ITA_TOKEN_PATH}?nonce={NONCE}");
    let rate_limited = rate_limited_server.get(&rate_limited_path).await;
    print_curl_style_response(&rate_limited_path, &rate_limited);

    // Then: the observable HTTP surface matches the Task 6 manual QA contract.
    assert_eq!(success.status_code(), 200, "{}", success.text());
    let success_body: Value = success.json();
    assert_eq!(success_body["gateway"]["token"], "gateway.jwt");
    assert_eq!(
        success_ita.paths(),
        vec!["/appraisal/v2/nonce", "/appraisal/v2/attest"]
    );

    assert_eq!(rate_limited.status_code(), 429, "{}", rate_limited.text());
    let retry_after = rate_limited
        .headers()
        .get(RETRY_AFTER)
        .and_then(|value| value.to_str().ok());
    assert_eq!(retry_after, Some("7"));
}

fn print_curl_style_response(path: &str, response: &axum_test::TestResponse) {
    println!("$ curl -i 'http://testserver{path}'");
    println!("HTTP/1.1 {}", response.status_code());
    if let Some(content_type) = response
        .headers()
        .get(axum::http::header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
    {
        println!("content-type: {content_type}");
    }
    if let Some(retry_after) = response
        .headers()
        .get(RETRY_AFTER)
        .and_then(|value| value.to_str().ok())
    {
        println!("retry-after: {retry_after}");
    }
    println!();
    println!("{}", response.text());
}

async fn setup_deprecated_alias(server: &axum_test::TestServer) -> String {
    setup_qwen_model(server).await;

    let alias = format!("test-ita-alias/{}", uuid::Uuid::new_v4());
    let mut batch = BatchUpdateModelApiRequest::new();
    batch.insert(
        alias.clone(),
        serde_json::from_value(json!({
            "inputCostPerToken": { "amount": 1_000_000, "currency": "USD" },
            "outputCostPerToken": { "amount": 2_000_000, "currency": "USD" },
            "modelDisplayName": "ITA Alias Test Model",
            "modelDescription": "Synthetic model deprecated onto Qwen for ITA e2e",
            "contextLength": 4096,
            "maxOutputLength": 1024,
            "verifiable": false,
            "isActive": true
        }))
        .expect("test model request should deserialize"),
    );
    admin_batch_upsert_models(server, batch, get_session_id()).await;

    let response = server
        .post("/v1/admin/models/deprecate")
        .add_header("Authorization", format!("Bearer {}", get_session_id()))
        .add_header("User-Agent", MOCK_USER_AGENT)
        .json(&json!({
            "modelId": alias,
            "successorModelId": E2E_QWEN_MODEL_NAME,
            "changeReason": "ITA alias e2e"
        }))
        .await;
    assert_eq!(response.status_code(), 200, "{}", response.text());
    alias
}
