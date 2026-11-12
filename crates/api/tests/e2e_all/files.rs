use crate::common::*;
use axum::http::Method;
use services::files::storage::{MockStorage, StorageTrait};
use services::id_prefixes::PREFIX_FILE;
use std::sync::Arc;

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
    assert!(error.error.message.contains("temporarily limited"));
}

const RETAINED_CONTENT: &[u8] = b"retained file\0contents\n";

struct RetainedFileFixture {
    server: axum_test::TestServer,
    owner_key: String,
    foreign_key: String,
    file: services::files::File,
    repository: database::repositories::FileRepository,
    storage: Arc<MockStorage>,
}

/// Prepare existing data directly; public uploads are disabled during migration.
async fn retained_file_fixture() -> RetainedFileFixture {
    let storage =
        Arc::new(MockStorage::new(test_config().s3.encryption_key).expect("valid storage key"));
    let service_storage = storage.clone();
    let (server, database) =
        setup_test_server_with_domain_services(move |domain_services, database| {
            domain_services.files_service = Arc::new(services::files::FileServiceImpl::new(
                Arc::new(database::repositories::FileRepository::new(
                    database.pool().clone(),
                )),
                service_storage,
            ));
        })
        .await;
    let (owner_key, owner) = create_org_and_api_key(&server).await;
    let (foreign_key, _) = create_org_and_api_key(&server).await;
    let storage_key = format!("retained-fixture/{}", uuid::Uuid::new_v4());
    storage
        .upload(
            &storage_key,
            RETAINED_CONTENT.to_vec(),
            "application/octet-stream",
        )
        .await
        .expect("seed encrypted mock storage");
    let repository = database::repositories::FileRepository::new(database.pool().clone());
    let file = repository
        .create(services::files::CreateFileParams {
            filename: "retained.bin".to_string(),
            bytes: RETAINED_CONTENT.len() as i64,
            content_type: "application/octet-stream".to_string(),
            purpose: "user_data".to_string(),
            storage_key,
            workspace_id: owner.workspace_id.parse().unwrap(),
            uploaded_by_api_key_id: owner.id.parse().unwrap(),
            expires_at: None,
        })
        .await
        .expect("seed retained file metadata");
    RetainedFileFixture {
        server,
        owner_key,
        foreign_key,
        file,
        repository,
        storage,
    }
}

#[tokio::test]
async fn retained_files_are_readable_only_by_the_owning_workspace() {
    let fixture = retained_file_fixture().await;
    let file_id = format!("{PREFIX_FILE}{}", fixture.file.id);
    let metadata = fixture
        .server
        .get(&format!("/v1/files/{file_id}"))
        .add_header("Authorization", format!("Bearer {}", fixture.owner_key))
        .await;
    assert_eq!(metadata.status_code(), 200);
    assert_no_store(&metadata);
    let metadata: api::models::FileUploadResponse = metadata.json();
    assert_eq!(metadata.id, file_id);
    assert_eq!(metadata.filename, "retained.bin");
    assert_eq!(metadata.bytes, RETAINED_CONTENT.len() as i64);
    assert_eq!(metadata.purpose, "user_data");

    let content = fixture
        .server
        .get(&format!("/v1/files/{file_id}/content"))
        .add_header("Authorization", format!("Bearer {}", fixture.owner_key))
        .await;
    assert_eq!(content.status_code(), 200);
    assert_no_store(&content);
    assert_eq!(content.as_bytes().as_ref(), RETAINED_CONTENT);
    assert_eq!(
        content.headers().get("content-type").unwrap(),
        "application/octet-stream"
    );

    let owner_list = fixture
        .server
        .get("/v1/files")
        .add_header("Authorization", format!("Bearer {}", fixture.owner_key))
        .await;
    assert_eq!(owner_list.status_code(), 200);
    assert_no_store(&owner_list);
    let owner_list: api::models::FileListResponse = owner_list.json();
    assert_eq!(owner_list.data.len(), 1);
    assert_eq!(owner_list.data[0].id, file_id);
    assert_eq!(owner_list.data[0].filename, "retained.bin");
    assert!(!owner_list.has_more);

    let foreign_list = fixture
        .server
        .get("/v1/files")
        .add_header("Authorization", format!("Bearer {}", fixture.foreign_key))
        .await;
    assert_eq!(foreign_list.status_code(), 200);
    assert_no_store(&foreign_list);
    assert!(foreign_list
        .json::<api::models::FileListResponse>()
        .data
        .is_empty());

    for suffix in ["", "/content"] {
        let foreign = fixture
            .server
            .get(&format!("/v1/files/{file_id}{suffix}"))
            .add_header("Authorization", format!("Bearer {}", fixture.foreign_key))
            .await;
        let unknown = fixture
            .server
            .get(&format!("/v1/files/{UNKNOWN_FILE_ID}{suffix}"))
            .add_header("Authorization", format!("Bearer {}", fixture.foreign_key))
            .await;
        assert_eq!(foreign.status_code(), 404);
        assert_eq!(unknown.status_code(), 404);
        assert_no_store(&foreign);
        assert_no_store(&unknown);
        assert_eq!(foreign.text(), unknown.text());
    }
}

