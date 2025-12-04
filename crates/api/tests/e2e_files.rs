// Import common test utilities
mod common;

use common::*;
use services::id_prefixes::PREFIX_FILE;

/// Helper function to upload a file
async fn upload_file(
    server: &axum_test::TestServer,
    api_key: &str,
    filename: &str,
    content: &[u8],
    content_type: &str,
    purpose: &str,
) -> axum_test::TestResponse {
    server
        .post("/v1/files")
        .add_header("Authorization", format!("Bearer {api_key}"))
        .multipart(
            axum_test::multipart::MultipartForm::new()
                .add_text("purpose", purpose)
                .add_part(
                    "file",
                    axum_test::multipart::Part::bytes(content.to_vec())
                        .file_name(filename)
                        .mime_type(content_type),
                ),
        )
        .await
}

/// Helper function to upload a file with expiration
async fn upload_file_with_expiration(
    server: &axum_test::TestServer,
    api_key: &str,
    filename: &str,
    content: &[u8],
    content_type: &str,
    purpose: &str,
    expires_after_seconds: i64,
) -> axum_test::TestResponse {
    server
        .post("/v1/files")
        .add_header("Authorization", format!("Bearer {api_key}"))
        .multipart(
            axum_test::multipart::MultipartForm::new()
                .add_text("purpose", purpose)
                .add_text("expires_after[anchor]", "created_at")
                .add_text("expires_after[seconds]", expires_after_seconds.to_string())
                .add_part(
                    "file",
                    axum_test::multipart::Part::bytes(content.to_vec())
                        .file_name(filename)
                        .mime_type(content_type),
                ),
        )
        .await
}

#[tokio::test]
async fn test_upload_file_success() {
    let server = setup_test_server().await;
    let (api_key, _) = create_org_and_api_key(&server).await;

    let content = b"Hello, this is a test file!";
    let response = upload_file(
        &server,
        &api_key,
        "test.txt",
        content,
        "text/plain",
        "user_data",
    )
    .await;

    assert_eq!(response.status_code(), 201);
    let file: api::models::FileUploadResponse = response.json();
    assert!(file.id.starts_with(PREFIX_FILE));
    assert_eq!(file.object, "file");
    assert_eq!(file.bytes, content.len() as i64);
    assert_eq!(file.filename, "test.txt");
    assert_eq!(file.purpose, "user_data");
    assert!(file.created_at > 0);
    assert!(file.expires_at.is_none());
}

#[tokio::test]
async fn test_upload_file_with_expiration() {
    let server = setup_test_server().await;
    let (api_key, _) = create_org_and_api_key(&server).await;

    let content = b"Temporary file";
    let expires_after_seconds = 86400; // 1 day
    let response = upload_file_with_expiration(
        &server,
        &api_key,
        "temp.txt",
        content,
        "text/plain",
        "user_data",
        expires_after_seconds,
    )
    .await;

    assert_eq!(response.status_code(), 201);
    let file: api::models::FileUploadResponse = response.json();
    assert!(file.expires_at.is_some());
    assert!(file.expires_at.unwrap() > file.created_at);
}

#[tokio::test]
async fn test_upload_json_file() {
    let server = setup_test_server().await;
    let (api_key, _) = create_org_and_api_key(&server).await;

    let content = br#"{"key": "value", "number": 42}"#;
    let response = upload_file(
        &server,
        &api_key,
        "data.json",
        content,
        "application/json",
        "user_data",
    )
    .await;

    assert_eq!(response.status_code(), 201);
    let file: api::models::FileUploadResponse = response.json();
    assert_eq!(file.filename, "data.json");
    assert_eq!(file.bytes, content.len() as i64);
}

#[tokio::test]
async fn test_upload_binary_file() {
    let server = setup_test_server().await;
    let (api_key, _) = create_org_and_api_key(&server).await;

    let content = vec![0u8, 1, 2, 3, 4, 5, 255, 254, 253];
    let response = upload_file(
        &server,
        &api_key,
        "binary.bin",
        &content,
        "application/octet-stream",
        "user_data",
    )
    .await;

    assert_eq!(response.status_code(), 201);
    let file: api::models::FileUploadResponse = response.json();
    assert_eq!(file.bytes, content.len() as i64);
}

