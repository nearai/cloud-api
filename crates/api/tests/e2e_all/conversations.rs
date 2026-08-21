use crate::common::*;
use axum::http::Method;

const UNKNOWN_CONVERSATION_ID: &str = "conv_00000000-0000-0000-0000-000000000000";

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

fn assert_conversation_write_is_gone(response: axum_test::TestResponse) {
    assert_eq!(response.status_code(), 410);
    assert_no_store(&response);

    let error = response.json::<api::models::ErrorResponse>();
    assert_eq!(error.error.r#type, "gone");
    assert_eq!(
        error.error.code.as_deref(),
        Some("conversation_write_disabled")
    );
    assert!(error.error.message.contains("read-only"));
}

#[tokio::test]
async fn conversation_migration_views_require_an_api_key() {
    let server = setup_test_server().await;

    assert_eq!(
        server
            .post("/v1/conversations/batch")
            .json(&serde_json::json!({ "ids": [UNKNOWN_CONVERSATION_ID] }))
            .await
            .status_code(),
        401
    );
    assert_eq!(
        server
            .get(&format!("/v1/conversations/{UNKNOWN_CONVERSATION_ID}"))
            .await
            .status_code(),
        401
    );
    assert_eq!(
        server
            .get(&format!(
                "/v1/conversations/{UNKNOWN_CONVERSATION_ID}/items"
            ))
            .await
            .status_code(),
        401
    );
}

#[tokio::test]
async fn conversation_migration_views_reach_read_handlers_and_are_no_store() {
    let server = setup_test_server().await;
    let (api_key, _) = create_org_and_api_key(&server).await;

    let batch = server
        .post("/v1/conversations/batch")
        .add_header("Authorization", format!("Bearer {api_key}"))
        .json(&serde_json::json!({ "ids": [UNKNOWN_CONVERSATION_ID] }))
        .await;
    assert_eq!(batch.status_code(), 200);
    assert_no_store(&batch);
    let batch = batch.json::<api::models::ConversationBatchResponse>();
    assert_eq!(batch.missing_ids, vec![UNKNOWN_CONVERSATION_ID.to_string()]);

    for path in [
        format!("/v1/conversations/{UNKNOWN_CONVERSATION_ID}"),
        format!("/v1/conversations/{UNKNOWN_CONVERSATION_ID}/items"),
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
async fn conversation_writes_and_unlisted_reads_remain_gone_after_authentication() {
    let server = setup_test_server().await;
    let (api_key, _) = create_org_and_api_key(&server).await;
    let routes = [
        (Method::POST, "/v1/conversations"),
        (Method::GET, "/v1/conversations"),
        (
            Method::POST,
            "/v1/conversations/conv_00000000-0000-0000-0000-000000000000",
        ),
        (
            Method::DELETE,
            "/v1/conversations/conv_00000000-0000-0000-0000-000000000000",
        ),
        (
            Method::POST,
            "/v1/conversations/conv_00000000-0000-0000-0000-000000000000/pin",
        ),
        (
            Method::DELETE,
            "/v1/conversations/conv_00000000-0000-0000-0000-000000000000/pin",
        ),
        (
            Method::POST,
            "/v1/conversations/conv_00000000-0000-0000-0000-000000000000/archive",
        ),
        (
            Method::DELETE,
            "/v1/conversations/conv_00000000-0000-0000-0000-000000000000/archive",
        ),
        (
            Method::POST,
            "/v1/conversations/conv_00000000-0000-0000-0000-000000000000/clone",
        ),
        (
            Method::POST,
            "/v1/conversations/conv_00000000-0000-0000-0000-000000000000/items",
        ),
        (
            Method::PATCH,
            "/v1/conversations/conv_00000000-0000-0000-0000-000000000000/unknown",
        ),
    ];

    for (method, path) in routes {
        assert_conversation_write_is_gone(
            server
                .method(method.clone(), path)
                .add_header("Authorization", format!("Bearer {api_key}"))
                .await,
        );
    }
}

#[tokio::test]
async fn openapi_advertises_only_temporary_read_only_migration_views() {
    let server = setup_test_server().await;
    let response = server.get("/api-docs/openapi.json").await;
    assert_eq!(response.status_code(), 200);

    let openapi = response.json::<serde_json::Value>();
    let paths = openapi["paths"]
        .as_object()
        .expect("OpenAPI paths must be an object");

    for (path, method) in [
        ("/v1/conversations/batch", "post"),
        ("/v1/conversations/{conversation_id}", "get"),
        ("/v1/conversations/{conversation_id}/items", "get"),
        ("/v1/files", "get"),
        ("/v1/files/{file_id}", "get"),
        ("/v1/files/{file_id}/content", "get"),
    ] {
        let operation = paths.get(path).and_then(|path_item| path_item.get(method));
        assert!(
            operation.is_some_and(serde_json::Value::is_object),
            "OpenAPI must advertise temporary read-only migration view: {method} {path}"
        );
        assert_eq!(
            operation.unwrap()["security"],
            serde_json::json!([{ "api_key": [] }]),
            "{method} {path} must require an API key"
        );
        assert!(
            operation.unwrap()["responses"]["200"]["headers"]["Cache-Control"].is_object(),
            "{method} {path} must document Cache-Control: no-store"
        );
    }

    for (path, method) in [
        ("/v1/conversations", "post"),
        ("/v1/conversations/{conversation_id}", "post"),
        ("/v1/conversations/{conversation_id}", "delete"),
        ("/v1/conversations/{conversation_id}/pin", "post"),
        ("/v1/conversations/{conversation_id}/archive", "post"),
        ("/v1/conversations/{conversation_id}/clone", "post"),
        ("/v1/conversations/{conversation_id}/items", "post"),
        ("/v1/files", "post"),
        ("/v1/files/{file_id}", "delete"),
    ] {
        assert!(
            paths
                .get(path)
                .and_then(|path_item| path_item.get(method))
                .is_none(),
            "OpenAPI must not advertise disabled mutation: {method} {path}"
        );
    }

    let tags = openapi["tags"]
        .as_array()
        .expect("OpenAPI tags must be an array");
    for tag_name in ["Conversations", "Files"] {
        let description = tags
            .iter()
            .find(|tag| tag["name"] == tag_name)
            .and_then(|tag| tag["description"].as_str())
            .unwrap_or_else(|| panic!("missing {tag_name} tag description"));
        assert!(
            description.contains("Temporary authenticated, workspace-scoped read access"),
            "{tag_name} must be documented as a temporary workspace-scoped read surface"
        );
        assert!(
            description.contains("410 Gone"),
            "{tag_name} must document disabled mutations"
        );
        assert!(
            description.contains("Cache-Control: no-store"),
            "{tag_name} must document the no-store response policy"
        );
    }

    let schemas = openapi["components"]["schemas"]
        .as_object()
        .expect("OpenAPI schemas must be an object");
    for schema in [
        "ConversationObject",
        "ConversationItemList",
        "BatchConversationsRequest",
        "ConversationBatchResponse",
        "FileUploadResponse",
        "FileListResponse",
    ] {
        assert!(
            schemas.contains_key(schema),
            "OpenAPI must expose temporary migration-view schema {schema}"
        );
    }
    for schema in [
        "CreateConversationRequest",
        "UpdateConversationRequest",
        "CreateConversationItemsRequest",
        "ConversationDeleteResult",
        "FileDeleteResponse",
        "ExpiresAfter",
    ] {
        assert!(
            !schemas.contains_key(schema),
            "OpenAPI must not expose disabled mutation schema {schema}"
        );
    }
}
