use crate::common::*;

#[tokio::test]
async fn test_feature_request_submit_dedupes_by_user_and_admin_lists() {
    let (server, database) = setup_test_server_with_database().await;
    let request_key = format!("missing-model-{}", uuid::Uuid::new_v4());
    let request_title = format!("Missing Model {}", uuid::Uuid::new_v4());

    let first = server
        .post("/v1/feature-requests")
        .add_header("Authorization", format!("Bearer {}", get_session_id()))
        .add_header("User-Agent", MOCK_USER_AGENT)
        .json(&serde_json::json!({
            "kind": "model",
            "key": request_key,
            "title": request_title,
            "note": "Need it for evals",
            "source": "models_page"
        }))
        .await;
    assert_eq!(first.status_code(), 200);
    let first_body: serde_json::Value = first.json();
    assert_eq!(first_body["uniqueUserCount"], 1);

    let duplicate = server
        .post("/v1/feature-requests")
        .add_header("Authorization", format!("Bearer {}", get_session_id()))
        .add_header("User-Agent", MOCK_USER_AGENT)
        .json(&serde_json::json!({
            "kind": "model",
            "key": request_key,
            "title": "A later voter should not replace the title",
            "note": "Updated use case",
            "source": "models_page"
        }))
        .await;
    assert_eq!(duplicate.status_code(), 200);
    let duplicate_body: serde_json::Value = duplicate.json();
    assert_eq!(duplicate_body["uniqueUserCount"], 1);

    let (second_session, _) = setup_unique_test_session(&database).await;
    let second = server
        .post("/v1/feature-requests")
        .add_header("Authorization", format!("Bearer {second_session}"))
        .add_header("User-Agent", MOCK_USER_AGENT)
        .json(&serde_json::json!({
            "kind": "model",
            "key": request_key,
            "title": "Another later voter should not replace the title",
            "note": "Another user also needs this",
            "source": "models_page"
        }))
        .await;
    assert_eq!(second.status_code(), 200);
    let second_body: serde_json::Value = second.json();
    assert_eq!(second_body["uniqueUserCount"], 2);

    let admin_list = server
        .get("/v1/admin/feature-requests?kind=model&limit=100&offset=0")
        .add_header("Authorization", format!("Bearer {}", get_session_id()))
        .add_header("User-Agent", MOCK_USER_AGENT)
        .await;
    assert_eq!(admin_list.status_code(), 200);
    let admin_body: serde_json::Value = admin_list.json();
    let requests = admin_body["requests"]
        .as_array()
        .expect("requests should be an array");
    let found = requests
        .iter()
        .find(|item| item["target"]["key"] == request_key.to_ascii_lowercase())
        .expect("submitted request should be listed");
    assert_eq!(found["target"]["title"], request_title);
    assert_eq!(found["uniqueUserCount"], 2);
    assert_eq!(found["recentVotes"].as_array().unwrap().len(), 2);
}

#[tokio::test]
async fn test_feature_request_validation_and_auth() {
    let server = setup_test_server().await;

    let unauthenticated = server
        .get("/v1/admin/feature-requests?limit=10&offset=0")
        .add_header("User-Agent", MOCK_USER_AGENT)
        .await;
    assert_eq!(unauthenticated.status_code(), 401);

    let empty_key = server
        .post("/v1/feature-requests")
        .add_header("Authorization", format!("Bearer {}", get_session_id()))
        .add_header("User-Agent", MOCK_USER_AGENT)
        .json(&serde_json::json!({
            "kind": "model",
            "key": "",
            "title": "Missing model"
        }))
        .await;
    assert_eq!(empty_key.status_code(), 400);

    let invalid_kind = server
        .post("/v1/feature-requests")
        .add_header("Authorization", format!("Bearer {}", get_session_id()))
        .add_header("User-Agent", MOCK_USER_AGENT)
        .json(&serde_json::json!({
            "kind": "integration",
            "key": "some-model",
            "title": "Some Model"
        }))
        .await;
    assert_eq!(invalid_kind.status_code(), 400);

    let long_note = "x".repeat(2001);
    let oversized_note = server
        .post("/v1/feature-requests")
        .add_header("Authorization", format!("Bearer {}", get_session_id()))
        .add_header("User-Agent", MOCK_USER_AGENT)
        .json(&serde_json::json!({
            "kind": "model",
            "key": "some-model",
            "title": "Some Model",
            "note": long_note
        }))
        .await;
    assert_eq!(oversized_note.status_code(), 400);
}
