// OAuth callback state and cross-provider account conflicts.

use crate::common::*;
use database::OAuthStateRepository;
use services::auth::{AuthError, AuthServiceTrait, OAuthUserInfo};
use std::sync::Arc;

fn oauth_info(provider: &str, email: &str) -> OAuthUserInfo {
    OAuthUserInfo {
        email_verified: true,
        provider: provider.into(),
        provider_user_id: uuid::Uuid::new_v4().to_string(),
        email: email.into(),
        username: email.split('@').next().unwrap().into(),
        display_name: Some(format!("{provider} profile")),
        avatar_url: None,
    }
}

fn real_auth(db: &Arc<database::Database>) -> Arc<dyn AuthServiceTrait> {
    let mut config = test_config();
    config.auth.mock = false;
    api::init_auth_services(db.clone(), &config).auth_service
}

async fn assert_single_oauth_account(db: &database::Database, user_id: uuid::Uuid, email: &str) {
    let client = db.pool().get().await.unwrap();
    let row = client
        .query_one(
            "SELECT
                (SELECT count(*) FROM users WHERE email = $2) AS users,
                (SELECT count(*) FROM organization_members WHERE user_id = $1) AS organizations,
                (SELECT count(*) FROM workspaces WHERE created_by_user_id = $1) AS workspaces,
                (SELECT count(*) FROM refresh_tokens WHERE user_id = $1) AS sessions",
            &[&user_id, &email],
        )
        .await
        .unwrap();
    assert_eq!(row.get::<_, i64>("users"), 1);
    assert_eq!(row.get::<_, i64>("organizations"), 1);
    assert_eq!(row.get::<_, i64>("workspaces"), 1);
    assert_eq!(row.get::<_, i64>("sessions"), 0);
}

#[tokio::test]
async fn oauth_matching_email_auto_links_only_verified_social_identities_in_both_directions() {
    let (_server, db) = setup_test_server_with_database().await;
    let auth = real_auth(&db);
    let users = database::repositories::UserRepository::new(db.pool().clone());
    for verified in [false, true] {
        for (original, incoming) in [("github", "google"), ("google", "github")] {
            let email = format!("oauth-auto-link-{}@example.test", uuid::Uuid::new_v4());
            let original_info = oauth_info(original, &email);
            let existing = auth
                .get_or_create_oauth_user(original_info.clone())
                .await
                .unwrap();
            let mut incoming_info = oauth_info(incoming, &email);
            incoming_info.email_verified = verified;
            for _ in 0..2 {
                let result = auth.get_or_create_oauth_user(incoming_info.clone()).await;
                if verified {
                    assert_eq!(result.unwrap().id, existing.id);
                } else {
                    assert!(
                        matches!(result, Err(AuthError::OAuthProviderConflict { existing_provider }) if existing_provider == original)
                    );
                }
            }
            let linked = users
                .get_by_provider(incoming, &incoming_info.provider_user_id)
                .await
                .unwrap();
            assert_eq!(
                linked.map(|user| user.id),
                verified.then_some(existing.id.0)
            );
            let stored = users.get_by_id(existing.id.0).await.unwrap().unwrap();
            assert_eq!(stored.auth_provider, original);
            assert_eq!(stored.provider_user_id, original_info.provider_user_id);
            assert_eq!(stored.email, email);
            assert_eq!(stored.display_name, existing.display_name);
            assert_eq!(stored.last_login_at.is_some(), verified);
            assert_single_oauth_account(&db, existing.id.0, &email).await;
            assert_eq!(
                auth.get_or_create_oauth_user(original_info)
                    .await
                    .unwrap()
                    .id,
                existing.id
            );
            if verified {
                // Do not replace an already linked identity for the same provider.
                let another_identity = oauth_info(incoming, &email);
                assert!(matches!(
                    auth.get_or_create_oauth_user(another_identity.clone())
                        .await,
                    Err(AuthError::OAuthProviderConflict { .. })
                ));
                assert!(users
                    .get_by_provider(incoming, &another_identity.provider_user_id)
                    .await
                    .unwrap()
                    .is_none());
            }
        }
    }
}

