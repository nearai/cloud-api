// Simple OAuth2 Authentication Module
//
// Provides GitHub and Google OAuth2 authentication

pub mod types;
pub mod providers;

pub use types::{AuthError, User, AuthSession};
pub use providers::{OAuthManager, SessionStore};