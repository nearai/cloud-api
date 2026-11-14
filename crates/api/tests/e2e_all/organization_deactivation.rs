//! An inactive organization must not authenticate API keys or gain new children.
//!
//! The organization is deactivated with SQL: the tests cover the guards that
//! sit below any route, not how an organization comes to be inactive.

use crate::common::*;

async fn deactivate_organization(database: &database::Database, organization_id: uuid::Uuid) {
    let client = database
        .pool()
        .get()
        .await
        .expect("failed to get database connection");
    let updated = client
        .execute(
            "UPDATE organizations SET is_active = false WHERE id = $1",
            &[&organization_id],
        )
        .await
        .expect("failed to deactivate organization");
    assert_eq!(updated, 1, "exactly one organization should be deactivated");
}

/// Production AuthService caches a successful API-key validation for 30
/// seconds. A cache hit must still be rejected on a key-only route after its
/// organization is deactivated.
#[tokio::test]
async fn test_cached_api_key_is_rejected_after_organization_deactivation() {
    // Use the normal mock-backed server only to create fixture rows. The
    // separate middleware below uses a real AuthService, constructed after the
    // key exists so its initial bloom-filter load includes the key.
    let (fixture_server, database) = setup_test_server_with_database().await;
    let org = create_org(&fixture_server).await;
    let api_key = get_api_key_for_org(&fixture_server, org.id.clone()).await;

    let mut config = test_config();
    config.auth.mock = false;
    let auth_components = api::init_auth_services(database.clone(), &config);
    let auth_server = axum_test::TestServer::new(
        axum::Router::new()
            .route(
                "/",
                axum::routing::get(|| async { axum::http::StatusCode::NO_CONTENT }),
            )
            .layer(axum::middleware::from_fn_with_state(
                auth_components.auth_state_middleware,
                api::middleware::auth::auth_middleware_with_api_key,
            )),
    );

    // This populates the real AuthService cache.
    let response = auth_server
        .get("/")
        .add_header("Authorization", format!("Bearer {api_key}"))
        .await;
    assert_eq!(response.status_code(), 204, "{}", response.text());

    deactivate_organization(
        &database,
        uuid::Uuid::parse_str(&org.id).expect("organization id"),
    )
    .await;

    // Without the parent lookup in auth_middleware_with_api_key this would use
    // the still-cached key and return 204 until the cache entry expired.
    let response = auth_server
        .get("/")
        .add_header("Authorization", format!("Bearer {api_key}"))
        .await;
    assert_eq!(
        response.status_code(),
        401,
        "a cached key must not authenticate after parent deactivation: {}",
        response.text()
    );
}

/// A workspace must not be insertable under an inactive organization. The
/// service-layer membership check reads `organization_members`, which
/// deactivation does not touch, so only the insert's own guard stops the orphan.
#[tokio::test]
async fn test_workspace_cannot_be_created_under_inactive_organization() {
    let (server, database) = setup_test_server_with_database().await;
    let org = create_org(&server).await;
    let organization_id = uuid::Uuid::parse_str(&org.id).expect("organization id");
    let user_id = uuid::Uuid::parse_str(&org.owner_id).expect("owner id");

    deactivate_organization(&database, organization_id).await;

    let repository = database::repositories::WorkspaceRepository::new(database.pool().clone());
    let result = repository
        .create(
            database::models::CreateWorkspaceRequest {
                name: "raced-orphan".to_string(),
                description: None,
            },
            organization_id,
            user_id,
        )
        .await;
    assert!(
        result.is_err(),
        "the insert itself must refuse an inactive parent org"
    );

    let client = database
        .pool()
        .get()
        .await
        .expect("failed to get database connection");
    let orphans: i64 = client
        .query_one(
            "SELECT COUNT(*) FROM workspaces \
             WHERE organization_id = $1 AND name = 'raced-orphan'",
            &[&organization_id],
        )
        .await
        .expect("failed to count workspaces")
        .get(0);
    assert_eq!(
        orphans, 0,
        "no workspace may be written under an inactive org"
    );
}

/// An API key must not be insertable under a workspace of an inactive
/// organization.
#[tokio::test]
async fn test_api_key_cannot_be_created_under_inactive_organization() {
    let (server, database) = setup_test_server_with_database().await;
    let org = create_org(&server).await;
    let workspace_id = list_workspaces(&server, org.id.clone())
        .await
        .first()
        .expect("new org should have a default workspace")
        .id
        .clone();
    let organization_id = uuid::Uuid::parse_str(&org.id).expect("organization id");
    let workspace_id = uuid::Uuid::parse_str(&workspace_id).expect("workspace id");
    let user_id = uuid::Uuid::parse_str(&org.owner_id).expect("owner id");

    deactivate_organization(&database, organization_id).await;

    let result = database::repositories::ApiKeyRepository::new(database.pool().clone())
        .create(services::workspace::CreateApiKeyRequest {
            name: "raced-api-key".to_string(),
            workspace_id: services::workspace::WorkspaceId(workspace_id),
            created_by_user_id: services::auth::UserId(user_id),
            expires_at: None,
            spend_limit: None,
        })
        .await;
    assert!(
        result.is_err(),
        "the insert itself must refuse a workspace under an inactive org"
    );

    let client = database
        .pool()
        .get()
        .await
        .expect("failed to get database connection");
    let keys: i64 = client
        .query_one(
            "SELECT COUNT(*) FROM api_keys WHERE workspace_id = $1 AND name = 'raced-api-key'",
            &[&workspace_id],
        )
        .await
        .expect("failed to count api keys")
        .get(0);
    assert_eq!(keys, 0, "no API key may be written under an inactive org");
}
