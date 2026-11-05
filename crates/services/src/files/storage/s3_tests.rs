use super::*;
use crate::files::encryption::{decrypt_with_key, encrypt_with_key, ObjectKeyRing};
use aws_sdk_s3::Client as S3Client;
use aws_sdk_s3::{
    config::{BehaviorVersion, Credentials, Region},
    presigning::PresigningConfig,
};
use std::sync::Arc;
use std::time::Duration;
use wiremock::{
    matchers::{header, method, path, query_param},
    Mock, MockServer, ResponseTemplate,
};

#[path = "s3_limit_tests.rs"]
mod limits;

async fn storage(server: &MockServer) -> S3Storage {
    let config = aws_sdk_s3::Config::builder()
        .behavior_version(BehaviorVersion::latest())
        .region(Region::new("us-east-1"))
        .credentials_provider(Credentials::new(
            "test-access-key",
            "test-secret-key",
            None,
            None,
            "test",
        ))
        .endpoint_url(server.uri())
        .force_path_style(true)
        .build();
    S3Storage::new_with_ring(
        S3Client::from_conf(config),
        "bucket".into(),
        Arc::new(
            ObjectKeyRing::new(
                "current".into(),
                [1; 32],
                vec![("previous".into(), [2; 32])],
            )
            .unwrap(),
        ),
    )
}

#[tokio::test]
async fn regional_s3_requests_use_sigv4_without_sigv4a() {
    // Given a regional S3 client without network operations.
    let config = aws_sdk_s3::Config::builder()
        .behavior_version(BehaviorVersion::latest())
        .region(Region::new("us-east-1"))
        .credentials_provider(Credentials::new(
            "test-access-key",
            "test-secret-key",
            None,
            None,
            "test",
        ))
        .build();
    let client = S3Client::from_conf(config);
    // When an object request is presigned.
    let request = client
        .get_object()
        .bucket("regional-test-bucket")
        .key("test-object")
        .presigned(PresigningConfig::expires_in(Duration::from_secs(60)).unwrap())
        .await
        .unwrap();
    // Then regional SigV4 is used.
    assert!(request.uri().contains("X-Amz-Algorithm=AWS4-HMAC-SHA256"));
    assert!(request.uri().contains("us-east-1%2Fs3%2Faws4_request"));
    assert!(request.uri().contains("X-Amz-Signature="));
}

#[tokio::test]
async fn s3_upload_sends_key_metadata_and_current_ciphertext() {
    // Given a local S3 endpoint that accepts one upload.
    let server = MockServer::start().await;
    Mock::given(method("PUT"))
        .and(path("/bucket/file"))
        .and(header("x-amz-meta-near-enc-key-id", "current"))
        .and(header("content-type", "text/plain"))
        .respond_with(ResponseTemplate::new(200))
        .expect(1)
        .mount(&server)
        .await;
    let storage = storage(&server).await;
    // When a file is uploaded.
    storage
        .upload("file", b"content".to_vec(), "text/plain")
        .await
        .unwrap();
    // Then the request body authenticates under the write key.
    let requests = server.received_requests().await.unwrap();
    assert_eq!(
        decrypt_with_key(&requests.first().unwrap().body, &[1; 32]).unwrap(),
        b"content"
    );
}

#[tokio::test]
async fn s3_download_uses_the_response_metadata_hint() {
    // Given previous ciphertext with a conflicting current-key hint.
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/bucket/file"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("x-amz-meta-near-enc-key-id", "current")
                .set_body_bytes(encrypt_with_key(b"content", &[2; 32]).unwrap()),
        )
        .expect(1)
        .mount(&server)
        .await;
    // When that object is downloaded.
    let result = storage(&server).await.download("file").await;
    // Then decryption is restricted to the metadata-selected key.
    assert!(matches!(result, Err(FileServiceError::EncryptionError(_))));
}

