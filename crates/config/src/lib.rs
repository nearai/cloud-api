// Configuration Management
//
// This crate handles all configuration loading and management for the platform-api.
// It provides:
// - Configuration structs and deserialization
// - File loading logic
// - Default configuration values
//
// This keeps configuration concerns separate from domain logic.

use std::path::Path;
use thiserror::Error;

pub mod types;

// Re-export all configuration types
pub use types::*;

#[derive(Error, Debug)]
pub enum ConfigError {
    #[error("Configuration file not found. Tried paths: {paths}")]
    FileNotFound { paths: String },

    #[error("Failed to read configuration file: {source}")]
    IoError {
        #[from]
        source: std::io::Error,
    },

    #[error("Failed to parse configuration: {source}")]
    ParseError {
        #[from]
        source: serde_yaml::Error,
    },
}

/// Main configuration loading interface
impl ApiConfig {
    /// Load configuration from YAML file
    pub fn load_from_file<P: AsRef<Path>>(path: P) -> Result<Self, ConfigError> {
        let content = std::fs::read_to_string(path)?;
        let config: ApiConfig = serde_yaml::from_str(&content)?;
        Ok(config)
    }

    /// Load configuration from default locations
    pub fn load() -> Result<Self, ConfigError> {
        // Try different config locations in order
        let config_paths = ["config/config.yaml", "config.yaml", "config/default.yaml"];

        for path in &config_paths {
            if std::path::Path::new(path).exists() {
                return Self::load_from_file(path);
            }
        }

        // If no config file found, fail with descriptive error
        Err(ConfigError::FileNotFound {
            paths: config_paths.join(", "),
        })
    }
}
