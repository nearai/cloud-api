// E2E tests for OAuth frontend_callback redirect logic

mod common;

use common::*;
use database::OAuthStateRepository;

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