#[tokio::test]
async fn oauth_returning_user_email_conflict_preserves_both_accounts() {
    let (_server, db) = setup_test_server_with_database().await;
    let auth = real_auth(&db);
    let users = database::repositories::UserRepository::new(db.pool().clone());

    for (provider, owner_provider) in [("github", "google"), ("google", "github")] {
        let original_email = format!("oauth-original-{}@example.test", uuid::Uuid::new_v4());
        let claimed_email = format!("oauth-claimed-{}@example.test", uuid::Uuid::new_v4());
        let original_info = oauth_info(provider, &original_email);
        let user = auth
            .get_or_create_oauth_user(original_info.clone())
            .await
            .unwrap();
        let owner = auth
            .get_or_create_oauth_user(oauth_info(owner_provider, &claimed_email))
            .await
            .unwrap();
        let mut changed_info = original_info.clone();
        changed_info.email = claimed_email.clone();
        changed_info.display_name = Some("Must not be applied".into());

        let error = auth
            .get_or_create_oauth_user(changed_info)
            .await
            .unwrap_err();
        assert!(
            matches!(error, AuthError::OAuthProviderConflict { existing_provider }
            if existing_provider == owner_provider)
        );
        let stored = users.get_by_id(user.id.0).await.unwrap().unwrap();
        assert_eq!(stored.email, original_email);
        assert_eq!(stored.display_name, user.display_name);
        assert!(stored.last_login_at.is_none());
        let stored_owner = users.get_by_id(owner.id.0).await.unwrap().unwrap();
        assert_eq!(stored_owner.email, claimed_email);
        assert_eq!(stored_owner.auth_provider, owner_provider);
        assert!(stored_owner.last_login_at.is_none());
        assert_single_oauth_account(&db, user.id.0, &original_email).await;
        assert_single_oauth_account(&db, owner.id.0, &claimed_email).await;

        assert_eq!(
            auth.get_or_create_oauth_user(original_info)
                .await
                .unwrap()
                .id,
            user.id
        );
    }
}

#[tokio::test]
async fn oauth_concurrent_email_changes_return_one_conflict_without_mutating_loser() {
    let (_server, db) = setup_test_server_with_database().await;
    let auth = real_auth(&db);
    let users = database::repositories::UserRepository::new(db.pool().clone());
    let shared_email = format!("oauth-update-race-{}@example.test", uuid::Uuid::new_v4());
    let github_info = oauth_info(
        "github",
        &format!("oauth-github-{}@example.test", uuid::Uuid::new_v4()),
    );
    let google_info = oauth_info(
        "google",
        &format!("oauth-google-{}@example.test", uuid::Uuid::new_v4()),
    );
    let github = auth
        .get_or_create_oauth_user(github_info.clone())
        .await
        .unwrap();
    let google = auth
        .get_or_create_oauth_user(google_info.clone())
        .await
        .unwrap();
    let mut github_change = github_info;
    github_change.email = shared_email.clone();
    github_change.display_name = Some("Changed GitHub profile".into());
    let mut google_change = google_info;
    google_change.email = shared_email.clone();
    google_change.display_name = Some("Changed Google profile".into());

    let (github_result, google_result) = tokio::join!(
        auth.get_or_create_oauth_user(github_change),
        auth.get_or_create_oauth_user(google_change),
    );
    let (winner, loser, error) = match (github_result, google_result) {
        (Ok(_), Err(error)) => (github, google, error),
        (Err(error), Ok(_)) => (google, github, error),
        results => panic!("expected one email update and one conflict, got {results:?}"),
    };
    assert!(
        matches!(error, AuthError::OAuthProviderConflict { existing_provider }
        if existing_provider == winner.auth_provider)
    );
    let stored_winner = users.get_by_id(winner.id.0).await.unwrap().unwrap();
    assert_eq!(stored_winner.email, shared_email);
    assert!(stored_winner.last_login_at.is_some());
    let stored_loser = users.get_by_id(loser.id.0).await.unwrap().unwrap();
    assert_eq!(stored_loser.email, loser.email);
    assert_eq!(stored_loser.display_name, loser.display_name);
    assert!(stored_loser.last_login_at.is_none());
    assert_single_oauth_account(&db, winner.id.0, &shared_email).await;
    assert_single_oauth_account(&db, loser.id.0, &loser.email).await;
}

#[tokio::test]
async fn oauth_concurrent_cross_provider_signup_creates_only_one_account() {
    let (_server, db) = setup_test_server_with_database().await;
    let auth = real_auth(&db);
    let email = format!("oauth-race-{}@example.test", uuid::Uuid::new_v4());
    let github = oauth_info("github", &email);
    let google = oauth_info("google", &email);
    let (github_result, google_result) = tokio::join!(
        auth.get_or_create_oauth_user(github),
        auth.get_or_create_oauth_user(google),
    );
    let winner = github_result.unwrap();
    assert_eq!(google_result.unwrap().id, winner.id);
    assert_eq!(
        auth.list_oauth_identities(winner.id.clone())
            .await
            .unwrap()
            .len(),
        2
    );
    assert_single_oauth_account(&db, winner.id.0, &email).await;
}

