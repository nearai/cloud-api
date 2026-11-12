use super::*;

#[tokio::test]
async fn oversized_current_head_skips_get_during_rewrap() {
    // Given current metadata whose content length exceeds the limit.
    let server = MockServer::start().await;
    Mock::given(method("HEAD"))
        .and(path("/bucket/file"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-length", "36")
                .insert_header("x-amz-meta-near-enc-key-id", "current"),
        )
        .expect(1)
        .mount(&server)
        .await;
    // When rewrap examines the object.
    let result = storage(&server).await.rewrap_object("file", 35).await;
    // Then it reports a current oversized object without a body request.
    assert!(matches!(
        result,
        Err(FileServiceError::ObjectTooLarge {
            key_state: ObjectKeyState::Current
        })
    ));
    assert_eq!(server.received_requests().await.unwrap().len(), 1);
}

#[tokio::test]
async fn oversized_old_head_skips_get_during_verify() {
    // Given stale metadata whose content length exceeds the limit.
    let server = MockServer::start().await;
    Mock::given(method("HEAD"))
        .and(path("/bucket/file"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-length", "36")
                .insert_header("x-amz-meta-near-enc-key-id", "previous"),
        )
        .expect(1)
        .mount(&server)
        .await;
    // When verify examines the object.
    let result = storage(&server).await.verify_object("file", 35).await;
    // Then it reports a stale oversized object without a body request.
    assert!(matches!(
        result,
        Err(FileServiceError::ObjectTooLarge {
            key_state: ObjectKeyState::StaleOrAbsent
        })
    ));
    assert_eq!(server.received_requests().await.unwrap().len(), 1);
}

#[tokio::test]
async fn execute_retains_unknown_id_when_get_grows_after_known_small_head() {
    // Given a known small HEAD and an unknown oversized replacement in GET.
    let server = MockServer::start().await;
    Mock::given(method("HEAD"))
        .and(path("/bucket/file"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-length", "35")
                .insert_header("x-amz-meta-near-enc-key-id", "previous"),
        )
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/bucket/file"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("x-amz-meta-near-enc-key-id", "unknown")
                .set_body_bytes(encrypt_with_key(b"content+", &[2; 32]).unwrap()),
        )
        .expect(1)
        .mount(&server)
        .await;
    // When execution rejects the authoritative GET by its size.
    let result = storage(&server).await.rewrap_object("file", 35).await;
    // Then the unknown metadata flag is not lost with the oversized error.
    assert!(matches!(
        result,
        Err(FileServiceError::ObjectTooLarge {
            key_state: ObjectKeyState::UnknownKeyId
        })
    ));
    assert_eq!(server.received_requests().await.unwrap().len(), 2);
}

#[tokio::test]
async fn unknown_key_id_skips_get_during_verify() {
    // Given HEAD metadata naming a key outside the ring.
    let server = MockServer::start().await;
    Mock::given(method("HEAD"))
        .and(path("/bucket/file"))
        .respond_with(
            ResponseTemplate::new(200).insert_header("x-amz-meta-near-enc-key-id", "unknown"),
        )
        .expect(1)
        .mount(&server)
        .await;
    // When verify examines the object.
    let result = storage(&server)
        .await
        .verify_object("file", 35)
        .await
        .unwrap();
    // Then no trial decryption or body request occurs.
    assert_eq!(
        result,
        ObjectVerification {
            is_current: false,
            decrypts_under_write_key: false,
            decrypts_under_previous_key: false,
            unknown_key_id: true,
        }
    );
    assert_eq!(server.received_requests().await.unwrap().len(), 1);
}

#[tokio::test]
async fn verify_retains_unknown_key_id_discovered_in_get_after_known_head() {
    // Given known HEAD metadata and an object replaced with an unknown id before GET.
    let server = MockServer::start().await;
    Mock::given(method("HEAD"))
        .and(path("/bucket/file"))
        .respond_with(
            ResponseTemplate::new(200).insert_header("x-amz-meta-near-enc-key-id", "previous"),
        )
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/bucket/file"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("x-amz-meta-near-enc-key-id", "unknown")
                .set_body_bytes(encrypt_with_key(b"content", &[1; 32]).unwrap()),
        )
        .expect(1)
        .mount(&server)
        .await;
    // When verification reads the replacement object.
    let result = storage(&server)
        .await
        .verify_object("file", 35)
        .await
        .unwrap();
    // Then even write-key ciphertext is classified by its authoritative unknown id.
    assert_eq!(
        result,
        ObjectVerification {
            is_current: false,
            decrypts_under_write_key: false,
            decrypts_under_previous_key: false,
            unknown_key_id: true
        }
    );
}

#[tokio::test]
async fn execute_retains_unknown_key_id_discovered_in_get_after_known_head() {
    // Given known HEAD metadata and an object replaced with an unknown id before GET.
    let server = MockServer::start().await;
    Mock::given(method("HEAD"))
        .and(path("/bucket/file"))
        .respond_with(
            ResponseTemplate::new(200).insert_header("x-amz-meta-near-enc-key-id", "previous"),
        )
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/bucket/file"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("x-amz-meta-near-enc-key-id", "unknown")
                .insert_header("etag", "\"original\"")
                .set_body_bytes(encrypt_with_key(b"content", &[2; 32]).unwrap()),
        )
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("PUT"))
        .and(path("/bucket/file"))
        .and(header("if-match", "\"original\""))
        .and(header("x-amz-meta-near-enc-key-id", "current"))
        .respond_with(ResponseTemplate::new(200))
        .expect(1)
        .mount(&server)
        .await;
    // When execution recovers the replacement object.
    let outcome = storage(&server)
        .await
        .rewrap_object("file", 35)
        .await
        .unwrap();
    // Then the recovered unknown id reaches the worker as a distinct outcome.
    assert_eq!(outcome, RewrapOutcome::RewrappedUnknownKeyId);
}
