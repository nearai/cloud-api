//! End-to-end tests for server-side session revocation on logout
//! (nearai/infra#191).
//!
//! These tests run against the real `AuthService` (`auth.mock = false`) and
//! the real PostgreSQL-backed session repository, seeding users and sessions
//! directly in the shared test database.

use crate::common::*;
use database::repositories::SessionRepository;
use services::auth::{AuthServiceTrait, MockAuthService, UserId};
use std::sync::Arc;

/// Browser-like User-Agent; the session repository normalizes version
/// numbers, so any consistent value works.
const TEST_UA: &str = "Mozilla/5.0 (X11; Linux x86_64) TestBrowser/1.0";

async fn real_auth_server() -> (axum_test::TestServer, Arc<database::Database>) {
    setup_test_server_with_config_and_database(|c| c.auth.mock = false).await
}

/// Insert a unique user directly in the database and return its id.
///
/// `created_at` is backdated so these users sort below the shared mock user
/// in `ORDER BY created_at DESC` admin listings — other e2e tests page
/// through those with a fixed limit and expect the mock user on page one.
async fn create_real_user(database: &Arc<database::Database>) -> uuid::Uuid {
    let user_id = uuid::Uuid::new_v4();
    let user_id_str = user_id.to_string();
    let client = database
        .pool()
        .get()
        .await
        .expect("Failed to get database connection");
    client.execute(
        "INSERT INTO users (id, email, username, display_name, avatar_url, auth_provider, provider_user_id, created_at, updated_at)
         VALUES ($1, $2, $3, $4, $5, $6, $7, NOW() - INTERVAL '30 days', NOW())",
        &[
            &user_id,
            &format!("logout-test-{user_id_str}@test.com"),
            &format!("logout-test-{user_id_str}"),
            &Some("Logout Test User".to_string()),
            &None::<String>,
            &"mock",
            &format!("logout-test-{user_id_str}"),
        ],
    ).await.expect("Failed to create test user");
    user_id
}

/// Create a refresh-token session for the user directly via the repository.
/// Returns (session_id, plaintext_refresh_token).
async fn create_session(
    database: &Arc<database::Database>,
    user_id: uuid::Uuid,
) -> (uuid::Uuid, String) {
    let repo = SessionRepository::new(database.pool().clone());
    let (session, refresh_token) = repo
        .create(user_id, None, TEST_UA.to_string(), 24)
        .await
        .expect("Failed to create session");
    (session.id, refresh_token)
}

/// Exchange a refresh token for an access token + rotated refresh token over HTTP.
async fn mint_tokens(
    server: &axum_test::TestServer,
    refresh_token: &str,
) -> api::models::AccessAndRefreshTokenResponse {
    let response = server
        .post("/v1/users/me/access-tokens")
        .add_header("Authorization", format!("Bearer {refresh_token}"))
        .add_header("User-Agent", TEST_UA)
        .await;
    assert_eq!(response.status_code(), 200, "token mint should succeed");
    response.json::<api::models::AccessAndRefreshTokenResponse>()
}

async fn me_status(server: &axum_test::TestServer, access_token: &str) -> u16 {
    server
        .get("/v1/users/me")
        .add_header("Authorization", format!("Bearer {access_token}"))
        .await
        .status_code()
        .as_u16()
}

async fn logout_status(server: &axum_test::TestServer, refresh_token: &str) -> u16 {
    server
        .post("/v1/auth/logout")
        .add_header("Authorization", format!("Bearer {refresh_token}"))
        .add_header("User-Agent", TEST_UA)
        .await
        .status_code()
        .as_u16()
}

async fn session_row_exists(database: &Arc<database::Database>, session_id: uuid::Uuid) -> bool {
    let repo = SessionRepository::new(database.pool().clone());
    repo.get_by_id(session_id)
        .await
        .expect("Failed to query session")
        .is_some()
}

/// Delete a test user (sessions cascade) so repeated runs don't accumulate
/// rows in the shared e2e database. Deletes by id, so it cannot interfere
/// with users created by concurrently running tests.
async fn cleanup_user(database: &Arc<database::Database>, user_id: uuid::Uuid) {
    let client = database
        .pool()
        .get()
        .await
        .expect("Failed to get database connection");
    client
        .execute("DELETE FROM users WHERE id = $1", &[&user_id])
        .await
        .expect("Failed to clean up test user");
}

#[tokio::test]
async fn test_logout_invalidates_access_and_refresh_tokens() {
    let (server, database) = real_auth_server().await;
    let user_id = create_real_user(&database).await;
    let (session_id, refresh_token) = create_session(&database, user_id).await;

    // Mint an access token; this rotates the refresh token but keeps the
    // same session row.
    let tokens = mint_tokens(&server, &refresh_token).await;

    // Both credentials work before logout.
    assert_eq!(me_status(&server, &tokens.access_token).await, 200);

    // Logout with the current refresh token revokes the session.
    assert_eq!(logout_status(&server, &tokens.refresh_token).await, 200);

    // The still-unexpired access token is rejected immediately.
    assert_eq!(
        me_status(&server, &tokens.access_token).await,
        401,
        "access token must be invalid after logout"
    );

    // The refresh token is rejected as well.
    let response = server
        .post("/v1/users/me/access-tokens")
        .add_header("Authorization", format!("Bearer {}", tokens.refresh_token))
        .add_header("User-Agent", TEST_UA)
        .await;
    assert_eq!(
        response.status_code(),
        401,
        "refresh token must be invalid after logout"
    );

    // The session row was removed.
    assert!(
        !session_row_exists(&database, session_id).await,
        "refresh-token session row must be gone after logout"
    );

    cleanup_user(&database, user_id).await;
}

