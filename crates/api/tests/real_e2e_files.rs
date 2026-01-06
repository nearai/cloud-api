mod common;

use common::endpoints::upload_file;
use common::*;
use services::id_prefixes::PREFIX_FILE;

#[tokio::test]
async fn real_test_file_api_upload_and_fetch() {
    let (server, _pool, _guard) = setup_test_server_with_real_provider().await;
    let org = setup_org_with_credits(&server, 5_000_000_000i64).await;
    let api_key = get_api_key_for_org(&server, org.id).await;

    let payload = b"Real test file contents";
    let upload_response = upload_file(
        &server,
        &api_key,
        "real-test.txt",
        payload,
        "text/plain",
        "user_data",
    )
    .await;

    assert_eq!(upload_response.status_code(), 201);
    let file = upload_response.json::<api::models::FileUploadResponse>();
    assert!(file.id.starts_with(PREFIX_FILE));
    assert_eq!(file.filename, "real-test.txt");
    assert_eq!(file.bytes, payload.len() as i64);
    assert_eq!(file.purpose, "user_data");

    let meta_response = server
        .get(format!("/v1/files/{}", file.id).as_str())
        .add_header("Authorization", format!("Bearer {api_key}"))
        .await;
    assert_eq!(meta_response.status_code(), 200);
    let meta = meta_response.json::<api::models::FileUploadResponse>();
    assert_eq!(meta.id, file.id);
    assert_eq!(meta.filename, file.filename);

    let content_response = server
        .get(format!("/v1/files/{}/content", file.id).as_str())
        .add_header("Authorization", format!("Bearer {api_key}"))
        .await;
    assert_eq!(content_response.status_code(), 200);
    assert_eq!(content_response.as_bytes().as_ref(), &payload[..]);
}
