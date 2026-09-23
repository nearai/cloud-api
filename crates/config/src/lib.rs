// Configuration Management
//
// This crate handles all configuration loading and management for the cloud-api.
// It provides:
// - Configuration structs
// - Environment variable loading
// - Default configuration values
//
// This keeps configuration concerns separate from domain logic.

use thiserror::Error;

pub mod admission_cache;
pub mod api;
pub mod ita;
pub mod types;

// Re-export all configuration types
pub use admission_cache::*;
pub use api::ApiConfig;
pub use ita::*;
pub use types::*;

#[derive(Error, Debug)]
pub enum ConfigError {
    #[error("Failed to load configuration from environment: {0}")]
    EnvError(String),
}

/// Main configuration loading interface
impl ApiConfig {
    /// Load configuration from environment variables
    ///
    /// This will attempt to load a .env file from the current directory first,
    /// then read all configuration from environment variables.
    pub fn load() -> Result<Self, ConfigError> {
        // Try to load .env file if it exists (don't error if it doesn't)
        let _ = dotenvy::dotenv();

        ApiConfig::from_env().map_err(ConfigError::EnvError)
    }
}