#[tokio::test]
async fn test_logout_revokes_only_the_current_session() {
    let (server, database) = real_auth_server().await;
    let user_id = create_real_user(&database).await;
    let (session_a_id, refresh_a) = create_session(&database, user_id).await;
    let (session_b_id, refresh_b) = create_session(&database, user_id).await;

    let tokens_a = mint_tokens(&server, &refresh_a).await;
    let tokens_b = mint_tokens(&server, &refresh_b).await;

    // Log out session A only.
    assert_eq!(logout_status(&server, &tokens_a.refresh_token).await, 200);

    // Session A is invalidated immediately.
    assert_eq!(me_status(&server, &tokens_a.access_token).await, 401);
    assert!(!session_row_exists(&database, session_a_id).await);

    // Session B stays fully valid: access token works and the refresh token
    // can still mint new access tokens.
    assert_eq!(me_status(&server, &tokens_b.access_token).await, 200);
    assert!(session_row_exists(&database, session_b_id).await);
    let rotated_b = mint_tokens(&server, &tokens_b.refresh_token).await;
    assert_eq!(me_status(&server, &rotated_b.access_token).await, 200);

    cleanup_user(&database, user_id).await;
}

#[tokio::test]
async fn test_repeated_logout_is_safe() {
    let (server, database) = real_auth_server().await;
    let user_id = create_real_user(&database).await;
    let (session_id, refresh_token) = create_session(&database, user_id).await;

    assert_eq!(logout_status(&server, &refresh_token).await, 200);

    // A second logout with the same refresh token is rejected by the
    // refresh-token middleware and neither restores nor rotates the session.
    assert_eq!(logout_status(&server, &refresh_token).await, 401);
    assert!(!session_row_exists(&database, session_id).await);

    let repo = SessionRepository::new(database.pool().clone());
    let remaining = repo
        .list_by_user(user_id)
        .await
        .expect("Failed to list sessions");
    assert!(
        remaining.is_empty(),
        "repeated logout must not create or restore sessions"
    );

    cleanup_user(&database, user_id).await;
}

#[tokio::test]
async fn test_fresh_login_after_logout_creates_independent_session() {
    let (server, database) = real_auth_server().await;
    let user_id = create_real_user(&database).await;
    let (_, refresh_token) = create_session(&database, user_id).await;
    let tokens = mint_tokens(&server, &refresh_token).await;

    assert_eq!(logout_status(&server, &tokens.refresh_token).await, 200);
    assert_eq!(me_status(&server, &tokens.access_token).await, 401);

    // A fresh login (new session) works and is independently revocable.
    let (new_session_id, new_refresh) = create_session(&database, user_id).await;
    let new_tokens = mint_tokens(&server, &new_refresh).await;
    assert_eq!(me_status(&server, &new_tokens.access_token).await, 200);

    // The old access token stays dead; revoking the new session kills only it.
    assert_eq!(me_status(&server, &tokens.access_token).await, 401);
    assert_eq!(logout_status(&server, &new_tokens.refresh_token).await, 200);
    assert_eq!(me_status(&server, &new_tokens.access_token).await, 401);
    assert!(!session_row_exists(&database, new_session_id).await);

    cleanup_user(&database, user_id).await;
}

#[tokio::test]
async fn test_legacy_access_token_without_sid_follows_compat_flag() {
    // Legacy tokens carry no `sid` claim. Mint one with the same signing key
    // the servers use (MockAuthService::create_session_access_token skips the
    // claim entirely when the session id is None).
    let (server, database) = real_auth_server().await;
    let user_id = create_real_user(&database).await;

    let minter = MockAuthService {
        apikey_repository: Arc::new(database::repositories::ApiKeyRepository::new(
            database.pool().clone(),
        )),
    };
    // Sign with the same key the test servers use.
    let encoding_key = test_config().auth.encoding_key;
    let legacy_token = minter
        .create_session_access_token(UserId(user_id), None, encoding_key, 1)
        .expect("Failed to mint legacy token");

    // Default (compat window): legacy tokens still validate.
    assert_eq!(me_status(&server, &legacy_token).await, 200);

    // With the cutover flag on, tokens that cannot be tied to a live
    // session are rejected...
    let (strict_server, strict_database) = setup_test_server_with_config_and_database(|c| {
        c.auth.mock = false;
        c.auth.require_session_bound_access_tokens = true;
    })
    .await;
    assert_eq!(me_status(&strict_server, &legacy_token).await, 401);

    // ...while session-bound tokens keep working.
    let strict_user_id = create_real_user(&strict_database).await;
    let (_, strict_refresh) = create_session(&strict_database, strict_user_id).await;
    let strict_tokens = mint_tokens(&strict_server, &strict_refresh).await;
    assert_eq!(
        me_status(&strict_server, &strict_tokens.access_token).await,
        200
    );

    cleanup_user(&database, user_id).await;
    cleanup_user(&strict_database, strict_user_id).await;
}

#[tokio::test]
async fn test_logout_requires_refresh_token_auth() {
    let (server, _database) = real_auth_server().await;

    // No Authorization header.
    let response = server
        .post("/v1/auth/logout")
        .add_header("User-Agent", TEST_UA)
        .await;
    assert_eq!(response.status_code(), 401);

    // Invalid refresh token.
    assert_eq!(
        logout_status(&server, "rt_definitely_not_a_session").await,
        401
    );
}