#[tokio::test]
async fn oauth_concurrent_same_identity_signup_reuses_the_account() {
    let (_server, db) = setup_test_server_with_database().await;
    let auth = real_auth(&db);
    for provider in ["github", "google"] {
        let email = format!("oauth-identity-race-{}@example.test", uuid::Uuid::new_v4());
        let info = oauth_info(provider, &email);
        let (first, second) = tokio::join!(
            auth.get_or_create_oauth_user(info.clone()),
            auth.get_or_create_oauth_user(info),
        );
        let first = first.unwrap();
        assert_eq!(first.id, second.unwrap().id);
        assert_single_oauth_account(&db, first.id.0, &email).await;
    }
}

// ============================================
// OAuth Frontend Callback Tests
// ============================================

#[tokio::test]
async fn test_oauth_state_stores_frontend_callback_with_github() {
    let (_server, db) = setup_test_server_with_database().await;
    let repo = OAuthStateRepository::new(db.pool().clone());

    let state = format!("test-state-{}", uuid::Uuid::new_v4());
    let provider = "github".to_string();
    let frontend_callback = Some("https://app.example.com/auth/callback".to_string());

    // Create OAuth state with frontend_callback
    let created = repo
        .create(
            state.clone(),
            provider.clone(),
            None,
            frontend_callback.clone(),
        )
        .await
        .unwrap();

    assert_eq!(created.state, state);
    assert_eq!(created.provider, provider);
    assert_eq!(created.frontend_callback, frontend_callback);
    assert_eq!(created.pkce_verifier, None);

    // Retrieve and verify frontend_callback is preserved
    let retrieved = repo.get_and_delete(&state).await.unwrap();
    assert!(retrieved.is_some());

    let retrieved = retrieved.unwrap();
    assert_eq!(retrieved.frontend_callback, frontend_callback);

    println!("✅ OAuth state correctly stores and retrieves frontend_callback for GitHub");
}

#[tokio::test]
async fn test_oauth_state_stores_frontend_callback_with_google() {
    let (_server, db) = setup_test_server_with_database().await;
    let repo = OAuthStateRepository::new(db.pool().clone());

    let state = format!("test-state-{}", uuid::Uuid::new_v4());
    let provider = "google".to_string();
    let pkce_verifier = Some("test-pkce-verifier-string".to_string());
    let frontend_callback = Some("https://myapp.com/oauth/complete".to_string());

    // Create OAuth state with both PKCE verifier and frontend_callback
    let created = repo
        .create(
            state.clone(),
            provider.clone(),
            pkce_verifier.clone(),
            frontend_callback.clone(),
        )
        .await
        .unwrap();

    assert_eq!(created.state, state);
    assert_eq!(created.provider, provider);
    assert_eq!(created.pkce_verifier, pkce_verifier);
    assert_eq!(created.frontend_callback, frontend_callback);

    // Retrieve and verify both fields are preserved
    let retrieved = repo.get_and_delete(&state).await.unwrap().unwrap();
    assert_eq!(retrieved.pkce_verifier, pkce_verifier);
    assert_eq!(retrieved.frontend_callback, frontend_callback);

    println!("✅ OAuth state correctly stores and retrieves frontend_callback and PKCE verifier for Google");
}

#[tokio::test]
async fn test_oauth_state_without_frontend_callback() {
    let (_server, db) = setup_test_server_with_database().await;
    let repo = OAuthStateRepository::new(db.pool().clone());

    let state = format!("test-state-{}", uuid::Uuid::new_v4());
    let provider = "github".to_string();

    // Create OAuth state without frontend_callback (None)
    let created = repo
        .create(state.clone(), provider.clone(), None, None)
        .await
        .unwrap();

    assert_eq!(created.state, state);
    assert_eq!(created.provider, provider);
    assert_eq!(created.frontend_callback, None);

    // Retrieve and verify None is preserved
    let retrieved = repo.get_and_delete(&state).await.unwrap().unwrap();
    assert_eq!(retrieved.frontend_callback, None);

    println!("✅ OAuth state correctly handles None frontend_callback");
}

#[tokio::test]
async fn test_oauth_callback_with_valid_state_and_frontend_callback() {
    let (_server, db) = setup_test_server_with_database().await;
    let repo = OAuthStateRepository::new(db.pool().clone());

    // Simulate the OAuth flow: store a state with frontend_callback
    let state = format!("test-state-{}", uuid::Uuid::new_v4());
    let frontend_callback = "https://app.example.com/auth/complete";

    repo.create(
        state.clone(),
        "github".to_string(),
        None,
        Some(frontend_callback.to_string()),
    )
    .await
    .unwrap();

    // Verify the state can be retrieved with the frontend_callback
    let retrieved = repo.get_and_delete(&state).await.unwrap().unwrap();
    assert_eq!(
        retrieved.frontend_callback,
        Some(frontend_callback.to_string())
    );

    println!("✅ OAuth callback flow correctly stores and retrieves frontend_callback");
}

