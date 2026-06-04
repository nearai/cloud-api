//! E2E test for the public health endpoint.
//!
//! Ported from infra-tests `check_health` (`tests/cloud_api_helpers.py`), which
//! probes `GET /v1/health` on the live deployment. Here we assert the same
//! contract end-to-end through the assembled router: the endpoint is public (no
//! auth) and reports a healthy status.

use crate::common::*;

#[tokio::test]
async fn test_health_endpoint_public_and_ok() {
    let server = setup_test_server().await;

    let response = server.get("/v1/health").await;

    assert_eq!(
        response.status_code(),
        200,
        "health endpoint should be reachable without auth, got: {}",
        response.text()
    );
    let body = response.json::<serde_json::Value>();
    assert_eq!(
        body["status"].as_str(),
        Some("ok"),
        "health endpoint should report status=ok"
    );
}