#[tokio::test]
async fn test_upload_file_invalid_purpose() {
    let server = setup_test_server().await;
    let (api_key, _) = create_org_and_api_key(&server).await;

    let content = b"Test content";
    let response = upload_file(
        &server,
        &api_key,
        "test.txt",
        content,
        "text/plain",
        "invalid_purpose",
    )
    .await;

    assert_eq!(response.status_code(), 400);
    let error: api::models::ErrorResponse = response.json();
    assert!(error.error.message.contains("Invalid file purpose"));
}

#[tokio::test]
async fn test_upload_file_invalid_mime_type() {
    let server = setup_test_server().await;
    let (api_key, _) = create_org_and_api_key(&server).await;

    let content = b"Test content";
    let response = upload_file(
        &server,
        &api_key,
        "test.exe",
        content,
        "application/x-msdownload",
        "user_data",
    )
    .await;

    assert_eq!(response.status_code(), 400);
    let error: api::models::ErrorResponse = response.json();
    assert!(error.error.message.contains("Invalid file type"));
}

#[tokio::test]
async fn test_upload_file_missing_purpose() {
    let server = setup_test_server().await;
    let (api_key, _) = create_org_and_api_key(&server).await;

    let response = server
        .post("/v1/files")
        .add_header("Authorization", format!("Bearer {api_key}"))
        .multipart(
            axum_test::multipart::MultipartForm::new().add_part(
                "file",
                axum_test::multipart::Part::bytes(b"Test content".to_vec())
                    .file_name("test.txt")
                    .mime_type("text/plain"),
            ),
        )
        .await;

    assert_eq!(response.status_code(), 400);
    let error: api::models::ErrorResponse = response.json();
    assert!(error
        .error
        .message
        .contains("Missing required field: purpose"));
}

#[tokio::test]
async fn test_upload_file_unauthorized() {
    let server = setup_test_server().await;

    let content = b"Test content";
    let response = upload_file(
        &server,
        "invalid-api-key",
        "test.txt",
        content,
        "text/plain",
        "user_data",
    )
    .await;

    assert_eq!(response.status_code(), 401);
}

#[tokio::test]
async fn test_list_files() {
    let server = setup_test_server().await;
    let (api_key, _) = create_org_and_api_key(&server).await;

    // Upload multiple files
    for i in 1..=5 {
        let content = format!("File content {i}");
        upload_file(
            &server,
            &api_key,
            &format!("file{i}.txt"),
            content.as_bytes(),
            "text/plain",
            "user_data",
        )
        .await;
    }

    // List files
    let response = server
        .get("/v1/files")
        .add_header("Authorization", format!("Bearer {api_key}"))
        .await;

    assert_eq!(response.status_code(), 200);
    let list: api::models::FileListResponse = response.json();
    assert_eq!(list.object, "list");
    assert!(list.data.len() >= 5);
    assert!(list.first_id.is_some());
    assert!(list.last_id.is_some());
}

#[tokio::test]
async fn test_list_files_with_limit() {
    let server = setup_test_server().await;
    let (api_key, _) = create_org_and_api_key(&server).await;

    // Upload multiple files
    for i in 1..=5 {
        let content = format!("File content {i}");
        upload_file(
            &server,
            &api_key,
            &format!("file{i}.txt"),
            content.as_bytes(),
            "text/plain",
            "user_data",
        )
        .await;
    }

    // List with limit
    let response = server
        .get("/v1/files?limit=2")
        .add_header("Authorization", format!("Bearer {api_key}"))
        .await;

    assert_eq!(response.status_code(), 200);
    let list: api::models::FileListResponse = response.json();
    assert_eq!(list.data.len(), 2);
    assert!(list.has_more);
}

