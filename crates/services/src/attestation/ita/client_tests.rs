use super::{
    client_test_support::{
        client_for, config_for_api_base_url, sample_attest_request, FakeIta, FakeStep,
    },
    http::is_connection_reset,
    ItaClient, ItaClientError,
};
use std::time::Duration;

type TestResult = Result<(), Box<dyn std::error::Error>>;

#[tokio::test]
async fn client_allows_loopback_http_fake_ita_base_url() -> TestResult {
    // Given: tests use a local fake ITA server over HTTP.
    let server = FakeIta::start(Vec::new()).await?;

    // When: the client is built from the fake ITA loopback URL.
    let client = client_for(&server, 0, 100);

    // Then: local fake ITA tests can keep using loopback HTTP.
    assert!(client.is_ok());
    Ok(())
}

#[test]
fn client_allows_localhost_http_base_url() -> TestResult {
    // Given: a local fake ITA server can also be addressed with localhost.
    let config = config_for_api_base_url("http://localhost:8080", 0)?;

    // When: the ITA client is constructed.
    let client = ItaClient::from_config_for_test(&config, Duration::from_millis(100));

    // Then: localhost HTTP remains available for local fake ITA tests.
    assert!(client.is_ok());
    Ok(())
}

#[test]
fn client_rejects_plain_http_for_non_loopback_hosts() -> TestResult {
    // Given: a configured ITA API URL that would send credentials over remote HTTP.
    let config = config_for_api_base_url("http://ita.example.test", 0)?;

    // When: the ITA client is constructed.
    let error = ItaClient::from_config_for_test(&config, Duration::from_millis(100)).unwrap_err();

    // Then: non-local HTTP is rejected before any x-api-key request can be sent.
    assert!(matches!(
        error,
        ItaClientError::InvalidConfig {
            reason: "api base URL must use HTTPS for non-loopback hosts"
        }
    ));
    Ok(())
}

#[tokio::test]
async fn client_debug_redacts_api_key_header_value() -> TestResult {
    // Given: an ITA client configured with the API key header value.
    let server = FakeIta::start(Vec::new()).await?;
    let client = client_for(&server, 0, 100)?;

    // When: diagnostic formatting is produced.
    let debug = format!("{client:?}");

    // Then: debug output keeps useful fields but never prints the API key.
    assert!(debug.contains("ItaClient"));
    assert!(debug.contains("api_key"));
    assert!(debug.contains("<redacted>"));
    assert!(!debug.contains("test-api-key"));
    Ok(())
}

#[tokio::test]
async fn get_nonce_sends_expected_method_path_and_headers() -> TestResult {
    // Given: a fake ITA upstream ready to return a verifier nonce.
    let server = FakeIta::start(vec![FakeStep::json(
        200,
        r#"{"val":"bm9uY2UtdmFs","iat":"bm9uY2UtaWF0","signature":"bm9uY2Utc2ln"}"#,
    )])
    .await?;
    let client = client_for(&server, 0, 100)?;

    // When: the nonce endpoint is called.
    let response = client.get_nonce("request-123").await?;

    // Then: the request uses the official method, path, and privacy-safe headers.
    let requests = server.requests();
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].method, "GET");
    assert_eq!(requests[0].path, "/appraisal/v2/nonce");
    assert_eq!(requests[0].header("x-api-key"), Some("test-api-key"));
    assert_eq!(requests[0].header("accept"), Some("application/json"));
    assert_eq!(requests[0].header("request-id"), Some("request-123"));
    assert_eq!(response.nonce.val, "bm9uY2UtdmFs");
    assert_eq!(response.nonce.iat, "bm9uY2UtaWF0");
    assert_eq!(response.nonce.signature, "bm9uY2Utc2ln");
    assert_eq!(response.nonce.nonce_material()?, b"nonce-valnonce-iat");
    Ok(())
}

