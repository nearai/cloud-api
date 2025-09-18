// Simple Authentication Types for OAuth2
//
// This module defines simple OAuth2 authentication types
// for GitHub and Google authentication flows.

use serde::{Deserialize, Serialize};
use thiserror::Error;

#[derive(Error, Debug)]
pub enum AuthError {
    #[error("OAuth error: {0}")]
    OAuthError(String),
    
    #[error("Invalid state parameter")]
    InvalidState,
    
    #[error("Authentication failed: {0}")]
    AuthFailed(String),
    
    #[error("Configuration error: {0}")]
    ConfigError(String),
    
    #[error("Network error: {0}")]
    NetworkError(String),
    
    #[error("Session not found")]
    SessionNotFound,
}

/// Simple user structure containing only email
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct User {
    /// Unique user identifier (from OAuth provider)
    pub id: String,
    
    /// User's email address (the only thing we care about)
    pub email: String,
    
    /// OAuth provider (github or google)
    pub provider: String,
}

/// Authentication session
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AuthSession {
    /// Session ID
    pub session_id: String,
    
    /// Authenticated user
    pub user: User,
    
    /// Session creation timestamp
    pub created_at: i64,
    
    /// OAuth access token (stored for potential API calls)
    pub access_token: String,
}