// E2E tests for Repository-level database operations
// These tests directly test repository behavior with the database
mod common;

use chrono::{Duration, Utc};
use database::{Database, OAuthStateRepository};

// Helper to get database pool for repository testing
async fn get_test_pool() -> database::pool::DbPool {
    let (_server, _inference_provider_pool, _mock_provider, database) =
        common::setup_test_server_with_pool().await;
    database.pool().clone()
}

// Helper to create database pool for repository testing
async fn create_test_pool() -> database::pool::DbPool {
    let config = config::DatabaseConfig {
        primary_app_id: "postgres-test".to_string(),
        gateway_subdomain: "cvm1.near.ai".to_string(),
        port: 5432,
        host: None,
        database: "platform_api".to_string(),
        username: std::env::var("DATABASE_USERNAME").unwrap_or("postgres".to_string()),
        password: std::env::var("DATABASE_PASSWORD").unwrap_or("postgres".to_string()),
        max_connections: 2,
        tls_enabled: false,
        tls_ca_cert_path: None,
        refresh_interval: 30,
        mock: false,
    };

    Database::from_config(&config).await.unwrap().pool().clone()
}

// ============================================
// OAuth State Repository Tests
// ============================================

#[tokio::test]
async fn test_create_and_get_oauth_state() {
    let pool = get_test_pool().await;
    let repo = OAuthStateRepository::new(pool.clone());

    let state = format!("test-state-{}", uuid::Uuid::new_v4());
    let provider = "github".to_string();

    // Create state
    let created = repo
        .create(state.clone(), provider.clone(), None)
        .await
        .unwrap();
    assert_eq!(created.state, state);
    assert_eq!(created.provider, provider);
    assert_eq!(created.pkce_verifier, None);

    // Get and delete state
    let retrieved = repo.get_and_delete(&state).await.unwrap();
    assert!(retrieved.is_some());
    let retrieved = retrieved.unwrap();
    assert_eq!(retrieved.state, state);
    assert_eq!(retrieved.provider, provider);

    // Second get should return None (state was deleted)
    let second_get = repo.get_and_delete(&state).await.unwrap();
    assert!(second_get.is_none());
}

#[tokio::test]
async fn test_expired_state_not_returned() {
    let pool = get_test_pool().await;
    let repo = OAuthStateRepository::new(pool.clone());

    let state = format!("test-state-{}", uuid::Uuid::new_v4());

    // Create state with past expiration
    let client = pool.get().await.unwrap();
    let past_time = Utc::now() - Duration::minutes(1);
    client
        .execute(
            r#"
            INSERT INTO oauth_states (state, provider, pkce_verifier, created_at, expires_at)
            VALUES ($1, $2, $3, $4, $5)
            "#,
            &[&state, &"github", &None::<String>, &past_time, &past_time],
        )
        .await
        .unwrap();

    // Try to get expired state
    let result = repo.get_and_delete(&state).await.unwrap();
    assert!(result.is_none());
}

#[tokio::test]
async fn test_google_with_pkce_verifier() {
    let pool = get_test_pool().await;
    let repo = OAuthStateRepository::new(pool.clone());

    let state = format!("test-state-{}", uuid::Uuid::new_v4());
    let provider = "google".to_string();
    let verifier = Some("test-pkce-verifier".to_string());

    // Create state with PKCE verifier
    let created = repo
        .create(state.clone(), provider.clone(), verifier.clone())
        .await
        .unwrap();
    assert_eq!(created.pkce_verifier, verifier);

    // Get and verify PKCE verifier is preserved
    let retrieved = repo.get_and_delete(&state).await.unwrap().unwrap();
    assert_eq!(retrieved.pkce_verifier, verifier);
}

#[tokio::test]
async fn test_state_replay_protection() {
    let pool = get_test_pool().await;
    let repo = OAuthStateRepository::new(pool.clone());

    let state = format!("test-state-{}", uuid::Uuid::new_v4());
    let provider = "github".to_string();

    // Create one state
    repo.create(state.clone(), provider, None).await.unwrap();

    // First get should succeed
    let first = repo.get_and_delete(&state).await.unwrap();
    assert!(first.is_some());

    // Second get should fail (replay protection)
    let second = repo.get_and_delete(&state).await.unwrap();
    assert!(second.is_none());
}
