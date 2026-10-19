// OAuth callback state and cross-provider account conflicts.

use crate::common::*;
use database::OAuthStateRepository;
use services::auth::{AuthError, AuthServiceTrait, OAuthUserInfo};
use std::sync::Arc;

fn oauth_info(provider: &str, email: &str) -> OAuthUserInfo {
    OAuthUserInfo {
        email_verified: true,
        email_authoritative: true,
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
    for (original_verified, verified) in [(false, true), (true, false), (true, true)] {
        let can_auto_link = original_verified && verified;
        for (original, incoming) in [("github", "google"), ("google", "github")] {
            let email = format!("oauth-auto-link-{}@example.test", uuid::Uuid::new_v4());
            let mut original_info = oauth_info(original, &email);
            original_info.email_verified = original_verified;
            let existing = auth
                .get_or_create_oauth_user(original_info.clone())
                .await
                .unwrap();
            let mut incoming_info = oauth_info(incoming, &email);
            incoming_info.email_verified = verified;
            for _ in 0..2 {
                let result = auth.get_or_create_oauth_user(incoming_info.clone()).await;
                if can_auto_link {
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
                can_auto_link.then_some(existing.id.0)
            );
            let stored = users.get_by_id(existing.id.0).await.unwrap().unwrap();
            assert_eq!(stored.auth_provider, original);
            assert_eq!(stored.provider_user_id, original_info.provider_user_id);
            assert_eq!(stored.email, email);
            assert_eq!(stored.display_name, existing.display_name);
            assert_eq!(stored.last_login_at.is_some(), can_auto_link);
            assert_single_oauth_account(&db, existing.id.0, &email).await;
            assert_eq!(
                auth.get_or_create_oauth_user(original_info.clone())
                    .await
                    .unwrap()
                    .id,
                existing.id
            );
            if !original_verified {
                // A verified original-provider login refreshes unknown/backfilled metadata.
                original_info.email_verified = true;
                auth.get_or_create_oauth_user(original_info).await.unwrap();
                assert_eq!(
                    auth.get_or_create_oauth_user(incoming_info)
                        .await
                        .unwrap()
                        .id,
                    existing.id
                );
                assert_single_oauth_account(&db, existing.id.0, &email).await;
            }
            if can_auto_link {
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
async fn oauth_auto_link_rejects_missing_or_stale_primary_email_evidence() {
    let (_server, db) = setup_test_server_with_database().await;
    let auth = real_auth(&db);
    let users = database::repositories::UserRepository::new(db.pool().clone());
    for (original, incoming) in [("github", "google"), ("google", "github")] {
        for mutation in [
            "DELETE FROM user_oauth_identities WHERE user_id=$1",
            "UPDATE user_oauth_identities SET email_authoritative=false WHERE user_id=$1",
            "UPDATE user_oauth_identities SET email=NULL WHERE user_id=$1",
            "UPDATE user_oauth_identities SET email='stale@example.test' WHERE user_id=$1",
            "UPDATE user_oauth_identities SET provider_user_id='stale-' || provider_user_id WHERE user_id=$1",
        ] {
            let email = format!("oauth-primary-proof-{}@example.test", uuid::Uuid::new_v4());
            let existing = auth.get_or_create_oauth_user(oauth_info(original, &email)).await.unwrap();
            db.pool().get().await.unwrap().execute(mutation, &[&existing.id.0]).await.unwrap();
            let incoming_info = oauth_info(incoming, &email);
            let result = auth.get_or_create_oauth_user(incoming_info.clone()).await;
            assert!(matches!(result, Err(AuthError::OAuthProviderConflict { .. })), "{mutation}");
            assert!(users.get_by_provider(incoming, &incoming_info.provider_user_id).await.unwrap().is_none());
            assert_single_oauth_account(&db, existing.id.0, &email).await;
        }
    }
}

#[tokio::test]
async fn oauth_original_google_authority_refreshes_and_clears_on_subject_login() {
    let (_server, db) = setup_test_server_with_database().await;
    let auth = real_auth(&db);
    let email = format!("oauth-google-proof-{}@example.test", uuid::Uuid::new_v4());
    let mut original = oauth_info("google", &email);
    let user = auth
        .get_or_create_oauth_user(original.clone())
        .await
        .unwrap();
    let incoming = oauth_info("github", &email);
    original.email_authoritative = false;
    // A returning subject still logs in, but cannot provide email proof for another provider.
    assert_eq!(
        auth.get_or_create_oauth_user(original.clone())
            .await
            .unwrap()
            .id,
        user.id
    );
    assert!(matches!(
        auth.get_or_create_oauth_user(incoming.clone()).await,
        Err(AuthError::OAuthProviderConflict { .. })
    ));
    let proof = db
        .pool()
        .get()
        .await
        .unwrap()
        .query_one(
            "SELECT email_verified,email_authoritative FROM user_oauth_identities WHERE user_id=$1",
            &[&user.id.0],
        )
        .await
        .unwrap();
    assert!(proof.get::<_, bool>("email_verified"));
    assert!(!proof.get::<_, bool>("email_authoritative"));
    original.email_authoritative = true;
    assert_eq!(
        auth.get_or_create_oauth_user(original).await.unwrap().id,
        user.id
    );
    assert_eq!(
        auth.get_or_create_oauth_user(incoming).await.unwrap().id,
        user.id
    );
    assert_single_oauth_account(&db, user.id.0, &email).await;
}

#[tokio::test]
async fn oauth_auto_link_rechecks_primary_verification_after_waiting_for_its_lock() {
    let (_server, db) = setup_test_server_with_database().await;
    let auth = real_auth(&db);
    for (original, incoming, field) in [
        ("github", "google", "email_verified"),
        ("google", "github", "email_verified"),
        ("github", "google", "email_authoritative"),
        ("google", "github", "email_authoritative"),
    ] {
        let email = format!("oauth-primary-lock-{}@example.test", uuid::Uuid::new_v4());
        let existing = auth
            .get_or_create_oauth_user(oauth_info(original, &email))
            .await
            .unwrap();
        let incoming_info = oauth_info(incoming, &email);
        let mut writer = db.pool().get().await.unwrap();
        let tx = writer.transaction().await.unwrap();
        let writer_pid: i32 = tx
            .query_one("SELECT pg_backend_pid()", &[])
            .await
            .unwrap()
            .get(0);
        tx.execute(
            &format!("UPDATE user_oauth_identities SET {field}=false WHERE user_id=$1"),
            &[&existing.id.0],
        )
        .await
        .unwrap();
        let login_auth = auth.clone();
        let login_info = incoming_info.clone();
        let login =
            tokio::spawn(async move { login_auth.get_or_create_oauth_user(login_info).await });
        let observer = db.pool().get().await.unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            loop {
                let waiting: bool = observer.query_one(
                    "SELECT EXISTS (SELECT 1 FROM pg_stat_activity WHERE $1=ANY(pg_blocking_pids(pid)))",
                    &[&writer_pid],
                ).await.unwrap().get(0);
                if waiting { break; }
                assert!(!login.is_finished(), "automatic linking must wait for primary verification");
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
        }).await.expect("automatic linking should block on the primary identity row");
        tx.commit().await.unwrap();
        assert!(matches!(
            login.await.unwrap(),
            Err(AuthError::OAuthProviderConflict { .. })
        ));
        assert!(database::UserRepository::new(db.pool().clone())
            .get_by_provider(incoming, &incoming_info.provider_user_id)
            .await
            .unwrap()
            .is_none());
        assert_single_oauth_account(&db, existing.id.0, &email).await;
    }
}

#[tokio::test]
async fn oauth_different_email_identities_remain_separate_accounts() {
    let (_server, db) = setup_test_server_with_database().await;
    let auth = real_auth(&db);
    for (original, incoming) in [("github", "google"), ("google", "github")] {
        let first = oauth_info(
            original,
            &format!("oauth-first-{}@example.test", uuid::Uuid::new_v4()),
        );
        let second = oauth_info(
            incoming,
            &format!("oauth-second-{}@example.test", uuid::Uuid::new_v4()),
        );
        let first_user = auth.get_or_create_oauth_user(first.clone()).await.unwrap();
        let second_user = auth.get_or_create_oauth_user(second.clone()).await.unwrap();
        assert_ne!(first_user.id, second_user.id);
        assert_eq!(
            auth.get_or_create_oauth_user(first.clone())
                .await
                .unwrap()
                .id,
            first_user.id
        );
        assert_eq!(
            auth.get_or_create_oauth_user(second.clone())
                .await
                .unwrap()
                .id,
            second_user.id
        );
        assert_single_oauth_account(&db, first_user.id.0, &first.email).await;
        assert_single_oauth_account(&db, second_user.id.0, &second.email).await;
    }
}

#[tokio::test]
async fn oauth_secondary_subject_login_preserves_primary_email_and_profile() {
    let (_server, db) = setup_test_server_with_database().await;
    let auth = real_auth(&db);
    for (original, incoming) in [("github", "google"), ("google", "github")] {
        let email = format!(
            "oauth-primary-profile-{}@example.test",
            uuid::Uuid::new_v4()
        );
        let original_info = oauth_info(original, &email);
        let user = auth
            .get_or_create_oauth_user(original_info.clone())
            .await
            .unwrap();
        let mut secondary = oauth_info(incoming, &email);
        assert_eq!(
            auth.get_or_create_oauth_user(secondary.clone())
                .await
                .unwrap()
                .id,
            user.id
        );
        secondary.email = format!(
            "oauth-secondary-changed-{}@example.test",
            uuid::Uuid::new_v4()
        );
        secondary.display_name = Some("Changed secondary profile".into());
        secondary.avatar_url = Some("https://example.test/secondary.png".into());
        secondary.email_authoritative = false;
        let returning = auth
            .get_or_create_oauth_user(secondary.clone())
            .await
            .unwrap();
        assert_eq!(returning.id, user.id);
        assert_eq!(returning.email, user.email);
        assert_eq!(returning.display_name, user.display_name);
        let stored = database::UserRepository::new(db.pool().clone())
            .get_by_id(user.id.0)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(stored.auth_provider, original);
        assert_eq!(stored.provider_user_id, original_info.provider_user_id);
        assert_eq!(stored.email, user.email);
        assert_eq!(stored.display_name, user.display_name);
        assert_eq!(stored.avatar_url, user.avatar_url);
        let proof = db.pool().get().await.unwrap().query_one(
            "SELECT email,email_authoritative FROM user_oauth_identities WHERE provider=$1 AND provider_user_id=$2",
            &[&incoming, &secondary.provider_user_id],
        ).await.unwrap();
        assert_eq!(proof.get::<_, String>("email"), secondary.email);
        assert!(!proof.get::<_, bool>("email_authoritative"));
        assert_single_oauth_account(&db, user.id.0, &email).await;
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
        auth.get_or_create_oauth_user(github.clone()),
        auth.get_or_create_oauth_user(google.clone()),
    );
    // Until the first callback records its primary email verification, the
    // concurrent callback must fail closed. A later login can then auto-link.
    let winner = match (github_result, google_result) {
        (Ok(first), Ok(second)) => {
            assert_eq!(first.id, second.id);
            first
        }
        (Ok(user), Err(AuthError::OAuthProviderConflict { .. })) => {
            assert_eq!(
                auth.get_or_create_oauth_user(google).await.unwrap().id,
                user.id
            );
            user
        }
        (Err(AuthError::OAuthProviderConflict { .. }), Ok(user)) => {
            assert_eq!(
                auth.get_or_create_oauth_user(github).await.unwrap().id,
                user.id
            );
            user
        }
        results => panic!("expected one account with verified identities, got {results:?}"),
    };
    assert_eq!(
        db.pool()
            .get()
            .await
            .unwrap()
            .query_one(
                "SELECT count(*) FROM user_oauth_identities WHERE user_id=$1",
                &[&winner.id.0]
            )
            .await
            .unwrap()
            .get::<_, i64>(0),
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
