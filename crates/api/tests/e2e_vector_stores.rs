mod common;

use common::*;
use services::id_prefixes::{PREFIX_FILE, PREFIX_VS, PREFIX_VSFB};
use services::rag::MockRagServiceTrait;
use std::sync::Arc;

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Upload a test file via the files API (creates a real DB record).
async fn upload_test_file(server: &axum_test::TestServer, api_key: &str) -> serde_json::Value {
    let response = server
        .post("/v1/files")
        .add_header("Authorization", format!("Bearer {api_key}"))
        .multipart(
            axum_test::multipart::MultipartForm::new()
                .add_text("purpose", "assistants")
                .add_part(
                    "file",
                    axum_test::multipart::Part::bytes(b"test file content".to_vec())
                        .file_name("test.txt")
                        .mime_type("text/plain"),
                ),
        )
        .await;
    assert_eq!(response.status_code(), 201);
    response.json::<serde_json::Value>()
}

/// Build a minimal vector store JSON response (as returned by RAG, no prefixes).
fn rag_vector_store_response(id: &str) -> serde_json::Value {
    serde_json::json!({
        "id": id,
        "object": "vector_store",
        "name": "Test VS",
        "created_at": 1700000000,
        "status": "completed",
        "usage_bytes": 0,
        "file_counts": {
            "in_progress": 0,
            "completed": 0,
            "failed": 0,
            "cancelled": 0,
            "total": 0
        }
    })
}

/// Build a minimal vector store file JSON response (as returned by RAG, no prefixes).
fn rag_vs_file_response(file_id: &str, vs_id: &str) -> serde_json::Value {
    serde_json::json!({
        "id": file_id,
        "object": "vector_store.file",
        "created_at": 1700000000,
        "vector_store_id": vs_id,
        "status": "completed",
        "usage_bytes": 100,
        "file_id": file_id
    })
}

/// Build a minimal file batch JSON response (as returned by RAG, no prefixes).
fn rag_file_batch_response(batch_id: &str, vs_id: &str) -> serde_json::Value {
    serde_json::json!({
        "id": batch_id,
        "object": "vector_store.file_batch",
        "created_at": 1700000000,
        "vector_store_id": vs_id,
        "status": "completed",
        "file_counts": {
            "in_progress": 0,
            "completed": 1,
            "failed": 0,
            "cancelled": 0,
            "total": 1
        }
    })
}

// ---------------------------------------------------------------------------
// Vector Store CRUD
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_create_vector_store() {
    let vs_uuid = uuid::Uuid::new_v4();
    let vs_uuid_str = vs_uuid.to_string();

    let mut mock_rag = MockRagServiceTrait::new();

    let uuid_for_return = vs_uuid_str.clone();
    mock_rag
        .expect_create_vector_store()
        .times(1)
        .withf(|body| body.get("name").and_then(|v| v.as_str()) == Some("My Store"))
        .returning(move |_body| Ok(rag_vector_store_response(&uuid_for_return)));

    let (server, _guard) = setup_test_server_with_rag(Arc::new(mock_rag)).await;
    let (api_key, _) = create_org_and_api_key(&server).await;

    let response = server
        .post("/v1/vector_stores")
        .add_header("Authorization", format!("Bearer {api_key}"))
        .json(&serde_json::json!({"name": "My Store"}))
        .await;

    assert_eq!(response.status_code(), 200);
    let body: serde_json::Value = response.json();
    assert_eq!(body["object"], "vector_store");
    let id = body["id"].as_str().unwrap();
    assert!(id.starts_with(PREFIX_VS));
    assert_eq!(id, format!("{PREFIX_VS}{vs_uuid_str}"));
}

