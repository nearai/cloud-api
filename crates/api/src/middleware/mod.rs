// API Middleware
//
// This module contains custom middleware for the API layer,
// including authentication, authorization, and request processing.

pub mod auth;

// Re-export commonly used items
pub use auth::{
    auth_middleware, 
    optional_auth_middleware, 
    AuthenticatedUser,
};
