// API Middleware
//
// This module contains custom middleware for the API layer,
// including authentication, authorization, and request processing.

pub mod auth;
pub mod usage;

// Re-export commonly used items
pub use auth::{admin_middleware, auth_middleware, AdminUser, AuthState, AuthenticatedUser};
pub use usage::{usage_check_middleware, UsageState};
