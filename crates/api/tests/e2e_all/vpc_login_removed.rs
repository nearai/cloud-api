use crate::common::*;
use axum::http::StatusCode;

/// The VPC login route has been removed and must not be served.
#[tokio::test]
async fn vpc_login_route_returns_not_found() {
    let server = setup_test_server().await;

    let response = server
        .post("/v1/auth/vpc/login")
        .json(&serde_json::json!({
            "timestamp": 0,
            "signature": "00",
            "client_id": "client",
        }))
        .await;

    assert_eq!(response.status_code(), StatusCode::NOT_FOUND);
}