#[tokio::test]
async fn retained_files_paginate_in_both_orders_with_purpose_filters() {
    let fixture = retained_file_fixture().await;
    let mut files = vec![fixture.file.clone()];
    for (index, purpose) in ["assistants", "user_data", "assistants", "user_data"]
        .into_iter()
        .enumerate()
    {
        // The repository orders and cursors by created_at, without an ID tie-break.
        // Space these test-local inserts and verify their returned timestamps.
        tokio::time::sleep(std::time::Duration::from_millis(2)).await;
        let storage_key = format!("retained-fixture/{}", uuid::Uuid::new_v4());
        fixture
            .storage
            .upload(
                &storage_key,
                RETAINED_CONTENT.to_vec(),
                "application/octet-stream",
            )
            .await
            .expect("seed retained content");
        let file = fixture
            .repository
            .create(services::files::CreateFileParams {
                filename: format!("retained-{index}.bin"),
                bytes: RETAINED_CONTENT.len() as i64,
                content_type: "application/octet-stream".to_string(),
                purpose: purpose.to_string(),
                storage_key,
                workspace_id: fixture.file.workspace_id,
                uploaded_by_api_key_id: fixture.file.uploaded_by_api_key_id,
                expires_at: None,
            })
            .await
            .expect("seed retained metadata");
        assert!(file.created_at > files.last().unwrap().created_at);
        files.push(file);
    }

    for order in ["asc", "desc"] {
        for purpose in [None, Some("assistants"), Some("user_data")] {
            let mut expected: Vec<String> = files
                .iter()
                .filter(|file| purpose.is_none_or(|purpose| file.purpose == purpose))
                .map(|file| format!("{PREFIX_FILE}{}", file.id))
                .collect();
            if order == "desc" {
                expected.reverse();
            }
            let mut after = None;
            for (page_index, expected_page) in expected.chunks(2).enumerate() {
                let mut path = format!("/v1/files?limit=2&order={order}");
                if let Some(purpose) = purpose {
                    path.push_str(&format!("&purpose={purpose}"));
                }
                if let Some(cursor) = &after {
                    path.push_str(&format!("&after={cursor}"));
                }
                let response = fixture
                    .server
                    .get(&path)
                    .add_header("Authorization", format!("Bearer {}", fixture.owner_key))
                    .await;
                assert_eq!(response.status_code(), 200, "{path}");
                assert_no_store(&response);
                let page: api::models::FileListResponse = response.json();
                let ids: Vec<String> = page.data.iter().map(|file| file.id.clone()).collect();
                assert_eq!(
                    ids.as_slice(),
                    expected_page,
                    "pages must contain every matching file exactly once in {order} order: {path}"
                );
                assert_eq!(page.first_id.as_ref(), expected_page.first());
                assert_eq!(page.last_id.as_ref(), expected_page.last());
                assert_eq!(page.has_more, (page_index + 1) * 2 < expected.len());
                if let Some(purpose) = purpose {
                    assert!(page.data.iter().all(|file| file.purpose == purpose));
                }
                after = page.last_id;
            }
        }
    }
}

