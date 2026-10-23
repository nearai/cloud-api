use super::*;

#[tokio::test]
async fn admin_user_list_shows_lifecycle() {
    let (server, database) = setup_test_server_with_database().await;
    let (_session, user_id, email) = new_user(&database).await;
    assert_eq!(erase(&server, user_id, &email).await.status_code(), 200);
    let users = server
        .get(format!("/v1/admin/users?search={user_id}").as_str())
        .add_header("Authorization", format!("Bearer {}", get_session_id()))
        .await
        .json::<serde_json::Value>();
    let user = users["users"]
        .as_array()
        .unwrap()
        .iter()
        .find(|u| u["id"] == user_id.to_string())
        .unwrap_or_else(|| panic!("erased user missing from list: {users}"));
    assert_eq!(user["lifecycle"], "erased");
}

/// Bounded scan of the admin org list for `org_id` under an optional lifecycle filter.
async fn org_list_contains(
    server: &axum_test::TestServer,
    lifecycle: Option<&str>,
    org_id: &str,
) -> bool {
    const LIMIT: usize = 50;
    for page in 0..20 {
        let filter = lifecycle
            .map(|l| format!("&lifecycle={l}"))
            .unwrap_or_default();
        let body = server
            .get(
                format!(
                    "/v1/admin/organizations?limit={LIMIT}&offset={}{filter}",
                    page * LIMIT
                )
                .as_str(),
            )
            .add_header("Authorization", format!("Bearer {}", get_session_id()))
            .await
            .json::<serde_json::Value>();
        let orgs = body["organizations"].as_array().unwrap();
        if orgs.iter().any(|o| o["id"] == org_id) {
            return true;
        }
        if orgs.is_empty() {
            return false;
        }
    }
    false
}

#[tokio::test]
async fn admin_org_list_lifecycle_filter_returns_only_matching_rows() {
    let (server, database) = setup_test_server_with_database().await;
    let (session, user_id, email) = new_user(&database).await;
    let erased_org = personal_org_id(&server, &session).await;
    assert_eq!(erase(&server, user_id, &email).await.status_code(), 200);
    assert!(
        org_list_contains(&server, Some("erased"), &erased_org).await,
        "the erased org must be listed under lifecycle=erased"
    );
    assert!(
        !org_list_contains(&server, None, &erased_org).await,
        "the erased org must not be listed under the default filter"
    );
    for filter in ["erased", "deleted", "active"] {
        let body = server
            .get(format!("/v1/admin/organizations?lifecycle={filter}&limit=50&offset=0").as_str())
            .add_header("Authorization", format!("Bearer {}", get_session_id()))
            .await
            .json::<serde_json::Value>();
        // Property over whatever the first page holds; no completeness or ordering claim.
        assert!(
            body["organizations"]
                .as_array()
                .unwrap()
                .iter()
                .all(|o| o["lifecycle"] == filter),
            "{filter}: {body}"
        );
    }
}

#[tokio::test]
async fn deleted_org_with_live_member_is_listed_only_under_deleted() {
    let (server, database) = setup_test_server_with_database().await;
    let (owner_session, _owner, _) = new_user(&database).await;
    let (_member_session, member, _) = new_user(&database).await;
    // A non-default org owned by `owner` with a live second member.
    let org = create_org_with_session(&server, &owner_session).await;
    add_member(&database, &org.id, member, "member").await;
    let response = server
        .delete(&format!("/v1/organizations/{}", org.id))
        .add_header("Authorization", format!("Bearer {owner_session}"))
        .await;
    assert_eq!(response.status_code(), 200, "{}", response.text());

    assert!(
        org_list_contains(&server, Some("deleted"), &org.id).await,
        "an owner-deleted org must be listed under lifecycle=deleted"
    );
    assert!(
        !org_list_contains(&server, Some("erased"), &org.id).await,
        "an org with a live member is not erased"
    );
    assert!(
        !org_list_contains(&server, None, &org.id).await,
        "a deleted org must not be listed under the default filter"
    );
}