#[tokio::test]
async fn test_multiple_oauth_states_with_different_frontend_callbacks() {
    let (_server, db) = setup_test_server_with_database().await;
    let repo = OAuthStateRepository::new(db.pool().clone());

    let callback_url_1 = "https://app1.example.com/auth/callback";
    let callback_url_2 = "https://app2.example.com/oauth/complete";

    // Create first OAuth state
    let state1 = format!("test-state-{}", uuid::Uuid::new_v4());
    let created1 = repo
        .create(
            state1.clone(),
            "github".to_string(),
            None,
            Some(callback_url_1.to_string()),
        )
        .await
        .unwrap();

    assert_eq!(created1.frontend_callback, Some(callback_url_1.to_string()));

    // Create second OAuth state with different callback
    let state2 = format!("test-state-{}", uuid::Uuid::new_v4());
    let created2 = repo
        .create(
            state2.clone(),
            "google".to_string(),
            Some("test-pkce".to_string()),
            Some(callback_url_2.to_string()),
        )
        .await
        .unwrap();

    assert_eq!(created2.frontend_callback, Some(callback_url_2.to_string()));

    // Verify they are stored separately
    let retrieved1 = repo.get_and_delete(&state1).await.unwrap().unwrap();
    assert_eq!(
        retrieved1.frontend_callback,
        Some(callback_url_1.to_string())
    );
    assert_eq!(retrieved1.provider, "github");

    let retrieved2 = repo.get_and_delete(&state2).await.unwrap().unwrap();
    assert_eq!(
        retrieved2.frontend_callback,
        Some(callback_url_2.to_string())
    );
    assert_eq!(retrieved2.provider, "google");

    println!("✅ Multiple OAuth states with different frontend_callbacks are stored correctly");
}

#[tokio::test]
async fn test_frontend_callback_with_special_characters_in_path() {
    let (_server, db) = setup_test_server_with_database().await;
    let repo = OAuthStateRepository::new(db.pool().clone());

    // Frontend URL with special characters in path (no query parameters - those are rejected by validation)
    let callback_url = "https://app.example.com/auth/callback-success";

    let state = format!("test-state-{}", uuid::Uuid::new_v4());
    let created = repo
        .create(
            state.clone(),
            "github".to_string(),
            None,
            Some(callback_url.to_string()),
        )
        .await
        .unwrap();

    assert_eq!(created.frontend_callback, Some(callback_url.to_string()));

    // Verify URL is preserved exactly
    let retrieved = repo.get_and_delete(&state).await.unwrap().unwrap();
    assert_eq!(retrieved.frontend_callback, Some(callback_url.to_string()));

    println!("✅ Frontend callback URL with special characters is preserved correctly");
}

#[tokio::test]
async fn test_frontend_callback_url_can_store_any_valid_url() {
    // Repository stores any URL; validation happens at route layer
    let (_server, db) = setup_test_server_with_database().await;
    let repo = OAuthStateRepository::new(db.pool().clone());

    let callback_url_with_params = "https://app.example.com/callback?param=value";
    let state = format!("test-state-{}", uuid::Uuid::new_v4());

    let created = repo
        .create(
            state.clone(),
            "github".to_string(),
            None,
            Some(callback_url_with_params.to_string()),
        )
        .await
        .unwrap();

    assert_eq!(
        created.frontend_callback,
        Some(callback_url_with_params.to_string())
    );

    let retrieved = repo.get_and_delete(&state).await.unwrap().unwrap();
    assert_eq!(
        retrieved.frontend_callback,
        Some(callback_url_with_params.to_string())
    );

    println!(
        "✅ Repository layer can store various URL formats (validation happens at route layer)"
    );
}

#[tokio::test]
async fn test_oauth_state_replay_protection_with_frontend_callback() {
    let (_server, db) = setup_test_server_with_database().await;
    let repo = OAuthStateRepository::new(db.pool().clone());

    let state = format!("test-state-{}", uuid::Uuid::new_v4());
    let frontend_callback = Some("https://app.example.com/auth/success".to_string());

    // Create state with frontend_callback
    repo.create(
        state.clone(),
        "github".to_string(),
        None,
        frontend_callback.clone(),
    )
    .await
    .unwrap();

    // First get_and_delete should succeed
    let first = repo.get_and_delete(&state).await.unwrap();
    assert!(first.is_some());
    assert_eq!(first.unwrap().frontend_callback, frontend_callback);

    // Second get_and_delete should fail (state was deleted, replay protection)
    let second = repo.get_and_delete(&state).await.unwrap();
    assert!(second.is_none());

    println!("✅ Replay protection works correctly with frontend_callback");
}
