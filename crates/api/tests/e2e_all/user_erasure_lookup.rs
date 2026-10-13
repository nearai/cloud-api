//! Admin lookup of user erasure records by email or user id.

use crate::common::*;
use crate::user_erasure::{add_member, erase, new_user, personal_org_id, signup};

async fn lookup(
    server: &axum_test::TestServer,
    token: &str,
    body: serde_json::Value,
) -> axum_test::TestResponse {
    server
        .post("/v1/admin/user-erasures/lookup")
        .add_header("Authorization", format!("Bearer {token}"))
        .add_header("User-Agent", MOCK_USER_AGENT)
        .json(&body)
        .await
}

async fn lookup_ok(
    server: &axum_test::TestServer,
    body: serde_json::Value,
) -> Vec<serde_json::Value> {
    let r = lookup(server, &get_session_id(), body).await;
    assert_eq!(r.status_code(), 200, "{}", r.text());
    r.json::<serde_json::Value>()["erasures"]
        .as_array()
        .expect("erasures array")
        .clone()
}

#[tokio::test]
async fn lookup_by_email_finds_erasure_with_org_lifecycles() {
    let (server, database) = setup_test_server_with_database().await;
    let (session, user_id, email) = new_user(&database).await;
    let (other_session, _other, _) = new_user(&database).await;
    let personal = personal_org_id(&server, &session).await;
    let team = create_org_with_session(&server, &other_session).await;
    add_member(&database, &team.id, user_id, "member").await;
    assert_eq!(erase(&server, user_id, &email).await.status_code(), 200);

    let items = lookup_ok(
        &server,
        serde_json::json!({ "email": format!("  {}  ", email.to_uppercase()) }),
    )
    .await;
    assert_eq!(items.len(), 1, "{items:?}");
    let item = &items[0];
    assert_eq!(item["user_id"], user_id.to_string());
    assert_eq!(item["user_lifecycle"], "erased");
    assert_eq!(
        item["erased_organizations"],
        serde_json::json!([{ "organization_id": personal, "lifecycle": "erased" }])
    );
    assert_eq!(
        item["retained_organizations"],
        serde_json::json!([{ "organization_id": team.id, "lifecycle": "active" }])
    );
    assert!(item["admin_user_id"].is_string());
    assert!(item["requested_at"].is_string());
    assert!(item["erased_at"].is_string());
}

#[tokio::test]
async fn lookup_by_user_id_finds_erasure() {
    let (server, database) = setup_test_server_with_database().await;
    let (_session, user_id, email) = new_user(&database).await;
    assert_eq!(erase(&server, user_id, &email).await.status_code(), 200);

    let items = lookup_ok(
        &server,
        serde_json::json!({ "user_id": user_id.to_string() }),
    )
    .await;
    assert_eq!(items.len(), 1, "{items:?}");
    assert_eq!(items[0]["user_id"], user_id.to_string());
    assert_eq!(items[0]["user_lifecycle"], "erased");
}

#[tokio::test]
async fn lookup_unknown_email_returns_empty() {
    let (server, _database) = setup_test_server_with_database().await;
    let email = format!("never-{}@test.com", uuid::Uuid::new_v4());
    let items = lookup_ok(&server, serde_json::json!({ "email": email })).await;
    assert!(items.is_empty());
    let by_id = lookup_ok(
        &server,
        serde_json::json!({ "user_id": uuid::Uuid::new_v4().to_string() }),
    )
    .await;
    assert!(by_id.is_empty());
}

#[tokio::test]
async fn lookup_returns_every_erasure_of_a_reused_email() {
    let (server, database) = setup_test_server_with_database().await;
    let u = uuid::Uuid::new_v4();
    let provider_id = format!("gh-{u}");
    let email = format!("reuse-{u}@test.com");
    let (_s1, first, _) = signup(&database, "github", &provider_id, &email).await;
    assert_eq!(erase(&server, first, &email).await.status_code(), 200);
    let (_s2, second, _) = signup(&database, "github", &provider_id, &email).await;
    assert_ne!(first, second);
    assert_eq!(erase(&server, second, &email).await.status_code(), 200);

    let items = lookup_ok(&server, serde_json::json!({ "email": email })).await;
    assert_eq!(items.len(), 2, "{items:?}");
    assert_eq!(items[0]["user_id"], second.to_string(), "newest first");
    assert_eq!(items[1]["user_id"], first.to_string());
    let erased_at = |item: &serde_json::Value| {
        item["erased_at"]
            .as_str()
            .expect("erased_at is a string")
            .parse::<chrono::DateTime<chrono::Utc>>()
            .expect("erased_at is RFC 3339")
    };
    assert!(erased_at(&items[0]) >= erased_at(&items[1]));
}

#[tokio::test]
async fn lookup_requires_admin_session() {
    let (server, _database) = setup_test_server_with_database().await;
    let created = server
        .post("/v1/admin/access-tokens")
        .add_header("Authorization", format!("Bearer {}", get_session_id()))
        .add_header("User-Agent", MOCK_USER_AGENT)
        .json(&serde_json::json!({
            "name": format!("erasure-lookup-{}", uuid::Uuid::new_v4()),
            "reason": "erasure lookup access token test",
            "expires_in_hours": 1
        }))
        .await;
    assert_eq!(created.status_code(), 200, "{}", created.text());
    let token = created.json::<serde_json::Value>()["access_token"]
        .as_str()
        .unwrap()
        .to_string();

    let r = lookup(
        &server,
        &token,
        serde_json::json!({ "email": "someone@test.com" }),
    )
    .await;
    assert_eq!(r.status_code(), 403, "{}", r.text());
}

#[tokio::test]
async fn lookup_rejects_both_or_neither() {
    let (server, _database) = setup_test_server_with_database().await;
    for body in [
        serde_json::json!({}),
        serde_json::json!({ "email": "a@test.com", "user_id": uuid::Uuid::new_v4().to_string() }),
        serde_json::json!({ "user_id": "not-a-uuid" }),
        serde_json::json!({ "email": "   ", "user_id": uuid::Uuid::new_v4().to_string() }),
        serde_json::json!({ "email": "   " }),
    ] {
        let r = lookup(&server, &get_session_id(), body.clone()).await;
        assert_eq!(r.status_code(), 400, "{body} {}", r.text());
        assert_eq!(
            r.json::<serde_json::Value>()["error"]["type"],
            "validation_error"
        );
    }
}