#[tokio::test]
async fn test_list_files_with_pagination() {
    let server = setup_test_server().await;
    let (api_key, _) = create_org_and_api_key(&server).await;

    // Upload multiple files
    for i in 1..=5 {
        let content = format!("File content {i}");
        upload_file(
            &server,
            &api_key,
            &format!("file{i}.txt"),
            content.as_bytes(),
            "text/plain",
            "user_data",
        )
        .await;
    }

    // Get first page
    let response = server
        .get("/v1/files?limit=2")
        .add_header("Authorization", format!("Bearer {api_key}"))
        .await;

    assert_eq!(response.status_code(), 200);
    let first_page: api::models::FileListResponse = response.json();
    assert_eq!(first_page.data.len(), 2);

    // Get second page using cursor
    let after_id = first_page.last_id.unwrap();
    let response = server
        .get(&format!("/v1/files?limit=2&after={after_id}"))
        .add_header("Authorization", format!("Bearer {api_key}"))
        .await;

    assert_eq!(response.status_code(), 200);
    let second_page: api::models::FileListResponse = response.json();
    assert!(!second_page.data.is_empty());
    // Ensure we got different files
    assert_ne!(first_page.data[0].id, second_page.data[0].id);
}

#[tokio::test]
async fn test_list_files_with_purpose_filter() {
    let server = setup_test_server().await;
    let (api_key, _) = create_org_and_api_key(&server).await;

    // Upload files with different purposes
    upload_file(
        &server,
        &api_key,
        "assistants_file.txt",
        b"Assistants content",
        "text/plain",
        "assistants",
    )
    .await;

    upload_file(
        &server,
        &api_key,
        "user_file.txt",
        b"User content",
        "text/plain",
        "user_data",
    )
    .await;

    // List only assistants files
    let response = server
        .get("/v1/files?purpose=assistants")
        .add_header("Authorization", format!("Bearer {api_key}"))
        .await;

    assert_eq!(response.status_code(), 200);
    let list: api::models::FileListResponse = response.json();
    for file in &list.data {
        assert_eq!(file.purpose, "assistants");
    }
}

#[tokio::test]
async fn test_list_files_with_order() {
    let server = setup_test_server().await;
    let (api_key, _) = create_org_and_api_key(&server).await;

    // Upload multiple files
    for i in 1..=3 {
        let content = format!("File content {i}");
        upload_file(
            &server,
            &api_key,
            &format!("file{i}.txt"),
            content.as_bytes(),
            "text/plain",
            "user_data",
        )
        .await;
        tokio::time::sleep(tokio::time::Duration::from_millis(100)).await; // Ensure different timestamps
    }

    // List in ascending order
    let response = server
        .get("/v1/files?order=asc")
        .add_header("Authorization", format!("Bearer {api_key}"))
        .await;

    assert_eq!(response.status_code(), 200);
    let asc_list: api::models::FileListResponse = response.json();

    // List in descending order
    let response = server
        .get("/v1/files?order=desc")
        .add_header("Authorization", format!("Bearer {api_key}"))
        .await;

    assert_eq!(response.status_code(), 200);
    let desc_list: api::models::FileListResponse = response.json();

    // Verify order is different
    if asc_list.data.len() >= 2 && desc_list.data.len() >= 2 {
        assert!(asc_list.data[0].created_at <= asc_list.data[1].created_at);
        assert!(desc_list.data[0].created_at >= desc_list.data[1].created_at);
    }
}

#[tokio::test]
async fn test_get_file_metadata() {
    let server = setup_test_server().await;
    let (api_key, _) = create_org_and_api_key(&server).await;

    // Upload a file
    let content = b"Test file content";
    let upload_response = upload_file(
        &server,
        &api_key,
        "metadata_test.txt",
        content,
        "text/plain",
        "user_data",
    )
    .await;

    assert_eq!(upload_response.status_code(), 201);
    let uploaded_file: api::models::FileUploadResponse = upload_response.json();

    // Get file metadata
    let response = server
        .get(&format!("/v1/files/{}", uploaded_file.id))
        .add_header("Authorization", format!("Bearer {api_key}"))
        .await;

    assert_eq!(response.status_code(), 200);
    let file: api::models::FileUploadResponse = response.json();
    assert_eq!(file.id, uploaded_file.id);
    assert_eq!(file.filename, "metadata_test.txt");
    assert_eq!(file.bytes, content.len() as i64);
    assert_eq!(file.purpose, "user_data");
}

#[tokio::test]
async fn test_get_file_metadata_not_found() {
    let server = setup_test_server().await;
    let (api_key, _) = create_org_and_api_key(&server).await;

    let response = server
        .get("/v1/files/file-00000000-0000-0000-0000-000000000000")
        .add_header("Authorization", format!("Bearer {api_key}"))
        .await;

    assert_eq!(response.status_code(), 404);
}