#[tokio::test]
async fn test_list_vector_stores() {
    let vs_uuid = uuid::Uuid::new_v4();
    let vs_uuid_str = vs_uuid.to_string();

    let mut mock_rag = MockRagServiceTrait::new();

    // Setup: create_vector_store
    let uuid_c = vs_uuid_str.clone();
    mock_rag
        .expect_create_vector_store()
        .times(1)
        .returning(move |_| Ok(rag_vector_store_response(&uuid_c)));

    // Test: list_vector_stores
    let uuid_l = vs_uuid_str.clone();
    mock_rag
        .expect_list_vector_stores()
        .times(1)
        .withf(move |rag_ids| rag_ids.len() == 1 && rag_ids[0] == uuid_l)
        .returning(move |rag_ids| {
            Ok(serde_json::json!({
                "object": "list",
                "data": [rag_vector_store_response(&rag_ids[0])]
            }))
        });

    let (server, _guard) = setup_test_server_with_rag(Arc::new(mock_rag)).await;
    let (api_key, _) = create_org_and_api_key(&server).await;

    // Create a VS first so there's a local ref
    server
        .post("/v1/vector_stores")
        .add_header("Authorization", format!("Bearer {api_key}"))
        .json(&serde_json::json!({"name": "Test VS"}))
        .await;

    let response = server
        .get("/v1/vector_stores")
        .add_header("Authorization", format!("Bearer {api_key}"))
        .await;

    assert_eq!(response.status_code(), 200);
    let body: serde_json::Value = response.json();
    assert_eq!(body["object"], "list");
    let data = body["data"].as_array().unwrap();
    assert_eq!(data.len(), 1);
    assert!(data[0]["id"].as_str().unwrap().starts_with(PREFIX_VS));
}

#[tokio::test]
async fn test_get_vector_store() {
    let vs_uuid = uuid::Uuid::new_v4();
    let vs_uuid_str = vs_uuid.to_string();

    let mut mock_rag = MockRagServiceTrait::new();

    let uuid_c = vs_uuid_str.clone();
    mock_rag
        .expect_create_vector_store()
        .times(1)
        .returning(move |_| Ok(rag_vector_store_response(&uuid_c)));

    let uuid_g = vs_uuid_str.clone();
    mock_rag
        .expect_get_vector_store()
        .times(1)
        .withf(move |rag_id| rag_id == uuid_g)
        .returning(move |rag_id| Ok(rag_vector_store_response(rag_id)));

    let (server, _guard) = setup_test_server_with_rag(Arc::new(mock_rag)).await;
    let (api_key, _) = create_org_and_api_key(&server).await;

    server
        .post("/v1/vector_stores")
        .add_header("Authorization", format!("Bearer {api_key}"))
        .json(&serde_json::json!({"name": "Test VS"}))
        .await;

    let response = server
        .get(&format!("/v1/vector_stores/{PREFIX_VS}{vs_uuid_str}"))
        .add_header("Authorization", format!("Bearer {api_key}"))
        .await;

    assert_eq!(response.status_code(), 200);
    let body: serde_json::Value = response.json();
    assert_eq!(body["object"], "vector_store");
    assert_eq!(body["id"], format!("{PREFIX_VS}{vs_uuid_str}"));
}