#[tokio::test]
async fn retained_file_deletion_is_workspace_scoped_and_removes_content() {
    let fixture = retained_file_fixture().await;
    let file_id = format!("{PREFIX_FILE}{}", fixture.file.id);
    let foreign = fixture
        .server
        .delete(&format!("/v1/files/{file_id}"))
        .add_header("Authorization", format!("Bearer {}", fixture.foreign_key))
        .await;
    let unknown = fixture
        .server
        .delete(&format!("/v1/files/{UNKNOWN_FILE_ID}"))
        .add_header("Authorization", format!("Bearer {}", fixture.foreign_key))
        .await;
    assert_eq!(foreign.status_code(), 404);
    assert_eq!(unknown.status_code(), 404);
    assert_no_store(&foreign);
    assert_no_store(&unknown);
    assert_eq!(foreign.text(), unknown.text());
    assert!(fixture
        .storage
        .exists(&fixture.file.storage_key)
        .await
        .unwrap());

    let still_readable = fixture
        .server
        .get(&format!("/v1/files/{file_id}/content"))
        .add_header("Authorization", format!("Bearer {}", fixture.owner_key))
        .await;
    assert_eq!(still_readable.status_code(), 200);
    assert_no_store(&still_readable);
    assert_eq!(still_readable.as_bytes().as_ref(), RETAINED_CONTENT);

    let deleted = fixture
        .server
        .delete(&format!("/v1/files/{file_id}"))
        .add_header("Authorization", format!("Bearer {}", fixture.owner_key))
        .await;
    assert_eq!(deleted.status_code(), 200);
    assert_no_store(&deleted);
    let deleted: api::models::FileDeleteResponse = deleted.json();
    assert_eq!(deleted.id, file_id);
    assert!(deleted.deleted);
    assert!(!fixture
        .storage
        .exists(&fixture.file.storage_key)
        .await
        .unwrap());

    for suffix in ["", "/content"] {
        let missing = fixture
            .server
            .get(&format!("/v1/files/{file_id}{suffix}"))
            .add_header("Authorization", format!("Bearer {}", fixture.owner_key))
            .await;
        assert_eq!(missing.status_code(), 404);
        assert_no_store(&missing);
    }
    let list = fixture
        .server
        .get("/v1/files")
        .add_header("Authorization", format!("Bearer {}", fixture.owner_key))
        .await;
    assert_eq!(list.status_code(), 200);
    assert_no_store(&list);
    assert!(list.json::<api::models::FileListResponse>().data.is_empty());
}

#[tokio::test]
async fn file_migration_routes_require_an_api_key() {
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
    assert_eq!(
        server
            .delete(&format!("/v1/files/{UNKNOWN_FILE_ID}"))
            .await
            .status_code(),
        401
    );
}

#[tokio::test]
async fn file_migration_routes_reach_workspace_scoped_handlers_and_are_no_store() {
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

    let delete = server
        .delete(&format!("/v1/files/{UNKNOWN_FILE_ID}"))
        .add_header("Authorization", format!("Bearer {api_key}"))
        .await;
    assert_eq!(
        delete.status_code(),
        404,
        "DELETE must reach its original workspace-scoped handler rather than the 410 fallback"
    );
    assert_no_store(&delete);
}

#[tokio::test]
async fn all_file_mutations_except_delete_remain_gone_after_authentication() {
    let server = setup_test_server().await;
    let (api_key, _) = create_org_and_api_key(&server).await;
    let routes = [
        (Method::POST, "/v1/files"),
        (Method::DELETE, "/v1/files"),
        (
            Method::PATCH,
            "/v1/files/file-00000000-0000-0000-0000-000000000000",
        ),
        (
            Method::GET,
            "/v1/files/file-00000000-0000-0000-0000-000000000000/",
        ),
        (
            Method::DELETE,
            "/v1/files/file-00000000-0000-0000-0000-000000000000/",
        ),
        (
            Method::DELETE,
            "/v1/files/file-00000000-0000-0000-0000-000000000000/content",
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
