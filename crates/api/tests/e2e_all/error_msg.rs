use crate::common::*;

// ============================================
// Models public endpoint tests
// ============================================

#[tokio::test]
async fn test_models_no_auth_required() {
    let server = setup_test_server().await;

    let response = server.get("/v1/models").await;

    assert_eq!(response.status_code(), 200);
    let models = response.json::<api::models::ModelsResponse>();
    assert_eq!(models.object, "list");
}

#[tokio::test]
async fn test_models_invalid_auth_header_ignored() {
    let server = setup_test_server().await;

    let response = server
        .get("/v1/models")
        .add_header("Authorization", "Token abc")
        .await;

    assert_eq!(response.status_code(), 200);
    let models = response.json::<api::models::ModelsResponse>();
    assert_eq!(models.object, "list");
}

#[tokio::test]
async fn test_models_invalid_api_key_ignored() {
    let server = setup_test_server().await;

    let response = server
        .get("/v1/models")
        .add_header("Authorization", "Bearer invalid_key_123")
        .await;

    assert_eq!(response.status_code(), 200);
    let models = response.json::<api::models::ModelsResponse>();
    assert_eq!(models.object, "list");
}