#[tokio::test]
async fn test_get_file_content() {
    let server = setup_test_server().await;
    let (api_key, _) = create_org_and_api_key(&server).await;

    // Upload a file
    let content = b"This is the file content that should be returned";
    let upload_response = upload_file(
        &server,
        &api_key,
        "content_test.txt",
        content,
        "text/plain",
        "user_data",
    )
    .await;

    assert_eq!(upload_response.status_code(), 201);
    let uploaded_file: api::models::FileUploadResponse = upload_response.json();

    // Get file content
    let response = server
        .get(&format!("/v1/files/{}/content", uploaded_file.id))
        .add_header("Authorization", format!("Bearer {api_key}"))
        .await;

    assert_eq!(response.status_code(), 200);

    // Verify headers
    let content_type = response
        .headers()
        .get("content-type")
        .unwrap()
        .to_str()
        .unwrap();
    assert_eq!(content_type, "text/plain");

    let content_length = response
        .headers()
        .get("content-length")
        .unwrap()
        .to_str()
        .unwrap();
    assert_eq!(content_length, content.len().to_string());

    let content_disposition = response
        .headers()
        .get("content-disposition")
        .unwrap()
        .to_str()
        .unwrap();
    assert!(content_disposition.contains("content_test.txt"));

    // Verify content
    let body = response.as_bytes();
    assert_eq!(body.as_ref(), &content[..]);
}

#[tokio::test]
async fn test_get_binary_file_content() {
    let server = setup_test_server().await;
    let (api_key, _) = create_org_and_api_key(&server).await;

    // Upload a binary file
    let content = vec![0u8, 1, 2, 3, 4, 5, 255, 254, 253];
    let upload_response = upload_file(
        &server,
        &api_key,
        "binary.bin",
        &content,
        "application/octet-stream",
        "user_data",
    )
    .await;

    assert_eq!(upload_response.status_code(), 201);
    let uploaded_file: api::models::FileUploadResponse = upload_response.json();

    // Get file content
    let response = server
        .get(&format!("/v1/files/{}/content", uploaded_file.id))
        .add_header("Authorization", format!("Bearer {api_key}"))
        .await;

    assert_eq!(response.status_code(), 200);

    // Verify binary content matches exactly
    let body = response.as_bytes();
    assert_eq!(body.to_vec(), content);
}

#[tokio::test]
async fn test_get_file_content_not_found() {
    let server = setup_test_server().await;
    let (api_key, _) = create_org_and_api_key(&server).await;

    let response = server
        .get("/v1/files/file-00000000-0000-0000-0000-000000000000/content")
        .add_header("Authorization", format!("Bearer {api_key}"))
        .await;

    assert_eq!(response.status_code(), 404);
}

#[tokio::test]
async fn test_delete_file() {
    let server = setup_test_server().await;
    let (api_key, _) = create_org_and_api_key(&server).await;

    // Upload a file
    let content = b"File to be deleted";
    let upload_response = upload_file(
        &server,
        &api_key,
        "delete_test.txt",
        content,
        "text/plain",
        "user_data",
    )
    .await;

    assert_eq!(upload_response.status_code(), 201);
    let uploaded_file: api::models::FileUploadResponse = upload_response.json();

    // Delete the file
    let response = server
        .delete(&format!("/v1/files/{}", uploaded_file.id))
        .add_header("Authorization", format!("Bearer {api_key}"))
        .await;

    assert_eq!(response.status_code(), 200);
    let delete_response: api::models::FileDeleteResponse = response.json();
    assert_eq!(delete_response.id, uploaded_file.id);
    assert_eq!(delete_response.object, "file");
    assert!(delete_response.deleted);

    // Verify file is deleted
    let response = server
        .get(&format!("/v1/files/{}", uploaded_file.id))
        .add_header("Authorization", format!("Bearer {api_key}"))
        .await;

    assert_eq!(response.status_code(), 404);
}

#[tokio::test]
async fn test_delete_file_not_found() {
    let server = setup_test_server().await;
    let (api_key, _) = create_org_and_api_key(&server).await;

    let response = server
        .delete("/v1/files/file-00000000-0000-0000-0000-000000000000")
        .add_header("Authorization", format!("Bearer {api_key}"))
        .await;

    assert_eq!(response.status_code(), 404);
}

