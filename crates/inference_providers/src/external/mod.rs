//! External provider module for third-party AI providers
//!
//! This module provides a unified `ExternalProvider` that abstracts different
//! external AI providers (OpenAI, Anthropic, Gemini, etc.) behind a single
//! implementation of the `InferenceProvider` trait.
//!
//! # Architecture
//!
//! ```text
//! ExternalProvider (implements InferenceProvider)
//!     └── backends:
//!         ├── OpenAiCompatibleBackend (OpenAI, Azure, Together, Groq, etc.)
//!         ├── AnthropicBackend
//!         └── GeminiBackend
//! ```
//!
//! # Adding New Providers
//!
//! 1. **If OpenAI-compatible**: Register in database with:
//!    ```json
//!    {"backend": "openai_compatible", "base_url": "https://api.provider.com/v1"}
//!    ```
//!
//! 2. **If different API format**: Add new backend file implementing `ExternalBackend`

pub mod anthropic;
pub mod backend;
pub mod gemini;
pub mod openai_compatible;

use crate::{
    AttestationError, ChatCompletionParams, ChatCompletionResponseWithBytes, ChatSignature,
    CompletionError, CompletionParams, InferenceProvider, ListModelsError, ModelsResponse,
    StreamingResult,
};
use async_trait::async_trait;
use backend::{BackendConfig, ExternalBackend};
use serde::Deserialize;
use std::sync::Arc;

pub use anthropic::AnthropicBackend;
pub use backend::BackendConfig as ExternalBackendConfig;
pub use gemini::GeminiBackend;
pub use openai_compatible::OpenAiCompatibleBackend;

/// Provider configuration stored in database
///
/// This enum represents the JSON configuration stored in the `provider_config`
/// column of the `models` table.
#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "backend")]
pub enum ProviderConfig {
    /// OpenAI-compatible providers (OpenAI, Azure, Together, Groq, Fireworks, etc.)
    #[serde(rename = "openai_compatible")]
    OpenAiCompatible {
        /// Base URL for the API (e.g., "https://api.openai.com/v1")
        base_url: String,
        /// Optional organization ID for OpenAI
        #[serde(default)]
        organization_id: Option<String>,
    },

    /// Anthropic provider
    #[serde(rename = "anthropic")]
    Anthropic {
        /// Base URL for the API (e.g., "https://api.anthropic.com/v1")
        base_url: String,
        /// API version (defaults to "2023-06-01")
        #[serde(default = "default_anthropic_version")]
        version: String,
    },

    /// Google Gemini provider
    #[serde(rename = "gemini")]
    Gemini {
        /// Base URL for the API (e.g., "https://generativelanguage.googleapis.com/v1beta")
        base_url: String,
    },
}

fn default_anthropic_version() -> String {
    "2023-06-01".to_string()
}

/// Configuration for an external provider
#[derive(Debug, Clone)]
pub struct ExternalProviderConfig {
    /// Model name (used for routing)
    pub model_name: String,
    /// Provider configuration from database
    pub provider_config: ProviderConfig,
    /// API key for authentication
    pub api_key: String,
    /// Request timeout in seconds
    pub timeout_seconds: i64,
}

/// External provider facade
///
/// Implements `InferenceProvider` by delegating to the appropriate backend
/// based on the provider configuration.
pub struct ExternalProvider {
    backend: Arc<dyn ExternalBackend>,
    config: BackendConfig,
    model_name: String,
}

impl ExternalProvider {
    /// Create a new external provider with the given configuration
    pub fn new(external_config: ExternalProviderConfig) -> Self {
        let ExternalProviderConfig {
            model_name,
            provider_config,
            api_key,
            timeout_seconds,
        } = external_config;

        let (backend, config): (Arc<dyn ExternalBackend>, BackendConfig) = match provider_config {
            ProviderConfig::OpenAiCompatible {
                base_url,
                organization_id,
            } => {
                let mut extra = std::collections::HashMap::new();
                if let Some(org_id) = organization_id {
                    extra.insert("organization_id".to_string(), org_id);
                }

                (
                    Arc::new(OpenAiCompatibleBackend::new()),
                    BackendConfig {
                        base_url,
                        api_key,
                        timeout_seconds,
                        extra,
                    },
                )
            }
            ProviderConfig::Anthropic { base_url, version } => {
                let mut extra = std::collections::HashMap::new();
                extra.insert("version".to_string(), version);

                (
                    Arc::new(AnthropicBackend::new()),
                    BackendConfig {
                        base_url,
                        api_key,
                        timeout_seconds,
                        extra,
                    },
                )
            }
            ProviderConfig::Gemini { base_url } => (
                Arc::new(GeminiBackend::new()),
                BackendConfig {
                    base_url,
                    api_key,
                    timeout_seconds,
                    extra: std::collections::HashMap::new(),
                },
            ),
        };

        Self {
            backend,
            config,
            model_name,
        }
    }

    /// Get the backend type identifier
    pub fn backend_type(&self) -> &'static str {
        self.backend.backend_type()
    }

    /// Get the model name
    pub fn model_name(&self) -> &str {
        &self.model_name
    }
}

