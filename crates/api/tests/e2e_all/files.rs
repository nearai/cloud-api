use crate::common::*;
use axum::http::Method;

const UNKNOWN_FILE_ID: &str = "file-00000000-0000-0000-0000-000000000000";

fn assert_no_store(response: &axum_test::TestResponse) {
    assert_eq!(
        response
            .headers()
            .get("cache-control")
            .and_then(|value| value.to_str().ok()),
        Some("no-store"),
        "temporary confidential-data migration views must not be cacheable"
    );
}

fn assert_file_write_is_gone(response: axum_test::TestResponse) {
    assert_eq!(response.status_code(), 410);
    assert_no_store(&response);

    let error = response.json::<api::models::ErrorResponse>();
    assert_eq!(error.error.r#type, "gone");
    assert!(error.error.message.contains("read-only"));
}

#[tokio::test]
async fn file_migration_views_require_an_api_key() {
    let server = setup_test_server().await;

    for path in [
        "/v1/files".to_string(),
        format!("/v1/files/{UNKNOWN_FILE_ID}"),
        format!("/v1/files/{UNKNOWN_FILE_ID}/content"),
    ] {
        assert_eq!(
            server.get(&path).await.status_code(),
            401,
            "temporary view must require an API key: {path}"
        );
    }
}

#[tokio::test]
async fn file_migration_views_reach_read_handlers_and_are_no_store() {
    let server = setup_test_server().await;
    let (api_key, _) = create_org_and_api_key(&server).await;

    let list = server
        .get("/v1/files?limit=1")
        .add_header("Authorization", format!("Bearer {api_key}"))
        .await;
    assert_eq!(list.status_code(), 200);
    assert_no_store(&list);

    for path in [
        format!("/v1/files/{UNKNOWN_FILE_ID}"),
        format!("/v1/files/{UNKNOWN_FILE_ID}/content"),
    ] {
        let response = server
            .get(&path)
            .add_header("Authorization", format!("Bearer {api_key}"))
            .await;
        assert_eq!(
            response.status_code(),
            404,
            "temporary view must reach its workspace-scoped read handler: {path}"
        );
        assert_no_store(&response);
    }
}

#[tokio::test]
async fn file_writes_and_unlisted_subpaths_remain_gone_after_authentication() {
    let server = setup_test_server().await;
    let (api_key, _) = create_org_and_api_key(&server).await;
    let routes = [
        (Method::POST, "/v1/files"),
        (
            Method::DELETE,
            "/v1/files/file-00000000-0000-0000-0000-000000000000",
        ),
        (
            Method::PATCH,
            "/v1/files/file-00000000-0000-0000-0000-000000000000",
        ),
        (Method::PUT, "/v1/files/legacy/nested/path"),
        (Method::GET, "/v1/files/"),
    ];

    for (method, path) in routes {
        assert_file_write_is_gone(
            server
                .method(method.clone(), path)
                .add_header("Authorization", format!("Bearer {api_key}"))
                .await,
        );
    }
}