#[tokio::test]
async fn test_file_isolation_between_workspaces() {
    let server = setup_test_server().await;

    // Create two organizations with API keys
    let org1 = create_org(&server).await;
    let api_key1 = get_api_key_for_org(&server, org1.id.clone()).await;

    let org2 = create_org(&server).await;
    let api_key2 = get_api_key_for_org(&server, org2.id.clone()).await;

    // Upload file with first workspace
    let content = b"Workspace 1 file";
    let upload_response = upload_file(
        &server,
        &api_key1,
        "workspace1.txt",
        content,
        "text/plain",
        "user_data",
    )
    .await;

    assert_eq!(upload_response.status_code(), 201);
    let file1: api::models::FileUploadResponse = upload_response.json();

    // Try to access with second workspace - should fail
    let response = server
        .get(&format!("/v1/files/{}", file1.id))
        .add_header("Authorization", format!("Bearer {api_key2}"))
        .await;

    assert_eq!(response.status_code(), 404);

    // Try to delete with second workspace - should fail
    let response = server
        .delete(&format!("/v1/files/{}", file1.id))
        .add_header("Authorization", format!("Bearer {api_key2}"))
        .await;

    assert_eq!(response.status_code(), 404);

    // Verify first workspace can still access
    let response = server
        .get(&format!("/v1/files/{}", file1.id))
        .add_header("Authorization", format!("Bearer {api_key1}"))
        .await;

    assert_eq!(response.status_code(), 200);
}

#[tokio::test]
async fn test_upload_and_download_large_file() {
    let server = setup_test_server().await;
    let (api_key, _) = create_org_and_api_key(&server).await;

    // Create a 1MB file
    let content = vec![42u8; 1024 * 1024];
    let upload_response = upload_file(
        &server,
        &api_key,
        "large_file.bin",
        &content,
        "application/octet-stream",
        "user_data",
    )
    .await;

    assert_eq!(upload_response.status_code(), 201);
    let uploaded_file: api::models::FileUploadResponse = upload_response.json();
    assert_eq!(uploaded_file.bytes, content.len() as i64);

    // Download and verify
    let response = server
        .get(&format!("/v1/files/{}/content", uploaded_file.id))
        .add_header("Authorization", format!("Bearer {api_key}"))
        .await;

    assert_eq!(response.status_code(), 200);
    let body = response.as_bytes();
    assert_eq!(body.len(), content.len());
    assert_eq!(body.to_vec(), content);
}

#[tokio::test]
async fn test_file_id_formats() {
    let server = setup_test_server().await;
    let (api_key, _) = create_org_and_api_key(&server).await;

    // Upload a file
    let content = b"Test content";
    let upload_response = upload_file(
        &server,
        &api_key,
        "test.txt",
        content,
        "text/plain",
        "user_data",
    )
    .await;

    assert_eq!(upload_response.status_code(), 201);
    let uploaded_file: api::models::FileUploadResponse = upload_response.json();

    // Test with file prefix
    let response = server
        .get(&format!("/v1/files/{}", uploaded_file.id))
        .add_header("Authorization", format!("Bearer {api_key}"))
        .await;
    assert_eq!(response.status_code(), 200);

    // Test without file prefix (strip prefix from ID)
    let id_without_prefix = uploaded_file.id.strip_prefix(PREFIX_FILE).unwrap();
    let response = server
        .get(&format!("/v1/files/{id_without_prefix}"))
        .add_header("Authorization", format!("Bearer {api_key}"))
        .await;
    assert_eq!(response.status_code(), 200);
}