#[async_trait]
impl InferenceProvider for ExternalProvider {
    /// Lists models - external providers don't support dynamic model listing
    async fn models(&self) -> Result<ModelsResponse, ListModelsError> {
        // External providers don't support dynamic model discovery
        // Models are registered in the database
        Ok(ModelsResponse {
            object: "list".to_string(),
            data: vec![],
        })
    }

    /// Performs a streaming chat completion request
    async fn chat_completion_stream(
        &self,
        params: ChatCompletionParams,
        _request_hash: String,
    ) -> Result<StreamingResult, CompletionError> {
        self.backend
            .chat_completion_stream(&self.config, &self.model_name, params)
            .await
    }

    /// Performs a non-streaming chat completion request
    async fn chat_completion(
        &self,
        params: ChatCompletionParams,
        _request_hash: String,
    ) -> Result<ChatCompletionResponseWithBytes, CompletionError> {
        self.backend
            .chat_completion(&self.config, &self.model_name, params)
            .await
    }

    /// Performs a streaming text completion request
    ///
    /// Note: Most external providers don't support the legacy text completion API.
    /// This implementation returns an error for unsupported providers.
    async fn text_completion_stream(
        &self,
        _params: CompletionParams,
    ) -> Result<StreamingResult, CompletionError> {
        Err(CompletionError::CompletionError(
            "Text completion is not supported for external providers. Use chat completion instead."
                .to_string(),
        ))
    }

    /// Get signature - not supported for external providers
    async fn get_signature(
        &self,
        _chat_id: &str,
        _signing_algo: Option<String>,
    ) -> Result<ChatSignature, CompletionError> {
        Err(CompletionError::CompletionError(
            "Cryptographic signatures are not supported for external providers. \
             This feature requires TEE (Trusted Execution Environment) which is only \
             available on vLLM-based providers."
                .to_string(),
        ))
    }

    /// Get attestation report - not supported for external providers
    async fn get_attestation_report(
        &self,
        _model: String,
        _signing_algo: Option<String>,
        _nonce: Option<String>,
        _signing_address: Option<String>,
    ) -> Result<serde_json::Map<String, serde_json::Value>, AttestationError> {
        Err(AttestationError::FetchError(
            "TEE attestation is not supported for external providers. \
             External providers run outside of our Trusted Execution Environment."
                .to_string(),
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_provider_config_deserialization_openai() {
        let json = r#"{"backend": "openai_compatible", "base_url": "https://api.openai.com/v1"}"#;
        let config: ProviderConfig = serde_json::from_str(json).unwrap();

        match config {
            ProviderConfig::OpenAiCompatible {
                base_url,
                organization_id,
            } => {
                assert_eq!(base_url, "https://api.openai.com/v1");
                assert!(organization_id.is_none());
            }
            _ => panic!("Expected OpenAiCompatible variant"),
        }
    }

    #[test]
    fn test_provider_config_deserialization_openai_with_org() {
        let json = r#"{
            "backend": "openai_compatible",
            "base_url": "https://api.openai.com/v1",
            "organization_id": "org-123"
        }"#;
        let config: ProviderConfig = serde_json::from_str(json).unwrap();

        match config {
            ProviderConfig::OpenAiCompatible {
                base_url,
                organization_id,
            } => {
                assert_eq!(base_url, "https://api.openai.com/v1");
                assert_eq!(organization_id, Some("org-123".to_string()));
            }
            _ => panic!("Expected OpenAiCompatible variant"),
        }
    }

    #[test]
    fn test_provider_config_deserialization_anthropic() {
        let json = r#"{"backend": "anthropic", "base_url": "https://api.anthropic.com/v1"}"#;
        let config: ProviderConfig = serde_json::from_str(json).unwrap();

        match config {
            ProviderConfig::Anthropic { base_url, version } => {
                assert_eq!(base_url, "https://api.anthropic.com/v1");
                assert_eq!(version, "2023-06-01"); // Default version
            }
            _ => panic!("Expected Anthropic variant"),
        }
    }

    #[test]
    fn test_provider_config_deserialization_anthropic_with_version() {
        let json = r#"{
            "backend": "anthropic",
            "base_url": "https://api.anthropic.com/v1",
            "version": "2024-01-01"
        }"#;
        let config: ProviderConfig = serde_json::from_str(json).unwrap();

        match config {
            ProviderConfig::Anthropic { base_url, version } => {
                assert_eq!(base_url, "https://api.anthropic.com/v1");
                assert_eq!(version, "2024-01-01");
            }
            _ => panic!("Expected Anthropic variant"),
        }
    }

    #[test]
    fn test_provider_config_deserialization_gemini() {
        let json =
            r#"{"backend": "gemini", "base_url": "https://generativelanguage.googleapis.com/v1beta"}"#;
        let config: ProviderConfig = serde_json::from_str(json).unwrap();

        match config {
            ProviderConfig::Gemini { base_url } => {
                assert_eq!(
                    base_url,
                    "https://generativelanguage.googleapis.com/v1beta"
                );
            }
            _ => panic!("Expected Gemini variant"),
        }
    }
}