#[tokio::test]
async fn test_modify_vector_store() {
    let vs_uuid = uuid::Uuid::new_v4();
    let vs_uuid_str = vs_uuid.to_string();

    let mut mock_rag = MockRagServiceTrait::new();

    let uuid_c = vs_uuid_str.clone();
    mock_rag
        .expect_create_vector_store()
        .times(1)
        .returning(move |_| Ok(rag_vector_store_response(&uuid_c)));

    let uuid_u = vs_uuid_str.clone();
    mock_rag
        .expect_update_vector_store()
        .times(1)
        .withf(move |rag_id, body| {
            rag_id == uuid_u && body.get("name").and_then(|v| v.as_str()) == Some("Updated Name")
        })
        .returning(move |rag_id, _body| {
            let mut resp = rag_vector_store_response(rag_id);
            resp["name"] = serde_json::json!("Updated Name");
            Ok(resp)
        });

    let (server, _guard) = setup_test_server_with_rag(Arc::new(mock_rag)).await;
    let (api_key, _) = create_org_and_api_key(&server).await;

    server
        .post("/v1/vector_stores")
        .add_header("Authorization", format!("Bearer {api_key}"))
        .json(&serde_json::json!({"name": "Test VS"}))
        .await;

    let response = server
        .post(&format!("/v1/vector_stores/{PREFIX_VS}{vs_uuid_str}"))
        .add_header("Authorization", format!("Bearer {api_key}"))
        .json(&serde_json::json!({"name": "Updated Name"}))
        .await;

    assert_eq!(response.status_code(), 200);
    let body: serde_json::Value = response.json();
    assert_eq!(body["name"], "Updated Name");
    assert_eq!(body["id"], format!("{PREFIX_VS}{vs_uuid_str}"));
}

#[tokio::test]
async fn test_delete_vector_store() {
    let vs_uuid = uuid::Uuid::new_v4();
    let vs_uuid_str = vs_uuid.to_string();

    let mut mock_rag = MockRagServiceTrait::new();

    let uuid_c = vs_uuid_str.clone();
    mock_rag
        .expect_create_vector_store()
        .times(1)
        .returning(move |_| Ok(rag_vector_store_response(&uuid_c)));

    let uuid_d = vs_uuid_str.clone();
    mock_rag
        .expect_delete_vector_store()
        .times(1)
        .withf(move |rag_id| rag_id == uuid_d)
        .returning(move |rag_id| {
            Ok(serde_json::json!({
                "id": rag_id,
                "object": "vector_store.deleted",
                "deleted": true
            }))
        });

    let (server, _guard) = setup_test_server_with_rag(Arc::new(mock_rag)).await;
    let (api_key, _) = create_org_and_api_key(&server).await;

    server
        .post("/v1/vector_stores")
        .add_header("Authorization", format!("Bearer {api_key}"))
        .json(&serde_json::json!({"name": "Test VS"}))
        .await;

    let response = server
        .delete(&format!("/v1/vector_stores/{PREFIX_VS}{vs_uuid_str}"))
        .add_header("Authorization", format!("Bearer {api_key}"))
        .await;

    assert_eq!(response.status_code(), 200);
    let body: serde_json::Value = response.json();
    assert_eq!(body["deleted"], true);
    assert_eq!(body["id"], format!("{PREFIX_VS}{vs_uuid_str}"));
    assert_eq!(body["object"], "vector_store.deleted");
}

// ---------------------------------------------------------------------------
// Vector Store Search
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_search_vector_store() {
    let vs_uuid = uuid::Uuid::new_v4();
    let vs_uuid_str = vs_uuid.to_string();
    let file_uuid = uuid::Uuid::new_v4();
    let file_uuid_str = file_uuid.to_string();

    let mut mock_rag = MockRagServiceTrait::new();

    let uuid_c = vs_uuid_str.clone();
    mock_rag
        .expect_create_vector_store()
        .times(1)
        .returning(move |_| Ok(rag_vector_store_response(&uuid_c)));

    let uuid_s = vs_uuid_str.clone();
    let file_s = file_uuid_str.clone();
    mock_rag
        .expect_search_vector_store()
        .times(1)
        .withf(move |rag_vs_id, body| {
            rag_vs_id == uuid_s && body.get("query").and_then(|v| v.as_str()) == Some("test query")
        })
        .returning(move |_rag_vs_id, _body| {
            Ok(serde_json::json!({
                "object": "vector_store.search_results.page",
                "search_query": "test query",
                "data": [{
                    "file_id": file_s,
                    "filename": "test.txt",
                    "score": 0.95,
                    "content": [{"type": "text", "text": "result text"}]
                }],
                "has_more": false
            }))
        });

    let (server, _guard) = setup_test_server_with_rag(Arc::new(mock_rag)).await;
    let (api_key, _) = create_org_and_api_key(&server).await;

    server
        .post("/v1/vector_stores")
        .add_header("Authorization", format!("Bearer {api_key}"))
        .json(&serde_json::json!({"name": "Test VS"}))
        .await;

    let response = server
        .post(&format!(
            "/v1/vector_stores/{PREFIX_VS}{vs_uuid_str}/search"
        ))
        .add_header("Authorization", format!("Bearer {api_key}"))
        .json(&serde_json::json!({"query": "test query"}))
        .await;

    assert_eq!(response.status_code(), 200);
    let body: serde_json::Value = response.json();
    assert_eq!(body["object"], "vector_store.search_results.page");
    let data = body["data"].as_array().unwrap();
    assert_eq!(data.len(), 1);
    assert!(data[0]["file_id"]
        .as_str()
        .unwrap()
        .starts_with(PREFIX_FILE));
}