#[tokio::test]
async fn test_complete_file_lifecycle() {
    let server = setup_test_server().await;
    let (api_key, _) = create_org_and_api_key(&server).await;

    // 1. Upload file
    let content = b"Complete lifecycle test";
    let upload_response = upload_file(
        &server,
        &api_key,
        "lifecycle.txt",
        content,
        "text/plain",
        "user_data",
    )
    .await;
    assert_eq!(upload_response.status_code(), 201);
    let file: api::models::FileUploadResponse = upload_response.json();

    // 2. List files (should include our file)
    let list_response = server
        .get("/v1/files")
        .add_header("Authorization", format!("Bearer {api_key}"))
        .await;
    assert_eq!(list_response.status_code(), 200);
    let list: api::models::FileListResponse = list_response.json();
    assert!(list.data.iter().any(|f| f.id == file.id));

    // 3. Get metadata
    let get_response = server
        .get(&format!("/v1/files/{}", file.id))
        .add_header("Authorization", format!("Bearer {api_key}"))
        .await;
    assert_eq!(get_response.status_code(), 200);

    // 4. Download content
    let content_response = server
        .get(&format!("/v1/files/{}/content", file.id))
        .add_header("Authorization", format!("Bearer {api_key}"))
        .await;
    assert_eq!(content_response.status_code(), 200);
    assert_eq!(content_response.as_bytes().as_ref(), &content[..]);

    // 5. Delete file
    let delete_response = server
        .delete(&format!("/v1/files/{}", file.id))
        .add_header("Authorization", format!("Bearer {api_key}"))
        .await;
    assert_eq!(delete_response.status_code(), 200);

    // 6. Verify deletion
    let get_response = server
        .get(&format!("/v1/files/{}", file.id))
        .add_header("Authorization", format!("Bearer {api_key}"))
        .await;
    assert_eq!(get_response.status_code(), 404);
}

