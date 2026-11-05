use super::*;

#[path = "../../common/object_encryption_missing.rs"]
mod storage_fixture;

async fn fixture(mode: &str, fail: bool) -> (TestServer, Arc<MockStorage>) {
    let ring = ring();
    let (wrapped, storage) = storage_fixture::storage(mode, fail, ring.clone()).await;
    let (server, _) = server(wrapped, ring).await;
    (server, storage)
}

#[tokio::test]
async fn deleted_after_listing_is_skipped_in_all_job_modes() {
    for mode in ["dry_run", "execute", "verify"] {
        // When the listed object disappears before processing.
        let (server, storage) = fixture(mode, false).await;
        let actions = if mode == "verify" {
            ["verify"]
        } else {
            ["rewrap"]
        };
        let response = post(
            &server,
            "jobs",
            json!({"mode": mode, "actions": actions, "batch_size": 1}),
        )
        .await;
        assert_eq!(response.status_code(), 202);
        let result = wait(
            &server,
            response.json::<Value>()["job_id"].as_str().unwrap(),
        )
        .await;
        assert_eq!(result["status"], "completed");
        assert_eq!(result["progress"]["missing"], 1);
        assert_eq!(result["progress"]["missing_keys"], json!(["a-deleted"]));
        assert_eq!(result["cursor"]["complete"], true);
        assert_eq!(result["progress"]["processed"], 2);
        if mode == "verify" {
            assert_eq!(result["progress"]["pass"], true);
        } else if mode == "execute" {
            assert_eq!(result["progress"]["rewrapped"], 1);
        } else {
            assert_eq!(result["progress"]["by_key_id"], json!({"s3-v2": 1}));
        }
        assert_eq!(
            storage.object_key_id("b-retained").await.unwrap(),
            Some("s3-v2".into())
        );
    }
}

#[tokio::test]
async fn non_not_found_storage_error_fails_the_job() {
    // Given a storage adapter returning a non-NotFound failure.
    let (server, _) = fixture("dry_run", true).await;
    // When the job is submitted.
    let response = post(
        &server,
        "jobs",
        json!({"mode":"dry_run","actions":["rewrap"],"batch_size":1}),
    )
    .await;
    assert_eq!(response.status_code(), 202);
    // Then the worker reports its failure class.
    let result = wait_failed(
        &server,
        response.json::<Value>()["job_id"].as_str().unwrap(),
    )
    .await;
    assert_eq!(result["status"], "failed");
    assert_eq!(result["last_error_class"], "object_worker_failed");
}

async fn wait_failed(server: &TestServer, id: &str) -> Value {
    tokio::time::timeout(Duration::from_secs(15), async {
        loop {
            let response = server
                .get(&format!("/v1/admin/object-encryption/jobs/{id}"))
                .add_header("Authorization", format!("Bearer {}", get_session_id()))
                .add_header("User-Agent", MOCK_USER_AGENT)
                .await;
            assert_eq!(response.status_code(), 200);
            let body = response.json::<Value>();
            match body["status"].as_str() {
                Some("failed") => return body,
                _ => tokio::time::sleep(Duration::from_millis(10)).await,
            }
        }
    })
    .await
    .expect("object job failure timeout")
}
