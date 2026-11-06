//! An API key must stop working once its organization is deleted, including a key whose
//! validation is cached.

use crate::common::*;

/// Every credential under a deleted organization must stop working: the API-key
/// validation path requires an active parent.
#[tokio::test]
async fn test_deleted_organization_rejects_its_api_key() {
    let (server, database) = setup_test_server_with_database().await;
    let (session_id, _email) = signup_new_user(&database).await;

    let org = create_org_with_session(&server, &session_id).await;
    let api_key = get_api_key_for_org_with_session(&server, org.id.clone(), &session_id).await;

    // Exercise a route that accepts only an API key before deleting the org.
    // The same key-only path must fail once the parent is gone.
    let response = server
        .get("/v1/files?limit=1")
        .add_header("Authorization", format!("Bearer {api_key}"))
        .add_header("User-Agent", MOCK_USER_AGENT)
        .await;
    assert_eq!(response.status_code(), 200, "{}", response.text());

    seeding::seed_delete_organization(&org.id).await;

    let response = server
        .get("/v1/files?limit=1")
        .add_header("Authorization", format!("Bearer {api_key}"))
        .add_header("User-Agent", MOCK_USER_AGENT)
        .await;
    assert_eq!(
        response.status_code(),
        401,
        "an API key of a deleted org must not authenticate on a key-only route: {}",
        response.text()
    );
}

/// Production AuthService caches a successful API-key validation for 30
/// seconds. A cache hit must still be rejected on a key-only route after its
/// organization is deleted.
#[tokio::test]
async fn test_cached_api_key_is_rejected_after_organization_deletion() {
    // Use the normal mock-backed server only to create fixture rows. The
    // separate middleware below uses a real AuthService, constructed after the
    // key exists so its initial bloom-filter load includes the key.
    let (fixture_server, database) = setup_test_server_with_database().await;
    let (session_id, _email) = signup_new_user(&database).await;
    let org = create_org_with_session(&fixture_server, &session_id).await;
    let api_key =
        get_api_key_for_org_with_session(&fixture_server, org.id.clone(), &session_id).await;

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

    seeding::seed_delete_organization(&org.id).await;

    // Without the parent lookup in auth_middleware_with_api_key this would use
    // the still-cached key and return 204 until the cache entry expired.
    let response = auth_server
        .get("/")
        .add_header("Authorization", format!("Bearer {api_key}"))
        .await;
    assert_eq!(
        response.status_code(),
        401,
        "a cached key must not authenticate after parent deletion: {}",
        response.text()
    );
}

/// Run the real signup path (the one OAuth and NEAR logins both go through), so
/// the user starts with the default organization and workspace production gives
/// them. The e2e server itself runs with the mock auth service, which maps an
/// `rt_{user_id}` bearer token onto that user.
async fn signup_new_user(database: &std::sync::Arc<database::Database>) -> (String, uuid::Uuid) {
    use services::auth::ports::OAuthUserInfo;

    let mut config = test_config();
    config.auth.mock = false;
    let auth_components = api::init_auth_services(database.clone(), &config);

    let unique = uuid::Uuid::new_v4();
    let user = auth_components
        .auth_service
        .get_or_create_oauth_user(OAuthUserInfo {
            email_verified: true,
            email_authoritative: true,
            provider: "github".to_string(),
            provider_user_id: format!("gh-{unique}"),
            email: format!("newuser-{unique}@test.com"),
            username: format!("newuser-{unique}"),
            display_name: Some("New User".to_string()),
            avatar_url: None,
        })
        .await
        .expect("signup should create the user");

    (format!("rt_{}", user.id.0), user.id.0)
}