#[tokio::test]
async fn test_file_in_response_api() {
    let (server, _pool, mock, _database) = setup_test_server_with_pool().await;
    setup_qwen_model(&server).await;
    let (api_key, _) = create_org_and_api_key(&server).await;

    // Configure mock provider with exact prompt matchers
    // Timestamps will be normalized automatically to [TIME] for matching
    use common::mock_prompts;

    // First request - with file content
    let first_prompt = mock_prompts::build_prompt(
        "Tell me more about yourself.\n\nFile: test_doc.txt\nContent:\nMichael Jordan is widely regarded as one of the greatest basketball players of all time. He won six NBA championships and was known for his scoring and competitiveness."
    );
    mock.when(inference_providers::mock::RequestMatcher::ExactPrompt(first_prompt))
    .respond_with(inference_providers::mock::ResponseTemplate::new(
        "Michael Jordan is indeed a legendary basketball player! He's widely regarded as one of the greatest players of all time, having won six NBA championships with the Chicago Bulls."
    ))
    .await;

    // Second request - conversation history includes first message with file + new question
    let second_prompt = mock_prompts::build_prompt(
        "Tell me more about yourself.\nFile: test_doc.txt\nContent:\nMichael Jordan is widely regarded as one of the greatest basketball players of all time. He won six NBA championships and was known for his scoring and competitiveness. Michael Jordan is indeed a legendary basketball player! He's widely regarded as one of the greatest players of all time, having won six NBA championships with the Chicago Bulls. What does the file say?"
    );
    let expected_response = "The file contains information about Michael Jordan, discussing his greatness as a basketball player and his six NBA championships.";

    mock.when(inference_providers::mock::RequestMatcher::ExactPrompt(
        second_prompt,
    ))
    .respond_with(inference_providers::mock::ResponseTemplate::new(
        expected_response,
    ))
    .await;

    // 1. Upload a text file
    let file_content = b"Michael Jordan is widely regarded as one of the greatest basketball players of all time. He won six NBA championships and was known for his scoring and competitiveness.";
    let upload_response = upload_file(
        &server,
        &api_key,
        "test_doc.txt",
        file_content,
        "text/plain",
        "user_data",
    )
    .await;

    assert_eq!(upload_response.status_code(), 201);
    let file: api::models::FileUploadResponse = upload_response.json();
    println!("Uploaded file: {}", file.id);

    // Get file to check if it exists
    let file_response = server
        .get(&format!("/v1/files/{}", file.id))
        .add_header("Authorization", format!("Bearer {api_key}"))
        .await;
    assert_eq!(file_response.status_code(), 200);
    let file_obj: api::models::FileUploadResponse = file_response.json();
    println!("File: {file_obj:?}");

    // 2. Get available models
    let models_response = server
        .get("/v1/models")
        .add_header("Authorization", format!("Bearer {api_key}"))
        .await;
    let models = models_response.json::<api::models::ModelsResponse>();
    assert!(!models.data.is_empty());
    let model_id = models.data[0].id.clone();

    // 3. Create a conversation
    let conversation_response = server
        .post("/v1/conversations")
        .add_header("Authorization", format!("Bearer {api_key}"))
        .json(&serde_json::json!({}))
        .await;
    assert_eq!(conversation_response.status_code(), 201);
    let conversation: api::models::ConversationObject = conversation_response.json();
    println!("Created conversation: {}", conversation.id);

    // 4. Create a response with file input (non-streaming)
    let response = server
        .post("/v1/responses")
        .add_header("Authorization", format!("Bearer {api_key}"))
        .json(&serde_json::json!({
            "model": model_id,
            "conversation": conversation.id,
            "input": [{
                "role": "user",
                "content": [{
                    "type": "input_text",
                    "text": "Tell me more about yourself."
                }, {
                    "type": "input_file",
                    "file_id": file.id
                }]
            }],
            "max_output_tokens": 100,
            "stream": false
        }))
        .await;

    assert_eq!(response.status_code(), 200);
    let response_obj: api::models::ResponseObject = response.json();

    // 5. Verify the response completed successfully
    assert_eq!(response_obj.status, api::models::ResponseStatus::Completed);

    // 6. Verify the response has output
    assert!(!response_obj.output.is_empty());

    // 7. Check that input items were stored (should include file reference)
    let input_items_response = server
        .get(&format!("/v1/responses/{}/input_items", response_obj.id))
        .add_header("Authorization", format!("Bearer {api_key}"))
        .await;

    if input_items_response.status_code() != 200 {
        println!(
            "Error response: status={}, body={}",
            input_items_response.status_code(),
            input_items_response.text()
        );
    }
    assert_eq!(input_items_response.status_code(), 200);
    let input_items: api::models::ResponseInputItemList = input_items_response.json();
    println!("Input items: {input_items:?}");

    // Should have at least one input item
    assert!(!input_items.data.is_empty());

    // 8. Test streaming response with file
    let stream_response = server
        .post("/v1/responses")
        .add_header("Authorization", format!("Bearer {api_key}"))
        .json(&serde_json::json!({
            "model": model_id,
            "conversation": conversation.id,
            "input": [{
                "role": "user",
                "content": [{
                    "type": "input_text",
                    "text": "What does the file say?"
                }]
            }],
            "max_output_tokens": 50,
            "stream": true
        }))
        .await;

    assert_eq!(stream_response.status_code(), 200);
    let stream_text = stream_response.text();
    println!(
        "Stream response (first 500 chars): {}",
        &stream_text[..stream_text.len().min(500)]
    );

    // Verify we got SSE events
    assert!(stream_text.contains("event:"));
    assert!(stream_text.contains("data:"));

    // Parse the streaming response to check for completion
    let mut final_response: Option<api::models::ResponseObject> = None;
    for line_chunk in stream_text.split("\n\n") {
        if line_chunk.trim().is_empty() {
            continue;
        }

        let mut event_type = "";
        let mut event_data = "";

        for line in line_chunk.lines() {
            if let Some(event_name) = line.strip_prefix("event: ") {
                event_type = event_name;
            } else if let Some(data) = line.strip_prefix("data: ") {
                event_data = data;
            }
        }

        if event_type == "response.completed" && !event_data.is_empty() {
            if let Ok(event_json) = serde_json::from_str::<serde_json::Value>(event_data) {
                if let Some(response_obj) = event_json.get("response") {
                    final_response =
                        serde_json::from_value::<api::models::ResponseObject>(response_obj.clone())
                            .ok();
                }
            }
            break;
        }
    }

    assert!(
        final_response.is_some(),
        "Expected final response in stream"
    );
    let final_resp = final_response.unwrap();
    // Extract text from the response output
    let mut final_text = String::new();
    for item in &final_resp.output {
        if let api::models::ResponseOutputItem::Message { content, .. } = item {
            for part in content {
                if let api::models::ResponseOutputContent::OutputText { text, .. } = part {
                    final_text.push_str(text);
                }
            }
        }
    }
    let final_text = final_text.trim();
    // Verify we got the expected response from the mock
    assert_eq!(
        expected_response, final_text,
        "final response does not match expected mock response"
    );
    assert_eq!(final_resp.status, api::models::ResponseStatus::Completed);
}