// ---------------------------------------------------------------------------
// Vector Store Files
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_create_vector_store_file() {
    let vs_uuid = uuid::Uuid::new_v4();
    let vs_uuid_str = vs_uuid.to_string();

    let mut mock_rag = MockRagServiceTrait::new();

    let uuid_c = vs_uuid_str.clone();
    mock_rag
        .expect_create_vector_store()
        .times(1)
        .returning(move |_| Ok(rag_vector_store_response(&uuid_c)));

    let uuid_a = vs_uuid_str.clone();
    mock_rag
        .expect_attach_file()
        .times(1)
        .withf(move |rag_vs_id, body| {
            rag_vs_id == uuid_a
                && body.get("file_id").and_then(|v| v.as_str()).is_some()
                && body.get("filename").and_then(|v| v.as_str()) == Some("test.txt")
                && body.get("storage_key").and_then(|v| v.as_str()).is_some()
        })
        .returning(move |rag_vs_id, body| {
            let file_id = body["file_id"].as_str().unwrap().to_string();
            Ok(rag_vs_file_response(&file_id, rag_vs_id))
        });

    let (server, _guard) = setup_test_server_with_rag(Arc::new(mock_rag)).await;
    let (api_key, _) = create_org_and_api_key(&server).await;

    // Create VS
    server
        .post("/v1/vector_stores")
        .add_header("Authorization", format!("Bearer {api_key}"))
        .json(&serde_json::json!({"name": "Test VS"}))
        .await;

    // Upload a real file
    let file_resp = upload_test_file(&server, &api_key).await;
    let file_id = file_resp["id"].as_str().unwrap();

    let response = server
        .post(&format!("/v1/vector_stores/{PREFIX_VS}{vs_uuid_str}/files"))
        .add_header("Authorization", format!("Bearer {api_key}"))
        .json(&serde_json::json!({"file_id": file_id}))
        .await;

    assert_eq!(response.status_code(), 200);
    let body: serde_json::Value = response.json();
    assert_eq!(body["object"], "vector_store.file");
    assert!(body["id"].as_str().unwrap().starts_with(PREFIX_FILE));
    assert!(body["vector_store_id"]
        .as_str()
        .unwrap()
        .starts_with(PREFIX_VS));
}