#[tokio::test]
async fn s3_rewrap_preserves_content_type_and_user_metadata() {
    // Given legacy ciphertext with content type and additional metadata.
    let server = MockServer::start().await;
    Mock::given(method("HEAD"))
        .and(path("/bucket/file"))
        .respond_with(ResponseTemplate::new(200))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/bucket/file"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/csv")
                .insert_header("etag", "\"original\"")
                .insert_header("x-amz-meta-purpose", "batch")
                .set_body_bytes(encrypt_with_key(b"content", &[2; 32]).unwrap()),
        )
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("PUT"))
        .and(path("/bucket/file"))
        .and(header("content-type", "text/csv"))
        .and(header("x-amz-meta-near-enc-key-id", "current"))
        .and(header("x-amz-meta-purpose", "batch"))
        .and(header("if-match", "\"original\""))
        .respond_with(ResponseTemplate::new(200))
        .expect(1)
        .mount(&server)
        .await;
    // When the legacy object is rewrapped.
    let outcome = storage(&server)
        .await
        .rewrap_object("file", u64::MAX)
        .await
        .unwrap();
    // Then the replacement retains content and identifies its source key.
    assert_eq!(
        outcome,
        RewrapOutcome::Rewrapped {
            from: "previous".into()
        }
    );
    let requests = server.received_requests().await.unwrap();
    let upload = requests
        .iter()
        .find(|request| request.method == "PUT")
        .unwrap();
    assert_eq!(
        decrypt_with_key(&upload.body, &[1; 32]).unwrap(),
        b"content"
    );
}

#[tokio::test]
async fn s3_current_metadata_skips_the_body_download() {
    // Given an object whose metadata identifies the write key.
    let server = MockServer::start().await;
    Mock::given(method("HEAD"))
        .and(path("/bucket/file"))
        .respond_with(
            ResponseTemplate::new(200).insert_header("x-amz-meta-near-enc-key-id", "current"),
        )
        .expect(1)
        .mount(&server)
        .await;
    // When a rewrap is requested.
    let outcome = storage(&server)
        .await
        .rewrap_object("file", u64::MAX)
        .await
        .unwrap();
    // Then only the metadata request is necessary.
    assert_eq!(outcome, RewrapOutcome::AlreadyCurrent);
    assert_eq!(server.received_requests().await.unwrap().len(), 1);
}

#[tokio::test]
async fn s3_listing_forwards_the_continuation_and_limit() {
    // Given a bucket listing with a continuation token.
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/bucket/"))
        .and(query_param("list-type", "2"))
        .and(query_param("max-keys", "2"))
        .and(query_param("continuation-token", "previous-page"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(
            r#"<?xml version="1.0" encoding="UTF-8"?>
<ListBucketResult xmlns="http://s3.amazonaws.com/doc/2006-03-01/">
    <Name>bucket</Name>
    <KeyCount>2</KeyCount>
    <MaxKeys>2</MaxKeys>
    <IsTruncated>true</IsTruncated>
    <Contents><Key>a</Key></Contents>
    <Contents><Key>b</Key></Contents>
    <NextContinuationToken>next-page</NextContinuationToken>
</ListBucketResult>"#,
            "application/xml",
        ))
        .expect(1)
        .mount(&server)
        .await;
    // When the next page is requested.
    let page = storage(&server)
        .await
        .list_keys(Some("previous-page".into()), 2)
        .await
        .unwrap();
    // Then both object keys and the opaque continuation are returned.
    assert_eq!(
        page,
        (vec!["a".into(), "b".into()], Some("next-page".into()))
    );
}

#[tokio::test]
async fn s3_verify_requires_write_key_authentication() {
    // Given current metadata on previous-key ciphertext.
    let server = MockServer::start().await;
    Mock::given(method("HEAD"))
        .and(path("/bucket/file"))
        .respond_with(
            ResponseTemplate::new(200).insert_header("x-amz-meta-near-enc-key-id", "current"),
        )
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/bucket/file"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("x-amz-meta-near-enc-key-id", "current")
                .set_body_bytes(encrypt_with_key(b"content", &[2; 32]).unwrap()),
        )
        .expect(1)
        .mount(&server)
        .await;
    // When the object is verified.
    let result = storage(&server)
        .await
        .verify_object("file", u64::MAX)
        .await
        .unwrap();
    // Then metadata alone does not make it decryptable.
    assert_eq!(
        result,
        ObjectVerification {
            is_current: true,
            is_decryptable: false,
            unknown_key_id: false,
        }
    );
}