#[tokio::test]
async fn test_file_not_found_in_response_api() {
    let server = setup_test_server().await;
    setup_qwen_model(&server).await;
    let (api_key, _) = create_org_and_api_key(&server).await;

    // Get available models
    let models_response = server
        .get("/v1/models")
        .add_header("Authorization", format!("Bearer {api_key}"))
        .await;
    let models = models_response.json::<api::models::ModelsResponse>();
    assert!(!models.data.is_empty());
    let model_id = models.data[0].id.clone();

    // Create a conversation
    let conversation_response = server
        .post("/v1/conversations")
        .add_header("Authorization", format!("Bearer {api_key}"))
        .json(&serde_json::json!({}))
        .await;
    assert_eq!(conversation_response.status_code(), 201);
    let conversation: api::models::ConversationObject = conversation_response.json();

    // Try to create a response with a non-existent file
    let fake_file_id = "file-00000000-0000-0000-0000-000000000000";
    let response = server
        .post("/v1/responses")
        .add_header("Authorization", format!("Bearer {api_key}"))
        .json(&serde_json::json!({
            "model": model_id,
            "conversation": conversation.id,
            "input": [{
                "role": "user",
                "content": [{
                    "type": "input_text",
                    "text": "What's in the file?"
                }, {
                    "type": "input_file",
                    "file_id": fake_file_id
                }]
            }],
            "max_output_tokens": 50,
            "stream": true
        }))
        .await;

    // The response should start streaming, but will fail when trying to fetch the file
    assert_eq!(response.status_code(), 200);
    let stream_text = response.text();

    // Check for error event in the stream
    assert!(
        stream_text.contains("response.failed") || stream_text.contains("error"),
        "Expected error in stream response"
    );
}

#[tokio::test]
async fn test_multiple_files_in_response_api() {
    let server = setup_test_server().await;
    setup_qwen_model(&server).await;
    let (api_key, _) = create_org_and_api_key(&server).await;

    // 1. Upload multiple text files
    let file1_content = b"File 1: Product specifications\nPrice: $100\nColor: Red";
    let upload1 = upload_file(
        &server,
        &api_key,
        "product1.txt",
        file1_content,
        "text/plain",
        "user_data",
    )
    .await;
    assert_eq!(upload1.status_code(), 201);
    let file1: api::models::FileUploadResponse = upload1.json();

    let file2_content = b"File 2: Product specifications\nPrice: $200\nColor: Blue";
    let upload2 = upload_file(
        &server,
        &api_key,
        "product2.txt",
        file2_content,
        "text/plain",
        "user_data",
    )
    .await;
    assert_eq!(upload2.status_code(), 201);
    let file2: api::models::FileUploadResponse = upload2.json();

    // 2. Get available models
    let models_response = server
        .get("/v1/models")
        .add_header("Authorization", format!("Bearer {api_key}"))
        .await;
    let models = models_response.json::<api::models::ModelsResponse>();
    assert!(!models.data.is_empty());
    let model_id = models.data[0].id.clone();

    // 3. Create a conversation
    let conversation_response = server
        .post("/v1/conversations")
        .add_header("Authorization", format!("Bearer {api_key}"))
        .json(&serde_json::json!({}))
        .await;
    assert_eq!(conversation_response.status_code(), 201);
    let conversation: api::models::ConversationObject = conversation_response.json();

    // 4. Create a response with multiple files
    let response = server
        .post("/v1/responses")
        .add_header("Authorization", format!("Bearer {api_key}"))
        .json(&serde_json::json!({
            "model": model_id,
            "conversation": conversation.id,
            "input": [{
                "role": "user",
                "content": [{
                    "type": "input_text",
                    "text": "Compare these two products:"
                }, {
                    "type": "input_file",
                    "file_id": file1.id
                }, {
                    "type": "input_file",
                    "file_id": file2.id
                }]
            }],
            "max_output_tokens": 100,
            "stream": false
        }))
        .await;

    assert_eq!(response.status_code(), 200);
    let response_obj: api::models::ResponseObject = response.json();

    // Verify the response completed successfully
    assert_eq!(response_obj.status, api::models::ResponseStatus::Completed);

    // Verify the response has output
    assert!(!response_obj.output.is_empty());

    println!("Successfully processed multiple files in response");
}