#[tokio::test]
async fn test_list_vector_store_files() {
    let vs_uuid = uuid::Uuid::new_v4();
    let vs_uuid_str = vs_uuid.to_string();
    let file_uuid = uuid::Uuid::new_v4();
    let file_uuid_str = file_uuid.to_string();

    let mut mock_rag = MockRagServiceTrait::new();

    let uuid_c = vs_uuid_str.clone();
    mock_rag
        .expect_create_vector_store()
        .times(1)
        .returning(move |_| Ok(rag_vector_store_response(&uuid_c)));

    let uuid_l = vs_uuid_str.clone();
    let file_l = file_uuid_str.clone();
    mock_rag
        .expect_list_vs_files()
        .times(1)
        .withf(move |rag_vs_id, _qs| rag_vs_id == uuid_l)
        .returning(move |rag_vs_id, _qs| {
            Ok(serde_json::json!({
                "object": "list",
                "data": [rag_vs_file_response(&file_l, rag_vs_id)],
                "has_more": false
            }))
        });

    let (server, _guard) = setup_test_server_with_rag(Arc::new(mock_rag)).await;
    let (api_key, _) = create_org_and_api_key(&server).await;

    server
        .post("/v1/vector_stores")
        .add_header("Authorization", format!("Bearer {api_key}"))
        .json(&serde_json::json!({"name": "Test VS"}))
        .await;

    let response = server
        .get(&format!("/v1/vector_stores/{PREFIX_VS}{vs_uuid_str}/files"))
        .add_header("Authorization", format!("Bearer {api_key}"))
        .await;

    assert_eq!(response.status_code(), 200);
    let body: serde_json::Value = response.json();
    assert_eq!(body["object"], "list");
    let data = body["data"].as_array().unwrap();
    assert_eq!(data.len(), 1);
    assert!(data[0]["id"].as_str().unwrap().starts_with(PREFIX_FILE));
}

#[tokio::test]
async fn test_get_vector_store_file() {
    let vs_uuid = uuid::Uuid::new_v4();
    let vs_uuid_str = vs_uuid.to_string();

    let mut mock_rag = MockRagServiceTrait::new();

    let uuid_c = vs_uuid_str.clone();
    mock_rag
        .expect_create_vector_store()
        .times(1)
        .returning(move |_| Ok(rag_vector_store_response(&uuid_c)));

    let uuid_g = vs_uuid_str.clone();
    mock_rag
        .expect_get_vs_file()
        .times(1)
        .withf(move |rag_vs_id, _rag_file_id| rag_vs_id == uuid_g)
        .returning(move |rag_vs_id, rag_file_id| Ok(rag_vs_file_response(rag_file_id, rag_vs_id)));

    let (server, _guard) = setup_test_server_with_rag(Arc::new(mock_rag)).await;
    let (api_key, _) = create_org_and_api_key(&server).await;

    server
        .post("/v1/vector_stores")
        .add_header("Authorization", format!("Bearer {api_key}"))
        .json(&serde_json::json!({"name": "Test VS"}))
        .await;

    // Upload a real file so verify_file() passes
    let file_resp = upload_test_file(&server, &api_key).await;
    let file_id = file_resp["id"].as_str().unwrap();

    let response = server
        .get(&format!(
            "/v1/vector_stores/{PREFIX_VS}{vs_uuid_str}/files/{file_id}"
        ))
        .add_header("Authorization", format!("Bearer {api_key}"))
        .await;

    assert_eq!(response.status_code(), 200);
    let body: serde_json::Value = response.json();
    assert_eq!(body["object"], "vector_store.file");
    assert!(body["id"].as_str().unwrap().starts_with(PREFIX_FILE));
    assert!(body["vector_store_id"]
        .as_str()
        .unwrap()
        .starts_with(PREFIX_VS));
}

