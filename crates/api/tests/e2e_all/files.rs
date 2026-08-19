use crate::common::*;

const FILES_API_GONE_MESSAGE: &str =
    "The Files API has been deprecated and is no longer available. Manage file content in your application and use stateless POST /v1/responses requests with store: false.";

fn assert_files_api_is_gone(response: axum_test::TestResponse) {
    assert_eq!(response.status_code(), 410);

    let error = response.json::<api::models::ErrorResponse>();
    assert_eq!(error.error.r#type, "gone");
    assert_eq!(error.error.message, FILES_API_GONE_MESSAGE);
}

#[tokio::test]
async fn test_files_api_returns_gone_for_all_legacy_routes() {
    let server = setup_test_server().await;
    let (api_key, _) = create_org_and_api_key(&server).await;

    assert_files_api_is_gone(
        server
            .post("/v1/files")
            .add_header("Authorization", format!("Bearer {api_key}"))
            .json(&serde_json::json!({"purpose": "user_data"}))
            .await,
    );

    assert_files_api_is_gone(
        server
            .get("/v1/files?limit=1")
            .add_header("Authorization", format!("Bearer {api_key}"))
            .await,
    );

    assert_files_api_is_gone(
        server
            .get("/v1/files/")
            .add_header("Authorization", format!("Bearer {api_key}"))
            .await,
    );

    assert_files_api_is_gone(
        server
            .get("/v1/files/file-00000000-0000-0000-0000-000000000000")
            .add_header("Authorization", format!("Bearer {api_key}"))
            .await,
    );

    assert_files_api_is_gone(
        server
            .delete("/v1/files/file-00000000-0000-0000-0000-000000000000")
            .add_header("Authorization", format!("Bearer {api_key}"))
            .await,
    );

    assert_files_api_is_gone(
        server
            .get("/v1/files/file-00000000-0000-0000-0000-000000000000/content")
            .add_header("Authorization", format!("Bearer {api_key}"))
            .await,
    );
}

#[tokio::test]
async fn test_files_api_returns_gone_for_other_methods_and_subpaths() {
    let server = setup_test_server().await;
    let (api_key, _) = create_org_and_api_key(&server).await;

    assert_files_api_is_gone(
        server
            .patch("/v1/files/file-00000000-0000-0000-0000-000000000000")
            .add_header("Authorization", format!("Bearer {api_key}"))
            .await,
    );

    assert_files_api_is_gone(
        server
            .put("/v1/files/legacy/nested/path")
            .add_header("Authorization", format!("Bearer {api_key}"))
            .await,
    );
}

#[tokio::test]
async fn test_files_api_requires_authentication_before_returning_gone() {
    let server = setup_test_server().await;
    let (api_key, _) = create_org_and_api_key(&server).await;

    assert_files_api_is_gone(
        server
            .get("/v1/files")
            .add_header("Authorization", format!("Bearer {api_key}"))
            .await,
    );

    let invalid_key_response = server
        .get("/v1/files")
        .add_header("Authorization", "Bearer invalid_key_12345")
        .await;
    assert_eq!(invalid_key_response.status_code(), 401);

    let missing_key_response = server.get("/v1/files").await;
    assert_eq!(missing_key_response.status_code(), 401);
}

#[tokio::test]
async fn test_openapi_does_not_advertise_files_api() {
    let server = setup_test_server().await;

    let response = server.get("/api-docs/openapi.json").await;
    assert_eq!(response.status_code(), 200);

    let openapi = response.json::<serde_json::Value>();
    let paths = openapi["paths"]
        .as_object()
        .expect("OpenAPI paths must be an object");
    assert!(
        paths.keys().all(|path| !path.starts_with("/v1/files")),
        "OpenAPI must not expose retired Files API paths"
    );

    let tags = openapi["tags"]
        .as_array()
        .expect("OpenAPI tags must be an array");
    assert!(
        tags.iter().all(|tag| tag["name"] != "Files"),
        "OpenAPI must not expose the Files tag"
    );

    let schemas = openapi["components"]["schemas"]
        .as_object()
        .expect("OpenAPI schemas must be an object");
    for schema in [
        "FileUploadResponse",
        "FileListResponse",
        "FileDeleteResponse",
        "ExpiresAfter",
    ] {
        assert!(
            !schemas.contains_key(schema),
            "OpenAPI must not expose the retired {schema} schema"
        );
    }
}