#[tokio::test]
async fn attest_sends_expected_method_path_headers_and_body() -> TestResult {
    // Given: a fake ITA upstream ready to return a JWT wrapper.
    let server = FakeIta::start(vec![FakeStep::json(
        200,
        r#"{"token":"header.payload.signature"}"#,
    )])
    .await?;
    let client = client_for(&server, 0, 100)?;
    let body = sample_attest_request();

    // When: the attest endpoint is called.
    let response = client.attest("request-456", &body).await?;

    // Then: the request uses the official method, path, headers, and typed JSON body.
    let requests = server.requests();
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].method, "POST");
    assert_eq!(requests[0].path, "/appraisal/v2/attest");
    assert_eq!(requests[0].header("x-api-key"), Some("test-api-key"));
    assert_eq!(requests[0].header("accept"), Some("application/json"));
    assert_eq!(requests[0].header("content-type"), Some("application/json"));
    assert_eq!(requests[0].header("request-id"), Some("request-456"));
    let sent: serde_json::Value = serde_json::from_slice(&requests[0].body)?;
    assert_eq!(sent["tdx"]["quote"], "base64-quote");
    assert_eq!(response.token, "header.payload.signature");
    Ok(())
}

#[tokio::test]
async fn client_does_not_retry_non_transient_statuses() -> TestResult {
    for status in [400, 401, 403] {
        // Given: ITA returns a caller/config/policy failure status.
        let server =
            FakeIta::start(vec![FakeStep::json(status, r#"{"error":"rejected"}"#)]).await?;
        let client = client_for(&server, 2, 100)?;

        // When: the nonce endpoint observes that status.
        let error = client.get_nonce("request-non-transient").await.unwrap_err();

        // Then: the status is surfaced without retrying.
        assert!(matches!(
            error,
            ItaClientError::NonRetryableStatus { status: observed }
                if observed.as_u16() == status
        ));
        assert_eq!(server.requests().len(), 1);
    }
    Ok(())
}

#[tokio::test]
async fn client_retries_transient_statuses_up_to_cap() -> TestResult {
    for status in [429, 502, 503, 504] {
        // Given: a transient ITA status twice, then a valid nonce.
        let server = FakeIta::start(vec![
            FakeStep::json(status, r#"{}"#),
            FakeStep::json(status, r#"{}"#),
            FakeStep::json(
                200,
                r#"{"val":"cmV0cnktdmFs","iat":"cmV0cnktaWF0","signature":"cmV0cnktc2ln"}"#,
            ),
        ])
        .await?;
        let client = client_for(&server, 2, 100)?;

        // When: the nonce endpoint is called.
        let response = client.get_nonce("request-retry").await?;

        // Then: retries are bounded by the configured cap and eventually succeed.
        assert_eq!(response.nonce.val, "cmV0cnktdmFs");
        assert_eq!(server.requests().len(), 3);
    }
    Ok(())
}

#[tokio::test]
async fn client_rejects_malformed_base64_nonce_without_retry() -> TestResult {
    // Given: ITA returns the official nonce fields but one byte field is malformed.
    let server = FakeIta::start(vec![FakeStep::json(
        200,
        r#"{"val":"not base64","iat":"aWF0","signature":"c2ln"}"#,
    )])
    .await?;
    let client = client_for(&server, 2, 100)?;

    // When: the nonce response is decoded at the client boundary.
    let error = client.get_nonce("request-bad-nonce").await.unwrap_err();

    // Then: malformed verifier nonce bytes fail closed without retrying.
    assert!(matches!(error, ItaClientError::InvalidVerifierNonce { .. }));
    assert_eq!(server.requests().len(), 1);
    Ok(())
}

#[tokio::test]
async fn client_does_not_retry_non_reset_transport_closure() -> TestResult {
    // Given: the upstream closes before sending a response.
    let server = FakeIta::start(vec![
        FakeStep::CloseBeforeResponse,
        FakeStep::json(
            200,
            r#"{"val":"cmV0cnktdmFs","iat":"cmV0cnktaWF0","signature":"cmV0cnktc2ln"}"#,
        ),
    ])
    .await?;
    let client = client_for(&server, 2, 100)?;

    // When: the nonce endpoint observes a non-reset transport failure.
    let error = client
        .get_nonce("request-closed-transport")
        .await
        .unwrap_err();

    // Then: only connection-reset transport failures are eligible for retry.
    assert!(matches!(
        error,
        ItaClientError::Transport {
            retryable: false,
            ..
        }
    ));
    assert_eq!(server.requests().len(), 1);
    Ok(())
}

#[tokio::test]
async fn client_retries_connection_reset_and_succeeds() -> TestResult {
    // Given: ITA resets the first connection, then returns a valid nonce.
    let server = FakeIta::start(vec![
        FakeStep::ResetConnection,
        FakeStep::json(
            200,
            r#"{"val":"cmVzZXQtdmFs","iat":"cmVzZXQtaWF0","signature":"cmVzZXQtc2ln"}"#,
        ),
    ])
    .await?;
    let client = client_for(&server, 1, 100)?;

    // When: the nonce endpoint observes a connection reset.
    let response = client.get_nonce("request-reset-retry").await?;

    // Then: the client retries the reset and returns the successful nonce response.
    assert_eq!(response.nonce.val, "cmVzZXQtdmFs");
    assert_eq!(server.requests().len(), 2);
    Ok(())
}

#[test]
fn retry_classifier_matches_connection_reset_only() {
    // Given: transport error messages from reset and non-reset closures.
    let reset = "connection reset by peer";
    let closed = "connection closed before message completed";

    // When/Then: the retry classifier accepts reset but not generic closure.
    assert!(is_connection_reset(reset));
    assert!(!is_connection_reset(closed));
}

#[tokio::test]
async fn retry_after_is_preserved_when_rate_limit_retry_is_exhausted() -> TestResult {
    // Given: ITA keeps returning 429 with Retry-After.
    let server = FakeIta::start(vec![
        FakeStep::json(429, r#"{}"#).with_header("Retry-After", "2"),
        FakeStep::json(429, r#"{}"#).with_header("Retry-After", "2"),
    ])
    .await?;
    let client = client_for(&server, 1, 25)?;

    // When: the retry cap is exhausted.
    let error = client.get_nonce("request-rate-limit").await.unwrap_err();

    // Then: the typed error retains Retry-After for API-layer propagation.
    assert!(matches!(
        error,
        ItaClientError::RateLimited { retry_after: Some(value) } if value == "2"
    ));
    assert_eq!(server.requests().len(), 2);
    Ok(())
}

#[tokio::test]
async fn timeout_retries_and_maps_to_typed_timeout_error() -> TestResult {
    // Given: every ITA connection stalls past the configured timeout.
    let server = FakeIta::start(vec![FakeStep::Hang, FakeStep::Hang]).await?;
    let client = client_for(&server, 1, 25)?;

    // When: the nonce endpoint is called.
    let error = client.get_nonce("request-timeout").await.unwrap_err();

    // Then: timeout is retried up to the cap and returned as a typed timeout.
    assert!(matches!(error, ItaClientError::Timeout));
    assert_eq!(server.requests().len(), 2);
    Ok(())
}

#[tokio::test]
async fn oversized_and_malformed_nonce_responses_are_upstream_errors() -> TestResult {
    let scenarios = [
        FakeStep::body(200, "x".repeat(4097)),
        FakeStep::body(200, "{not-json".to_string()),
    ];

    for step in scenarios {
        // Given: ITA returns an invalid response body.
        let server = FakeIta::start(vec![step]).await?;
        let client = client_for(&server, 0, 100)?;

        // When: the body is parsed at the client boundary.
        let error = client.get_nonce("request-invalid").await.unwrap_err();

        // Then: invalid upstream nonce material maps to a typed upstream response error.
        assert!(matches!(error, ItaClientError::UpstreamResponse { .. }));
    }
    Ok(())
}

#[tokio::test]
async fn token_response_rejects_oversized_malformed_and_missing_token_bodies() -> TestResult {
    let oversized_token = format!(r#"{{"token":"{}"}}"#, "a".repeat(64 * 1024));
    let scenarios = [
        FakeStep::body(200, oversized_token),
        FakeStep::body(200, "{not-json".to_string()),
        FakeStep::json(200, r#"{"token":""}"#),
        FakeStep::json(200, r#"{}"#),
    ];

    for step in scenarios {
        // Given: ITA returns valid JSON without a usable token.
        let server = FakeIta::start(vec![step]).await?;
        let client = client_for(&server, 0, 100)?;

        // When: the attest endpoint parses the response.
        let error = client
            .attest("request-missing-token", &sample_attest_request())
            .await
            .unwrap_err();

        // Then: the client fails closed without exposing response content.
        assert!(matches!(error, ItaClientError::UpstreamResponse { .. }));
    }
    Ok(())
}