#[tokio::test]
async fn test_update_vector_store_file() {
    let vs_uuid = uuid::Uuid::new_v4();
    let vs_uuid_str = vs_uuid.to_string();

    let mut mock_rag = MockRagServiceTrait::new();

    let uuid_c = vs_uuid_str.clone();
    mock_rag
        .expect_create_vector_store()
        .times(1)
        .returning(move |_| Ok(rag_vector_store_response(&uuid_c)));

    let uuid_u = vs_uuid_str.clone();
    mock_rag
        .expect_update_vs_file()
        .times(1)
        .withf(move |rag_vs_id, _rag_file_id, body| {
            rag_vs_id == uuid_u
                && body
                    .get("attributes")
                    .and_then(|v| v.get("color"))
                    .and_then(|v| v.as_str())
                    == Some("blue")
        })
        .returning(move |rag_vs_id, rag_file_id, _body| {
            let mut resp = rag_vs_file_response(rag_file_id, rag_vs_id);
            resp["attributes"] = serde_json::json!({"color": "blue"});
            Ok(resp)
        });

    let (server, _guard) = setup_test_server_with_rag(Arc::new(mock_rag)).await;
    let (api_key, _) = create_org_and_api_key(&server).await;

    server
        .post("/v1/vector_stores")
        .add_header("Authorization", format!("Bearer {api_key}"))
        .json(&serde_json::json!({"name": "Test VS"}))
        .await;

    let file_resp = upload_test_file(&server, &api_key).await;
    let file_id = file_resp["id"].as_str().unwrap();

    let response = server
        .post(&format!(
            "/v1/vector_stores/{PREFIX_VS}{vs_uuid_str}/files/{file_id}"
        ))
        .add_header("Authorization", format!("Bearer {api_key}"))
        .json(&serde_json::json!({"attributes": {"color": "blue"}}))
        .await;

    assert_eq!(response.status_code(), 200);
    let body: serde_json::Value = response.json();
    assert_eq!(body["object"], "vector_store.file");
    assert_eq!(body["attributes"]["color"], "blue");
}

#[tokio::test]
async fn test_delete_vector_store_file() {
    let vs_uuid = uuid::Uuid::new_v4();
    let vs_uuid_str = vs_uuid.to_string();

    let mut mock_rag = MockRagServiceTrait::new();

    let uuid_c = vs_uuid_str.clone();
    mock_rag
        .expect_create_vector_store()
        .times(1)
        .returning(move |_| Ok(rag_vector_store_response(&uuid_c)));

    let uuid_d = vs_uuid_str.clone();
    mock_rag
        .expect_detach_file()
        .times(1)
        .withf(move |rag_vs_id, _rag_file_id| rag_vs_id == uuid_d)
        .returning(move |_rag_vs_id, rag_file_id| {
            Ok(serde_json::json!({
                "id": rag_file_id,
                "object": "vector_store.file.deleted",
                "deleted": true
            }))
        });

    let (server, _guard) = setup_test_server_with_rag(Arc::new(mock_rag)).await;
    let (api_key, _) = create_org_and_api_key(&server).await;

    server
        .post("/v1/vector_stores")
        .add_header("Authorization", format!("Bearer {api_key}"))
        .json(&serde_json::json!({"name": "Test VS"}))
        .await;

    let file_resp = upload_test_file(&server, &api_key).await;
    let file_id = file_resp["id"].as_str().unwrap();

    let response = server
        .delete(&format!(
            "/v1/vector_stores/{PREFIX_VS}{vs_uuid_str}/files/{file_id}"
        ))
        .add_header("Authorization", format!("Bearer {api_key}"))
        .await;

    assert_eq!(response.status_code(), 200);
    let body: serde_json::Value = response.json();
    assert_eq!(body["deleted"], true);
    assert!(body["id"].as_str().unwrap().starts_with(PREFIX_FILE));
}

