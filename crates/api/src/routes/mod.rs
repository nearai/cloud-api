pub mod api;
pub mod auth;
pub mod completions;
pub mod organizations;
pub mod organization_members;
pub mod users;

// Re-export completion endpoints for backward compatibility
pub use completions::{chat_completions, completions, models, quote};