// ---------------------------------------------------------------------------
// Vector Store File Batches
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_create_file_batch() {
    let vs_uuid = uuid::Uuid::new_v4();
    let vs_uuid_str = vs_uuid.to_string();
    let batch_uuid = uuid::Uuid::new_v4();
    let batch_uuid_str = batch_uuid.to_string();

    let mut mock_rag = MockRagServiceTrait::new();

    let uuid_c = vs_uuid_str.clone();
    mock_rag
        .expect_create_vector_store()
        .times(1)
        .returning(move |_| Ok(rag_vector_store_response(&uuid_c)));

    let uuid_b = vs_uuid_str.clone();
    let batch_b = batch_uuid_str.clone();
    mock_rag
        .expect_create_file_batch()
        .times(1)
        .withf(move |rag_vs_id, body| {
            rag_vs_id == uuid_b
                && body.get("file_ids").and_then(|v| v.as_array()).is_some()
                && body.get("file_metadata").is_some()
        })
        .returning(move |rag_vs_id, _body| Ok(rag_file_batch_response(&batch_b, rag_vs_id)));

    let (server, _guard) = setup_test_server_with_rag(Arc::new(mock_rag)).await;
    let (api_key, _) = create_org_and_api_key(&server).await;

    server
        .post("/v1/vector_stores")
        .add_header("Authorization", format!("Bearer {api_key}"))
        .json(&serde_json::json!({"name": "Test VS"}))
        .await;

    let file_resp = upload_test_file(&server, &api_key).await;
    let file_id = file_resp["id"].as_str().unwrap();

    let response = server
        .post(&format!(
            "/v1/vector_stores/{PREFIX_VS}{vs_uuid_str}/file_batches"
        ))
        .add_header("Authorization", format!("Bearer {api_key}"))
        .json(&serde_json::json!({"file_ids": [file_id]}))
        .await;

    assert_eq!(response.status_code(), 200);
    let body: serde_json::Value = response.json();
    assert_eq!(body["object"], "vector_store.file_batch");
    assert!(body["id"].as_str().unwrap().starts_with(PREFIX_VSFB));
    assert!(body["vector_store_id"]
        .as_str()
        .unwrap()
        .starts_with(PREFIX_VS));
}

#[tokio::test]
async fn test_get_file_batch() {
    let vs_uuid = uuid::Uuid::new_v4();
    let vs_uuid_str = vs_uuid.to_string();
    let batch_uuid = uuid::Uuid::new_v4();
    let batch_uuid_str = batch_uuid.to_string();

    let mut mock_rag = MockRagServiceTrait::new();

    let uuid_c = vs_uuid_str.clone();
    mock_rag
        .expect_create_vector_store()
        .times(1)
        .returning(move |_| Ok(rag_vector_store_response(&uuid_c)));

    let uuid_g = vs_uuid_str.clone();
    let batch_g = batch_uuid_str.clone();
    mock_rag
        .expect_get_file_batch()
        .times(1)
        .withf(move |rag_vs_id, rag_batch_id| rag_vs_id == uuid_g && rag_batch_id == batch_g)
        .returning(move |rag_vs_id, rag_batch_id| {
            Ok(rag_file_batch_response(rag_batch_id, rag_vs_id))
        });

    let (server, _guard) = setup_test_server_with_rag(Arc::new(mock_rag)).await;
    let (api_key, _) = create_org_and_api_key(&server).await;

    server
        .post("/v1/vector_stores")
        .add_header("Authorization", format!("Bearer {api_key}"))
        .json(&serde_json::json!({"name": "Test VS"}))
        .await;

    let response = server
        .get(&format!(
            "/v1/vector_stores/{PREFIX_VS}{vs_uuid_str}/file_batches/{PREFIX_VSFB}{batch_uuid_str}"
        ))
        .add_header("Authorization", format!("Bearer {api_key}"))
        .await;

    assert_eq!(response.status_code(), 200);
    let body: serde_json::Value = response.json();
    assert_eq!(body["object"], "vector_store.file_batch");
    assert_eq!(body["id"], format!("{PREFIX_VSFB}{batch_uuid_str}"));
    assert_eq!(body["vector_store_id"], format!("{PREFIX_VS}{vs_uuid_str}"));
}

#[tokio::test]
async fn test_cancel_file_batch() {
    let vs_uuid = uuid::Uuid::new_v4();
    let vs_uuid_str = vs_uuid.to_string();
    let batch_uuid = uuid::Uuid::new_v4();
    let batch_uuid_str = batch_uuid.to_string();

    let mut mock_rag = MockRagServiceTrait::new();

    let uuid_c = vs_uuid_str.clone();
    mock_rag
        .expect_create_vector_store()
        .times(1)
        .returning(move |_| Ok(rag_vector_store_response(&uuid_c)));

    let uuid_x = vs_uuid_str.clone();
    let batch_x = batch_uuid_str.clone();
    mock_rag
        .expect_cancel_file_batch()
        .times(1)
        .withf(move |rag_vs_id, rag_batch_id| rag_vs_id == uuid_x && rag_batch_id == batch_x)
        .returning(move |rag_vs_id, rag_batch_id| {
            let mut resp = rag_file_batch_response(rag_batch_id, rag_vs_id);
            resp["status"] = serde_json::json!("cancelled");
            Ok(resp)
        });

    let (server, _guard) = setup_test_server_with_rag(Arc::new(mock_rag)).await;
    let (api_key, _) = create_org_and_api_key(&server).await;

    server
        .post("/v1/vector_stores")
        .add_header("Authorization", format!("Bearer {api_key}"))
        .json(&serde_json::json!({"name": "Test VS"}))
        .await;

    let response = server
        .post(&format!(
            "/v1/vector_stores/{PREFIX_VS}{vs_uuid_str}/file_batches/{PREFIX_VSFB}{batch_uuid_str}/cancel"
        ))
        .add_header("Authorization", format!("Bearer {api_key}"))
        .await;

    assert_eq!(response.status_code(), 200);
    let body: serde_json::Value = response.json();
    assert_eq!(body["object"], "vector_store.file_batch");
    assert_eq!(body["id"], format!("{PREFIX_VSFB}{batch_uuid_str}"));
    assert_eq!(body["status"], "cancelled");
}

#[tokio::test]
async fn test_list_file_batch_files() {
    let vs_uuid = uuid::Uuid::new_v4();
    let vs_uuid_str = vs_uuid.to_string();
    let batch_uuid = uuid::Uuid::new_v4();
    let batch_uuid_str = batch_uuid.to_string();
    let file_uuid = uuid::Uuid::new_v4();
    let file_uuid_str = file_uuid.to_string();

    let mut mock_rag = MockRagServiceTrait::new();

    let uuid_c = vs_uuid_str.clone();
    mock_rag
        .expect_create_vector_store()
        .times(1)
        .returning(move |_| Ok(rag_vector_store_response(&uuid_c)));

    let uuid_l = vs_uuid_str.clone();
    let batch_l = batch_uuid_str.clone();
    let file_l = file_uuid_str.clone();
    mock_rag
        .expect_list_batch_files()
        .times(1)
        .withf(move |rag_vs_id, rag_batch_id, _qs| rag_vs_id == uuid_l && rag_batch_id == batch_l)
        .returning(move |rag_vs_id, _rag_batch_id, _qs| {
            Ok(serde_json::json!({
                "object": "list",
                "data": [rag_vs_file_response(&file_l, rag_vs_id)],
                "has_more": false
            }))
        });

    let (server, _guard) = setup_test_server_with_rag(Arc::new(mock_rag)).await;
    let (api_key, _) = create_org_and_api_key(&server).await;

    server
        .post("/v1/vector_stores")
        .add_header("Authorization", format!("Bearer {api_key}"))
        .json(&serde_json::json!({"name": "Test VS"}))
        .await;

    let response = server
        .get(&format!(
            "/v1/vector_stores/{PREFIX_VS}{vs_uuid_str}/file_batches/{PREFIX_VSFB}{batch_uuid_str}/files"
        ))
        .add_header("Authorization", format!("Bearer {api_key}"))
        .await;

    assert_eq!(response.status_code(), 200);
    let body: serde_json::Value = response.json();
    assert_eq!(body["object"], "list");
    let data = body["data"].as_array().unwrap();
    assert_eq!(data.len(), 1);
    assert!(data[0]["id"].as_str().unwrap().starts_with(PREFIX_FILE));
}
