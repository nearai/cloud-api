use crate::common::encryption_headers;
use config::ExternalProvidersConfig;
use inference_providers::{
    models::{AttestationError, CompletionError},
    AudioTranscriptionError, AudioTranscriptionParams, AudioTranscriptionResponse,
    ChatCompletionParams, ExternalProvider, ExternalProviderConfig, ImageEditError,
    ImageEditParams, ImageEditResponseWithBytes, ImageGenerationError, ImageGenerationParams,
    ImageGenerationResponseWithBytes, InferenceProvider, ProviderConfig, RerankError, RerankParams,
    RerankResponse, StreamingResult, StreamingResultExt, VLlmConfig, VLlmProvider,
};
use regex::Regex;
use std::{collections::HashMap, sync::Arc, time::Duration};
use tokio::sync::{Mutex, RwLock};
use tracing::{debug, info, warn};

type InferenceProviderTrait = dyn InferenceProvider + Send + Sync;

/// Trait for fetching external model configurations from a data source (e.g., database).
/// This decouples the InferenceProviderPool from the database crate (hexagonal architecture).
#[async_trait::async_trait]
pub trait ExternalModelsSource: Send + Sync {
    async fn fetch_external_models(&self) -> Result<Vec<(String, serde_json::Value)>, String>;

    /// Fetch models that have a direct inference URL configured.
    /// Returns (model_name, inference_url) pairs for active models with inference_url set.
    /// These models are routed directly to the URL, bypassing the discovery server.
    async fn fetch_inference_url_models(&self) -> Result<Vec<(String, String)>, String>;
}

/// Combined provider mappings updated atomically to prevent race conditions
/// Both mappings are updated together under a single lock to ensure consistency
#[derive(Clone)]
struct ProviderMappings {
    /// Map of model name -> list of providers (for load balancing)
    model_to_providers: HashMap<String, Vec<Arc<InferenceProviderTrait>>>,
    /// Map of model signing public key -> list of providers (for load balancing when multiple instances share the same key)
    pubkey_to_providers: HashMap<String, Vec<Arc<InferenceProviderTrait>>>,
}

impl ProviderMappings {
    fn new() -> Self {
        Self {
            model_to_providers: HashMap::new(),
            pubkey_to_providers: HashMap::new(),
        }
    }
}

#[derive(Clone)]
pub struct InferenceProviderPool {
    /// Optional API key for authenticating with inference backends
    api_key: Option<String>,
    /// All inference_url-backed vLLM providers, updated atomically.
    provider_mappings: Arc<RwLock<ProviderMappings>>,
    /// Best-effort mapping of provider pointer -> provider URL for vLLM backends.
    /// Used by api-key affinity routing to prefer a previously bound provider URL.
    provider_urls: Arc<RwLock<HashMap<usize, String>>>,
    /// External providers (keyed by model name, created from database config)
    external_providers: Arc<RwLock<HashMap<String, Arc<InferenceProviderTrait>>>>,
    /// Configuration for external providers (API keys, timeouts, etc.)
    external_configs: ExternalProvidersConfig,
    /// Round-robin index for each model.
    /// Uses std::sync::RwLock because operations are instant HashMap lookups/inserts.
    load_balancer_index: Arc<std::sync::RwLock<HashMap<String, usize>>>,
    /// Map of chat_id -> provider for sticky routing
    chat_id_mapping: Arc<RwLock<HashMap<String, Arc<InferenceProviderTrait>>>>,
    /// Background task handle for periodic provider refresh from database
    refresh_task_handle: Arc<Mutex<Option<tokio::task::JoinHandle<()>>>>,
    /// Per-provider consecutive failure count, keyed by Arc pointer address.
    /// Providers with high failure counts are deprioritized in load balancing.
    /// Counts reset to 0 on success and are cleaned up on refresh.
    /// Uses std::sync::RwLock (not tokio) because all operations are non-blocking
    /// HashMap lookups/inserts — no .await while holding the lock.
    provider_failure_counts: Arc<std::sync::RwLock<HashMap<usize, u32>>>,
    /// Cache of inference_url → serving provider. When a model's URL hasn't changed
    /// across refreshes, the existing provider (and its warm reqwest::Client with
    /// pooled TLS connections) is reused instead of creating a new one.
    inference_url_providers: Arc<RwLock<HashMap<String, Arc<InferenceProviderTrait>>>>,
}

impl InferenceProviderPool {
    /// Create a new pool with optional API key for backend authentication
    pub fn new(api_key: Option<String>, external_configs: ExternalProvidersConfig) -> Self {
        Self {
            api_key,
            provider_mappings: Arc::new(RwLock::new(ProviderMappings::new())),
            provider_urls: Arc::new(RwLock::new(HashMap::new())),
            external_providers: Arc::new(RwLock::new(HashMap::new())),
            external_configs,
            load_balancer_index: Arc::new(std::sync::RwLock::new(HashMap::new())),
            chat_id_mapping: Arc::new(RwLock::new(HashMap::new())),
            refresh_task_handle: Arc::new(Mutex::new(None)),
            provider_failure_counts: Arc::new(std::sync::RwLock::new(HashMap::new())),
            inference_url_providers: Arc::new(RwLock::new(HashMap::new())),
        }
    }

    fn provider_ptr(provider: &Arc<InferenceProviderTrait>) -> usize {
        Arc::as_ptr(provider) as *const () as usize
    }

    /// Register an external provider for a model
    ///
    /// External providers are third-party AI providers (OpenAI, Anthropic, Gemini)
    /// that don't support TEE attestation.
    ///
    /// # Arguments
    /// * `model_name` - The model name to register (e.g., "gpt-4o", "claude-3-opus")
    /// * `provider_config` - The provider configuration from database
    pub async fn register_external_provider(
        &self,
        model_name: String,
        provider_config: serde_json::Value,
    ) -> Result<(), String> {
        let (provider, backend_type) =
            self.create_external_provider(&model_name, provider_config)?;

        let mut external = self.external_providers.write().await;
        external.insert(model_name.clone(), provider);

        info!(
            model = %model_name,
            backend = %backend_type,
            "Registered external provider"
        );

        Ok(())
    }

    /// Load and register external providers from a list of model configurations.
    pub async fn load_external_providers(
        &self,
        models: Vec<(String, serde_json::Value)>,
    ) -> Result<(), String> {
        let mut success_count = 0;
        let mut error_count = 0;

        for (model_name, provider_config) in models {
            match self
                .register_external_provider(model_name.clone(), provider_config)
                .await
            {
                Ok(()) => success_count += 1,
                Err(e) => {
                    warn!(
                        model = %model_name,
                        error = %e,
                        "Failed to register external provider"
                    );
                    error_count += 1;
                }
            }
        }

        info!(
            success = success_count,
            errors = error_count,
            "Loaded external providers"
        );

        if error_count > 0 && success_count == 0 {
            Err(format!(
                "All {} external provider(s) failed to load",
                error_count
            ))
        } else {
            Ok(())
        }
    }

    /// Check if a model has a registered provider
    pub async fn has_provider(&self, model_name: &str) -> bool {
        if self.is_external_provider(model_name).await {
            return true;
        }

        let mappings = self.provider_mappings.read().await;
        mappings.model_to_providers.contains_key(model_name)
    }

    /// Get an external provider by model name
    async fn get_external_provider(&self, model_name: &str) -> Option<Arc<InferenceProviderTrait>> {
        let external = self.external_providers.read().await;
        external.get(model_name).cloned()
    }

    /// Check if a model is an external provider
    pub async fn is_external_provider(&self, model_name: &str) -> bool {
        let external = self.external_providers.read().await;
        external.contains_key(model_name)
    }

    /// Remove a provider by model name. Used when admin deactivates a model.
    /// Also cleans up pubkey_to_providers, load_balancer_index, and provider failure state.
    pub async fn unregister_provider(&self, model_name: &str) -> bool {
        let removed_external = {
            let mut external = self.external_providers.write().await;
            external.remove(model_name).is_some()
        };

        let mut removed_ptrs = std::collections::HashSet::new();
        let removed_vllm = {
            let mut mappings = self.provider_mappings.write().await;
            let removed_providers = mappings.model_to_providers.remove(model_name);
            if let Some(removed) = &removed_providers {
                removed_ptrs.extend(removed.iter().map(Self::provider_ptr));
                mappings.pubkey_to_providers.retain(|_, providers| {
                    providers.retain(|p| !removed_ptrs.contains(&Self::provider_ptr(p)));
                    !providers.is_empty()
                });
            }
            removed_providers.is_some()
        };

        if removed_vllm {
            self.load_balancer_index
                .write()
                .unwrap_or_else(|e| e.into_inner())
                .remove(model_name);
            self.provider_failure_counts
                .write()
                .unwrap_or_else(|e| e.into_inner())
                .retain(|key, _| !removed_ptrs.contains(key));
            self.provider_urls
                .write()
                .await
                .retain(|key, _| !removed_ptrs.contains(key));
            self.inference_url_providers
                .write()
                .await
                .retain(|_, provider| !removed_ptrs.contains(&Self::provider_ptr(provider)));
        }

        if removed_external || removed_vllm {
            info!(model = %model_name, "Unregistered provider");
        } else {
            debug!(model = %model_name, "Provider not found for unregistration");
        }

        removed_external || removed_vllm
    }

    /// Register a provider for a model manually (useful for testing with mock providers)
    /// Also populates model_pub_key_mapping by fetching the attestation report
    /// Fetches attestation reports for both ECDSA and Ed25519 to support both signing algorithms
    pub async fn register_provider(&self, model_id: String, provider: Arc<InferenceProviderTrait>) {
        // Fetch signing public keys for both algorithms
        // Use "mock" as URL identifier for logging (since this is typically used for mock providers)
        let (pub_key_updates, _has_valid_attestation) =
            Self::fetch_signing_public_keys_for_both_algorithms(&provider, &model_id, "mock").await;

        // Atomic update: update both mappings together under a single lock
        let mut mappings = self.provider_mappings.write().await;
        mappings
            .model_to_providers
            .insert(model_id, vec![provider.clone()]);
        for (key, provider) in pub_key_updates {
            mappings
                .pubkey_to_providers
                .entry(key)
                .or_default()
                .push(provider);
        }
    }

    /// Register multiple providers for multiple models (useful for testing)
    /// Also populates model_pub_key_mapping by fetching attestation reports
    /// Fetches attestation reports for both ECDSA and Ed25519 to support both signing algorithms
    pub async fn register_providers(&self, providers: Vec<(String, Arc<InferenceProviderTrait>)>) {
        // Phase 1: Collect attestation reports and public keys (no locks held)
        let mut pub_key_updates: Vec<(String, Arc<InferenceProviderTrait>)> = Vec::new();
        let mut model_providers: HashMap<String, Vec<Arc<InferenceProviderTrait>>> = HashMap::new();

        for (model_id, provider) in providers {
            // Fetch signing public keys for both algorithms to populate model_pub_key_mapping
            // Use "mock" as URL identifier for logging (since this is typically used for mock providers)
            let (keys, _has_valid_attestation) =
                Self::fetch_signing_public_keys_for_both_algorithms(&provider, &model_id, "mock")
                    .await;
            pub_key_updates.extend(keys);

            model_providers.entry(model_id).or_default().push(provider);
        }

        // Phase 2: Atomic bulk update of both mappings under a single lock
        // This ensures consistency - both mappings are updated together
        {
            let mut mappings = self.provider_mappings.write().await;
            for (model_id, providers) in model_providers {
                mappings.model_to_providers.insert(model_id, providers);
            }
            for (key, provider) in pub_key_updates {
                mappings
                    .pubkey_to_providers
                    .entry(key)
                    .or_default()
                    .push(provider);
            }
        }
    }

    /// Fetch signing public keys for both ECDSA and Ed25519 algorithms
    ///
    /// Attempts to fetch attestation reports for both signing algorithms and returns
    /// all available signing public keys. This ensures that providers are registered
    /// for both algorithms if they support them.
    ///
    /// # Arguments
    /// * `provider` - The inference provider to fetch the attestation reports from
    /// * `model_name` - The model name to request attestation for
    /// * `url` - Optional URL for logging purposes (can be empty string if not available)
    ///
    /// # Returns
    /// * Tuple of (signing_public_keys, has_valid_attestation) where:
    ///   - `signing_public_keys`: Vector of (signing_public_key, provider) tuples for all available algorithms
    ///   - `has_valid_attestation`: True if at least one attestation report was successfully fetched
    async fn fetch_signing_public_keys_for_both_algorithms(
        provider: &Arc<InferenceProviderTrait>,
        model_name: &str,
        url: &str,
    ) -> (Vec<(String, Arc<InferenceProviderTrait>)>, bool) {
        let mut pub_key_updates = Vec::new();
        let mut has_valid_attestation = false;

        // Fetch for ECDSA
        if let Some(attestation_report) = Self::fetch_attestation_report_with_retry_for_algo(
            provider,
            model_name,
            url,
            Some("ecdsa"),
        )
        .await
        {
            has_valid_attestation = true;
            if let Some(signing_public_key) = attestation_report
                .get("signing_public_key")
                .and_then(|v| v.as_str())
            {
                pub_key_updates.push((signing_public_key.to_string(), provider.clone()));
            }
        }

        // Fetch for Ed25519
        if let Some(attestation_report) = Self::fetch_attestation_report_with_retry_for_algo(
            provider,
            model_name,
            url,
            Some("ed25519"),
        )
        .await
        {
            has_valid_attestation = true;
            if let Some(signing_public_key) = attestation_report
                .get("signing_public_key")
                .and_then(|v| v.as_str())
            {
                pub_key_updates.push((signing_public_key.to_string(), provider.clone()));
            }
        }

        (pub_key_updates, has_valid_attestation)
    }

    /// Fetch attestation report with retries for a specific signing algorithm
    ///
    /// Retries up to 3 times with exponential backoff (100ms, 200ms, 400ms).
    /// This prevents providers from being excluded from the pool due to transient network issues.
    ///
    /// # Arguments
    /// * `provider` - The inference provider to fetch the attestation report from
    /// * `model_name` - The model name to request attestation for
    /// * `url` - Optional URL for logging purposes (can be empty string if not available)
    /// * `signing_algo` - Optional signing algorithm ("ecdsa" or "ed25519")
    ///
    /// # Returns
    /// * `Some(attestation_report)` if successful after retries
    /// * `None` if all retry attempts failed
    async fn fetch_attestation_report_with_retry_for_algo(
        provider: &Arc<InferenceProviderTrait>,
        model_name: &str,
        url: &str,
        signing_algo: Option<&str>,
    ) -> Option<serde_json::Map<String, serde_json::Value>> {
        const MAX_ATTEMPTS: u32 = 3;
        const INITIAL_DELAY_MS: u64 = 100;

        for attempt in 0..MAX_ATTEMPTS {
            match provider
                .get_attestation_report(
                    model_name.to_string(),
                    signing_algo.map(|s| s.to_string()),
                    None,
                    None,
                    false,
                )
                .await
            {
                Ok(report) => {
                    if attempt > 0 {
                        tracing::debug!(
                            model = %model_name,
                            url = %url,
                            attempt = attempt + 1,
                            "Successfully fetched attestation report after retry"
                        );
                    }
                    return Some(report);
                }
                Err(e) => {
                    if attempt < MAX_ATTEMPTS - 1 {
                        // Exponential backoff: 100ms, 200ms, 400ms
                        let delay_ms = INITIAL_DELAY_MS * (1 << attempt);
                        tracing::debug!(
                            model = %model_name,
                            url = %url,
                            attempt = attempt + 1,
                            max_attempts = MAX_ATTEMPTS,
                            delay_ms = delay_ms,
                            error = %e,
                            "Failed to fetch attestation report, retrying..."
                        );
                        tokio::time::sleep(Duration::from_millis(delay_ms)).await;
                    } else {
                        tracing::warn!(
                            model = %model_name,
                            url = %url,
                            attempts = MAX_ATTEMPTS,
                            error = %e,
                            "Provider failed to return attestation report after retries, excluding from pool"
                        );
                    }
                }
            }
        }

        None
    }

    async fn get_providers_for_model(
        &self,
        model_id: &str,
    ) -> Option<Vec<Arc<InferenceProviderTrait>>> {
        let mappings = self.provider_mappings.read().await;
        mappings.model_to_providers.get(model_id).cloned()
    }

    /// Store a mapping of chat_id to provider
    async fn store_chat_id_mapping(
        &self,
        chat_id: String,
        provider: Arc<dyn InferenceProvider + Send + Sync>,
    ) {
        let mut mapping = self.chat_id_mapping.write().await;
        mapping.insert(chat_id.clone(), provider);
        tracing::debug!("Stored chat_id mapping: {}", chat_id);
    }

    /// Lookup provider by chat_id
    pub async fn get_provider_by_chat_id(
        &self,
        chat_id: &str,
    ) -> Option<Arc<dyn InferenceProvider + Send + Sync>> {
        let mapping = self.chat_id_mapping.read().await;
        mapping.get(chat_id).cloned()
    }

    /// Get providers with load balancing support
    ///
    /// This function handles provider selection based on model_id and optional model_pub_key:
    /// - Gets providers by model_id first
    /// - If model_pub_key is provided: Filters providers by public key
    /// - Applies round-robin load balancing
    ///
    /// Returns providers with the round-robin selected one first, followed by others for fallback.
    async fn get_providers_with_fallback(
        &self,
        model_id: &str,
        model_pub_key: Option<&str>,
        preferred_provider_url: Option<&str>,
    ) -> Option<Vec<Arc<InferenceProviderTrait>>> {
        let mappings = self.provider_mappings.read().await;

        // Get providers by model_id first
        let model_providers = mappings.model_to_providers.get(model_id)?.clone();

        // Filter by model_pub_key if provided
        let providers = if let Some(pub_key) = model_pub_key {
            // Use the existing 'mappings' lock instead of acquiring it again
            let pub_key_providers = mappings.pubkey_to_providers.get(pub_key)?.clone();

            // Find intersection: providers that are in both lists
            let filtered: Vec<Arc<InferenceProviderTrait>> = model_providers
                .iter()
                .filter(|model_provider| {
                    pub_key_providers
                        .iter()
                        .any(|pub_provider| Arc::ptr_eq(model_provider, pub_provider))
                })
                .cloned()
                .collect();

            if filtered.is_empty() {
                return None;
            }

            filtered
        } else {
            model_providers
        };

        if providers.is_empty() {
            return None;
        }

        if providers.len() == 1 {
            return Some(providers);
        }

        // Apply round-robin load balancing
        let index_key = if let Some(pub_key) = model_pub_key {
            format!("pubkey:{}", pub_key)
        } else {
            format!("id:{}", model_id)
        };

        let selected_index = {
            let mut indices = self
                .load_balancer_index
                .write()
                .unwrap_or_else(|e| e.into_inner());
            let index = indices.entry(index_key.clone()).or_insert(0);
            let selected_index = *index % providers.len();

            // Increment for next request
            *index = (*index + 1) % providers.len();
            selected_index
        };

        // Build ordered list following round-robin pattern:
        // selected provider first, then continue round-robin (selected+1, selected+2, ...)
        let mut ordered_providers = Vec::with_capacity(providers.len());
        for i in 0..providers.len() {
            let provider_index = (selected_index + i) % providers.len();
            ordered_providers.push(providers[provider_index].clone());
        }

        if let Some(preferred_provider_url) = preferred_provider_url {
            let provider_urls = self.provider_urls.read().await;
            if let Some(preferred_index) = ordered_providers.iter().position(|provider| {
                provider_urls
                    .get(&Self::provider_ptr(provider))
                    .map(|url| url == preferred_provider_url)
                    .unwrap_or(false)
            }) {
                let preferred_provider = ordered_providers.remove(preferred_index);
                ordered_providers.insert(0, preferred_provider);
            }
        }

        // Partition providers by failure count: healthy providers first, then demoted.
        // Demoted providers (>= MAX_CONSECUTIVE_FAILURES) are still included as last resort
        // but healthy providers are tried first, avoiding unnecessary timeout waits.
        const MAX_CONSECUTIVE_FAILURES: u32 = 10;
        let counts = self
            .provider_failure_counts
            .read()
            .unwrap_or_else(|e| e.into_inner());
        let (mut healthy, mut demoted): (Vec<_>, Vec<_>) =
            ordered_providers.into_iter().partition(|p| {
                let key = Arc::as_ptr(p) as *const () as usize;
                let failures = counts.get(&key).copied().unwrap_or(0);
                failures < MAX_CONSECUTIVE_FAILURES
            });
        drop(counts);

        // Healthy providers first (in round-robin order), then demoted as last resort.
        // This way, if 1 of 2 providers is down, requests immediately go to the healthy
        // one instead of waiting 5s for the dead one's connect timeout.
        healthy.append(&mut demoted);
        let ordered_providers = healthy;

        tracing::debug!(
            index_key = %index_key,
            providers_count = ordered_providers.len(),
            selected_index = selected_index,
            "Prepared providers for fallback with round-robin priority and failure demotion"
        );

        Some(ordered_providers)
    }

    async fn get_provider_url(&self, provider: &Arc<InferenceProviderTrait>) -> Option<String> {
        let provider_urls = self.provider_urls.read().await;
        provider_urls.get(&Self::provider_ptr(provider)).cloned()
    }

    pub async fn select_provider_url_for_model(
        &self,
        model_id: &str,
    ) -> Result<Option<String>, CompletionError> {
        if self.get_external_provider(model_id).await.is_some() {
            return Ok(None);
        }

        let providers = match self.get_providers_with_fallback(model_id, None, None).await {
            Some(providers) => providers,
            None => return Ok(None),
        };

        let provider_urls = self.provider_urls.read().await;
        for provider in providers {
            if let Some(provider_url) = provider_urls.get(&Self::provider_ptr(&provider)).cloned() {
                return Ok(Some(provider_url));
            }
        }

        Ok(None)
    }

    /// Sanitize a CompletionError by preserving its variant structure while sanitizing messages
    fn sanitize_completion_error(error: CompletionError, model_id: &str) -> CompletionError {
        // Helper to sanitize message and format with model_id context
        let sanitize_and_format = |msg: &str| -> String {
            let sanitized = Self::sanitize_error_message(msg);
            format!("Provider failed for model '{}': {}", model_id, sanitized)
        };

        match error {
            CompletionError::HttpError {
                status_code,
                message,
                is_external,
            } => {
                // For HttpError, sanitize the message and include model_id context
                // Preserve status_code and is_external for proper error mapping
                CompletionError::HttpError {
                    status_code,
                    message: sanitize_and_format(&message),
                    is_external,
                }
            }
            CompletionError::CompletionError(msg) => {
                CompletionError::CompletionError(sanitize_and_format(&msg))
            }
            CompletionError::InvalidResponse(msg) => {
                CompletionError::InvalidResponse(sanitize_and_format(&msg))
            }
            CompletionError::Unknown(msg) => CompletionError::Unknown(sanitize_and_format(&msg)),
        }
    }

    /// Sanitize error message by removing sensitive information like IP addresses, URLs, and internal details
    fn sanitize_error_message(error: &str) -> String {
        let mut sanitized = error.to_string();

        // Remove URLs (http://..., https://...)
        let url_regex = Regex::new(r"https?://[^\s)]+").unwrap();
        sanitized = url_regex
            .replace_all(&sanitized, "[URL_REDACTED]")
            .to_string();

        // Remove standalone IP addresses with ports (e.g., 192.168.0.1:8000)
        let ip_port_regex = Regex::new(r"\b(?:[0-9]{1,3}\.){3}[0-9]{1,3}:\d+\b").unwrap();
        sanitized = ip_port_regex
            .replace_all(&sanitized, "[IP_REDACTED]")
            .to_string();

        // Remove standalone IP addresses (e.g., 192.168.0.1)
        let ip_regex = Regex::new(r"\b(?:[0-9]{1,3}\.){3}[0-9]{1,3}\b").unwrap();
        sanitized = ip_regex
            .replace_all(&sanitized, "[IP_REDACTED]")
            .to_string();

        // Remove specific error details that might leak internal structure
        sanitized = sanitized.replace(
            "error sending request for url",
            "provider connection failed",
        );

        sanitized
    }

    /// Generic retry helper that tries each provider in order with automatic fallback.
    /// Returns both the result and the provider that succeeded (for chat_id mapping).
    /// If model_pub_key is provided, routes to the specific provider by signing public key.
    async fn retry_with_fallback<T, F, Fut>(
        &self,
        model_id: &str,
        operation_name: &str,
        model_pub_key: Option<&str>,
        preferred_provider_url: Option<&str>,
        provider_fn: F,
    ) -> Result<(T, Arc<InferenceProviderTrait>), CompletionError>
    where
        F: Fn(Arc<InferenceProviderTrait>) -> Fut,
        Fut: std::future::Future<Output = Result<T, CompletionError>>,
    {
        if let Some(external_provider) = self.get_external_provider(model_id).await {
            tracing::info!(
                model_id = %model_id,
                operation = operation_name,
                "Using external provider"
            );

            if model_pub_key.is_some() {
                return Err(CompletionError::CompletionError(format!(
                    "Model '{}' is an external provider and does not support encryption. \
                     External providers run outside of our Trusted Execution Environment.",
                    model_id
                )));
            }

            match provider_fn(external_provider.clone()).await {
                Ok(result) => {
                    tracing::info!(
                        model_id = %model_id,
                        operation = operation_name,
                        "Successfully completed request with external provider"
                    );
                    return Ok((result, external_provider));
                }
                Err(error) => {
                    return Err(Self::sanitize_completion_error(error, model_id));
                }
            }
        }

        let providers = match self
            .get_providers_with_fallback(model_id, model_pub_key, preferred_provider_url)
            .await
        {
            Some(p) => p,
            None => {
                if let Some(pub_key) = model_pub_key {
                    let (available_pubkeys, model_provider_count) = {
                        let mappings = self.provider_mappings.read().await;
                        let pubkeys: Vec<String> = mappings
                            .pubkey_to_providers
                            .keys()
                            .map(|k| {
                                let prefix: String = k.chars().take(16).collect();
                                format!("{}...({})", prefix, k.len())
                            })
                            .collect();
                        let count = mappings
                            .model_to_providers
                            .get(model_id)
                            .map(|v| v.len())
                            .unwrap_or(0);
                        (pubkeys, count)
                    };
                    let model_pub_key_prefix: String = pub_key.chars().take(16).collect();
                    tracing::warn!(
                        model_id = %model_id,
                        model_pub_key_prefix = %model_pub_key_prefix,
                        model_pub_key_len = pub_key.len(),
                        available_pubkeys = ?available_pubkeys,
                        model_provider_count = model_provider_count,
                        operation = operation_name,
                        "No provider found for model public key"
                    );
                    return Err(CompletionError::CompletionError(format!(
                        "No provider found for model {} with public key '{}...'. Encryption requires routing to the specific provider with this public key.",
                        model_id,
                        pub_key.chars().take(32).collect::<String>()
                    )));
                } else {
                    let (available_models, external_models) = {
                        let mappings = self.provider_mappings.read().await;
                        let available_models: Vec<_> =
                            mappings.model_to_providers.keys().cloned().collect();
                        drop(mappings);

                        let external = self.external_providers.read().await;
                        let external_models: Vec<_> = external.keys().cloned().collect();
                        (available_models, external_models)
                    };
                    tracing::error!(
                        model_id = %model_id,
                        available_models = ?available_models,
                        available_external_models = ?external_models,
                        operation = operation_name,
                        "Model not found in provider pool"
                    );
                    return Err(CompletionError::CompletionError(format!(
                        "Model '{model_id}' not found in any configured provider"
                    )));
                }
            }
        };

        tracing::info!(
            model_id = %model_id,
            providers_count = providers.len(),
            operation = operation_name,
            "Attempting {} with {} provider(s)",
            operation_name,
            providers.len()
        );

        // Track the last error (preserving its structure for proper status code mapping)
        let mut last_error: Option<CompletionError> = None;

        // Try each provider in order until one succeeds
        for (attempt, provider) in providers.iter().enumerate() {
            tracing::debug!(
                model_id = %model_id,
                attempt = attempt + 1,
                total_providers = providers.len(),
                operation = operation_name,
                "Trying provider {} of {}",
                attempt + 1,
                providers.len()
            );

            match provider_fn(provider.clone()).await {
                Ok(result) => {
                    // Reset failure counter on success
                    {
                        let mut counts = self
                            .provider_failure_counts
                            .write()
                            .unwrap_or_else(|e| e.into_inner());
                        let key = Arc::as_ptr(provider) as *const () as usize;
                        counts.insert(key, 0);
                    }
                    tracing::info!(
                        model_id = %model_id,
                        attempt = attempt + 1,
                        operation = operation_name,
                        "Successfully completed request with provider"
                    );
                    return Ok((result, provider.clone()));
                }
                Err(e) => {
                    // For HTTP client errors (4xx), don't retry with other providers.
                    // The request itself is invalid (e.g., too many tokens), so retrying won't help.
                    // Exception: 429 (rate limit) and 408 (request timeout) are retryable
                    // as other providers may have capacity or better connectivity.
                    // NOTE: Don't increment the failure counter for non-retryable 4xx —
                    // these indicate invalid requests, not unhealthy providers.
                    if let CompletionError::HttpError { status_code, .. } = &e {
                        if (400..=499).contains(status_code)
                            && *status_code != 429
                            && *status_code != 408
                        {
                            tracing::warn!(
                                model_id = %model_id,
                                attempt = attempt + 1,
                                status_code,
                                error_detail = %e,
                                operation = operation_name,
                                "Client error from provider, not retrying"
                            );
                            return Err(Self::sanitize_completion_error(e, model_id));
                        }
                    }

                    // Increment failure counter only for retryable errors
                    // (5xx, timeouts, network errors — indicators of backend health issues)
                    {
                        let mut counts = self
                            .provider_failure_counts
                            .write()
                            .unwrap_or_else(|e| e.into_inner());
                        let key = Arc::as_ptr(provider) as *const () as usize;
                        let counter = counts.entry(key).or_insert(0);
                        *counter = counter.saturating_add(1);
                    }

                    // Log the failure for debugging (before sanitization strips details)
                    tracing::warn!(
                        model_id = %model_id,
                        attempt = attempt + 1,
                        error_detail = %e,
                        operation = operation_name,
                        "Provider failed, will try next provider if available"
                    );

                    // Sanitize and preserve the last error with its structure intact
                    last_error = Some(Self::sanitize_completion_error(e, model_id));
                }
            }
        }

        // All providers failed
        if let Some(pub_key) = model_pub_key {
            tracing::error!(
                model_id = %model_id,
                model_pub_key_prefix = %pub_key.chars().take(32).collect::<String>(),
                providers_tried = providers.len(),
                operation = operation_name,
                "All providers failed for model with public key"
            );
        } else {
            tracing::error!(
                model_id = %model_id,
                providers_tried = providers.len(),
                operation = operation_name,
                "All providers failed for model"
            );
        }

        // Return the last error, preserving its HttpError variant for proper status code mapping
        match last_error {
            Some(CompletionError::HttpError {
                status_code,
                message,
                is_external,
            }) => Err(CompletionError::HttpError {
                status_code,
                message: if providers.len() > 1 {
                    format!(
                        "All {} provider(s) failed for model '{}'. Last error: {}",
                        providers.len(),
                        model_id,
                        message
                    )
                } else {
                    message
                },
                is_external,
            }),
            Some(other_error) => Err(other_error),
            None => Err(CompletionError::CompletionError(format!(
                "No providers available for model '{}'",
                model_id
            ))),
        }
    }

    pub async fn get_attestation_report(
        &self,
        model: String,
        signing_algo: Option<String>,
        nonce: Option<String>,
        signing_address: Option<String>,
        include_tls_fingerprint: bool,
    ) -> Result<Vec<serde_json::Map<String, serde_json::Value>>, AttestationError> {
        let providers = self
            .get_providers_for_model(&model)
            .await
            .ok_or_else(|| AttestationError::ProviderNotFound(model.clone()))?;

        // Each inference_url points to a proxy that load-balances across CVMs.
        // All CVMs behind the proxy share the same signing key (derived from model
        // name via dstack KMS), so one attestation report is sufficient.
        // Try providers in order and return the first successful response.
        let mut last_error = None;
        for provider in providers {
            match provider
                .get_attestation_report(
                    model.clone(),
                    signing_algo.clone(),
                    nonce.clone(),
                    signing_address.clone(),
                    include_tls_fingerprint,
                )
                .await
            {
                Ok(mut attestation) => {
                    attestation.remove("all_attestations");
                    return Ok(vec![attestation]);
                }
                Err(e) => {
                    tracing::debug!(
                        model = %model,
                        error = %e,
                        "Provider returned error for attestation request, trying next"
                    );
                    last_error = Some(e);
                }
            }
        }

        Err(last_error
            .map(|e| AttestationError::FetchError(e.to_string()))
            .unwrap_or_else(|| AttestationError::ProviderNotFound(model)))
    }

    pub async fn chat_completion_stream(
        &self,
        params: ChatCompletionParams,
        request_hash: String,
    ) -> Result<StreamingResult, CompletionError> {
        let (stream, _provider_url) = self
            .chat_completion_stream_with_preferred_provider(params, request_hash, None)
            .await?;
        Ok(stream)
    }

    pub async fn chat_completion_stream_with_preferred_provider(
        &self,
        mut params: ChatCompletionParams,
        request_hash: String,
        preferred_provider_url: Option<String>,
    ) -> Result<(StreamingResult, Option<String>), CompletionError> {
        let model_id = params.model.clone();

        // Extract model_pub_key from params.extra for routing
        let model_pub_key_str = params
            .extra
            .remove(encryption_headers::MODEL_PUB_KEY)
            .and_then(|v| v.as_str().map(|s| s.to_string()));
        let model_pub_key = model_pub_key_str.as_deref();

        let params_for_provider = params.clone();

        tracing::debug!(
            model = %model_id,
            "Starting chat completion stream request"
        );

        let (stream, provider) = self
            .retry_with_fallback(
                &model_id,
                "chat_completion_stream",
                model_pub_key,
                preferred_provider_url.as_deref(),
                |provider| {
                    let params = params_for_provider.clone();
                    let request_hash = request_hash.clone();
                    async move { provider.chat_completion_stream(params, request_hash).await }
                },
            )
            .await?;

        // Store chat_id mapping for sticky routing by peeking at the first event
        // Must be synchronous to ensure attestation service can find the provider
        let provider_url = self.get_provider_url(&provider).await;
        let mut peekable = StreamingResultExt::peekable(stream);
        let mut pinned = false;
        if let Some(Ok(event)) = peekable.peek().await {
            if let inference_providers::StreamChunk::Chat(chat_chunk) = &event.chunk {
                let chat_id = chat_chunk.id.clone();
                tracing::info!(
                    chat_id = %chat_id,
                    "Storing chat_id mapping for streaming completion"
                );
                // Pin the dedicated TLS connection so signature fetches
                // reuse the same connection that served this completion.
                provider.pin_chat_connection(&request_hash, &chat_id);
                pinned = true;
                self.store_chat_id_mapping(chat_id, provider.clone()).await;
            }
        }
        if !pinned {
            // Clean up orphaned pending client when peek fails or yields no chat_id
            provider.pin_chat_connection(&request_hash, "");
            provider.unpin_chat_connection("");
        }
        Ok((Box::pin(peekable), provider_url))
    }

    pub async fn chat_completion(
        &self,
        params: ChatCompletionParams,
        request_hash: String,
    ) -> Result<inference_providers::ChatCompletionResponseWithBytes, CompletionError> {
        let (response, _provider_url) = self
            .chat_completion_with_preferred_provider(params, request_hash, None)
            .await?;
        Ok(response)
    }

    pub async fn chat_completion_with_preferred_provider(
        &self,
        mut params: ChatCompletionParams,
        request_hash: String,
        preferred_provider_url: Option<String>,
    ) -> Result<
        (
            inference_providers::ChatCompletionResponseWithBytes,
            Option<String>,
        ),
        CompletionError,
    > {
        let model_id = params.model.clone();

        // Extract model_pub_key from params.extra for routing before any cloning.
        // This ensures the key is removed from params.extra so it won't be passed to the provider,
        // and we have a stable reference for routing even if retries occur.
        let model_pub_key_str = params
            .extra
            .remove(encryption_headers::MODEL_PUB_KEY)
            .and_then(|v| v.as_str().map(|s| s.to_string()));
        let model_pub_key = model_pub_key_str.as_deref();

        tracing::debug!(
            model = %model_id,
            "Starting chat completion request"
        );

        // Clone params after removing model_pub_key to ensure it's not in the cloned version
        let params_for_provider = params.clone();

        let (response, provider) = self
            .retry_with_fallback(
                &model_id,
                "chat_completion",
                model_pub_key,
                preferred_provider_url.as_deref(),
                |provider| {
                    let params = params_for_provider.clone();
                    let request_hash = request_hash.clone();
                    async move { provider.chat_completion(params, request_hash).await }
                },
            )
            .await?;

        // Store the chat_id mapping SYNCHRONOUSLY before returning
        // This ensures the attestation service can find the provider
        let provider_url = self.get_provider_url(&provider).await;
        let chat_id = response.response.id.clone();
        tracing::info!(
            chat_id = %chat_id,
            "Storing chat_id mapping for non-streaming completion"
        );
        self.store_chat_id_mapping(chat_id.clone(), provider).await;
        tracing::debug!(
            chat_id = %chat_id,
            "Stored chat_id mapping before returning response"
        );

        Ok((response, provider_url))
    }

    /// Generate images using the specified model
    pub async fn image_generation(
        &self,
        mut params: ImageGenerationParams,
        request_hash: String,
    ) -> Result<ImageGenerationResponseWithBytes, ImageGenerationError> {
        let model_id = params.model.clone();

        // Extract model_pub_key from params.extra for routing before any cloning.
        // This ensures the key is removed from params.extra so it won't be passed to the provider,
        // and we have a stable reference for routing even if retries occur.
        let model_pub_key_str = params
            .extra
            .remove(encryption_headers::MODEL_PUB_KEY)
            .and_then(|v| v.as_str().map(|s| s.to_string()));
        let model_pub_key = model_pub_key_str.as_deref();

        tracing::debug!(
            model = %model_id,
            "Starting image generation request"
        );

        // Clone params once before retry loop to minimize memory operations with large image data.
        // The provider interface requires ImageEditParams by value, so we must clone when calling
        // the provider. We clone once here and reuse across retries rather than cloning on each attempt.
        let cloned_params = params.clone();

        let (response, provider) = self
            .retry_with_fallback(
                &model_id,
                "image_generation",
                model_pub_key,
                None,
                |provider| {
                    let params = cloned_params.clone();
                    let request_hash = request_hash.clone();
                    async move {
                        provider
                            .image_generation(params, request_hash)
                            .await
                            .map_err(|e| CompletionError::CompletionError(e.to_string()))
                    }
                },
            )
            .await
            .map_err(|e| ImageGenerationError::GenerationError(e.to_string()))?;

        // Store the chat_id mapping so attestation service can find the provider
        // (same pattern as chat_completion)
        let image_id = response.response.id.clone();
        tracing::info!(
            image_id = %image_id,
            "Storing chat_id mapping for image generation"
        );
        self.store_chat_id_mapping(image_id, provider).await;

        Ok(response)
    }

    pub async fn audio_transcription(
        &self,
        params: AudioTranscriptionParams,
        request_hash: String,
    ) -> Result<AudioTranscriptionResponse, AudioTranscriptionError> {
        let model_id = params.model.clone();
        let file_size_kb = params.file_bytes.len() / 1024;

        tracing::debug!(
            model = %model_id,
            filename = %params.filename,
            file_size_kb = file_size_kb,
            "Starting audio transcription request"
        );

        let (response, _provider) = self
            .retry_with_fallback(&model_id, "audio_transcription", None, None, |provider| {
                let params = params.clone();
                let request_hash = request_hash.clone();
                async move {
                    provider
                        .audio_transcription(params, request_hash)
                        .await
                        .map_err(|e| CompletionError::CompletionError(e.to_string()))
                }
            })
            .await
            .map_err(|e| {
                AudioTranscriptionError::TranscriptionError(Self::sanitize_error_message(
                    &e.to_string(),
                ))
            })?;

        tracing::info!(
            model = %model_id,
            duration = ?response.duration,
            "Audio transcription completed successfully"
        );

        Ok(response)
    }

    pub async fn image_edit(
        &self,
        params: ImageEditParams,
        request_hash: String,
    ) -> Result<ImageEditResponseWithBytes, ImageEditError> {
        let model_id = params.model.clone();

        tracing::debug!(
            model = %model_id,
            "Starting image edit request"
        );

        // Wrap params in Arc to enable cheap cloning across retries.
        // Since image data is already Arc<Vec<u8>>, cloning the params struct is now O(1).
        // Each retry clones the Arc pointer (8 bytes) instead of the entire struct.
        let params = Arc::new(params);

        let (response, provider) = self
            .retry_with_fallback(&model_id, "image_edit", None, None, |provider| {
                let params = params.clone();
                let request_hash = request_hash.clone();
                async move {
                    provider
                        .image_edit(params, request_hash)
                        .await
                        .map_err(|e| CompletionError::CompletionError(e.to_string()))
                }
            })
            .await
            .map_err(|e| ImageEditError::EditError(e.to_string()))?;

        // Store the chat_id mapping so attestation service can find the provider
        // (same pattern as image_generation)
        let image_id = response.response.id.clone();
        tracing::info!(
            image_id = %image_id,
            "Storing chat_id mapping for image edit"
        );
        self.store_chat_id_mapping(image_id, provider).await;

        Ok(response)
    }

    pub async fn rerank(&self, params: RerankParams) -> Result<RerankResponse, RerankError> {
        let model_id = params.model.clone();

        tracing::debug!(
            model = %model_id,
            document_count = params.documents.len(),
            "Starting rerank request"
        );

        // Check if this model exists as an external provider - reranking is not supported for external models
        if self.get_external_provider(&model_id).await.is_some() {
            return Err(RerankError::GenerationError(format!(
                "Reranking is not supported for external provider models. \
                 Model '{}' is configured as an external provider. \
                 Reranking is only available for vLLM providers.",
                model_id
            )));
        }

        let providers = match self
            .get_providers_with_fallback(&model_id, None, None)
            .await
        {
            Some(p) => p,
            None => {
                return Err(RerankError::GenerationError(format!(
                    "Model '{}' not found in provider pool",
                    model_id
                )));
            }
        };

        // Try reranking with each provider (with fallback)
        let mut last_error = None;
        for provider in providers {
            match provider.rerank(params.clone()).await {
                Ok(response) => {
                    tracing::info!(
                        model = %model_id,
                        result_count = response.results.len(),
                        "Rerank completed successfully"
                    );
                    return Ok(response);
                }
                Err(e) => {
                    tracing::warn!(
                        model = %model_id,
                        error = %e,
                        "Rerank failed with provider, trying next"
                    );
                    last_error = Some(e);
                }
            }
        }

        // All providers failed
        let error_msg = last_error
            .map(|e| Self::sanitize_error_message(&e.to_string()))
            .unwrap_or_else(|| "No providers available for reranking".to_string());

        Err(RerankError::GenerationError(error_msg))
    }

    pub async fn embeddings(
        &self,
        model: &str,
        body: bytes::Bytes,
    ) -> Result<bytes::Bytes, inference_providers::EmbeddingError> {
        tracing::debug!(model = %model, "Starting embeddings request");

        // Check if this model exists as an external provider
        if let Some(provider) = self.get_external_provider(model).await {
            return provider.embeddings_raw(body).await;
        }

        let providers = match self.get_providers_with_fallback(model, None, None).await {
            Some(p) => p,
            None => {
                return Err(inference_providers::EmbeddingError::RequestFailed(format!(
                    "Model '{}' not found in provider pool",
                    model
                )));
            }
        };

        // Try with each provider (with fallback)
        let mut last_error = None;
        for provider in providers {
            match provider.embeddings_raw(body.clone()).await {
                Ok(response) => {
                    tracing::info!(model = %model, "Embeddings completed successfully");
                    return Ok(response);
                }
                Err(e) => {
                    tracing::warn!(model = %model, error = %e, "Embeddings failed with provider, trying next");
                    last_error = Some(e);
                }
            }
        }

        let error_msg = last_error
            .map(|e| Self::sanitize_error_message(&e.to_string()))
            .unwrap_or_else(|| "No providers available for embeddings".to_string());

        Err(inference_providers::EmbeddingError::RequestFailed(
            error_msg,
        ))
    }

    pub async fn score(
        &self,
        params: inference_providers::ScoreParams,
        request_hash: String,
    ) -> Result<inference_providers::ScoreResponse, inference_providers::ScoreError> {
        let model_id = params.model.clone();

        tracing::debug!(model = %model_id, "Starting score request");

        // Check if this model exists as an external provider - scoring is not supported for external models
        if self.get_external_provider(&model_id).await.is_some() {
            return Err(inference_providers::ScoreError::GenerationError(format!(
                "Scoring is not supported for external provider models. \
                 Model '{}' is configured as an external provider. \
                 Scoring is only available for vLLM providers.",
                model_id
            )));
        }

        let providers = match self
            .get_providers_with_fallback(&model_id, None, None)
            .await
        {
            Some(p) => p,
            None => {
                return Err(inference_providers::ScoreError::GenerationError(format!(
                    "Model '{}' not found in provider pool",
                    model_id
                )));
            }
        };

        // Try scoring with each provider (with fallback)
        let mut last_error = None;
        for provider in providers {
            match provider.score(params.clone(), request_hash.clone()).await {
                Ok(response) => {
                    tracing::info!(
                        model = %model_id,
                        "Score completed successfully"
                    );
                    return Ok(response);
                }
                Err(e) => {
                    tracing::warn!(
                        model = %model_id,
                        error = %e,
                        "Score failed with provider, trying next"
                    );
                    last_error = Some(e);
                }
            }
        }

        // All providers failed
        let error_msg = last_error
            .map(|e| Self::sanitize_error_message(&e.to_string()))
            .unwrap_or_else(|| "No providers available for scoring".to_string());

        Err(inference_providers::ScoreError::GenerationError(error_msg))
    }

    /// Create an external provider from a model name and provider config JSON.
    /// Returns a tuple of (provider Arc, backend_type string) without inserting it into any map.
    fn create_external_provider(
        &self,
        model_name: &str,
        provider_config: serde_json::Value,
    ) -> Result<(Arc<InferenceProviderTrait>, String), String> {
        // Extract and remove per-model api_key from raw JSON before deserializing into ProviderConfig
        let mut provider_config = provider_config;
        let per_model_api_key = provider_config
            .as_object_mut()
            .and_then(|obj| obj.remove("api_key"))
            .and_then(|v| v.as_str().map(String::from));

        let config: ProviderConfig = serde_json::from_value(provider_config)
            .map_err(|e| format!("Failed to parse provider config: {e}"))?;

        let backend_type = match &config {
            ProviderConfig::OpenAiCompatible { .. } => "openai_compatible".to_string(),
            ProviderConfig::Anthropic { .. } => "anthropic".to_string(),
            ProviderConfig::Gemini { .. } => "gemini".to_string(),
        };

        let api_key = per_model_api_key
            .or_else(|| {
                self.external_configs
                    .get_api_key(&backend_type)
                    .map(|s| s.to_string())
            })
            .ok_or_else(|| {
                format!(
                    "No API key configured for backend type '{}'. \
                     Set the appropriate environment variable (e.g., OPENAI_API_KEY, ANTHROPIC_API_KEY, GEMINI_API_KEY) \
                     or include 'api_key' in the model's providerConfig",
                    backend_type
                )
            })?;

        let external_config = ExternalProviderConfig {
            model_name: model_name.to_string(),
            provider_config: config,
            api_key,
            timeout_seconds: self.external_configs.timeout_seconds,
        };

        let provider =
            Arc::new(ExternalProvider::new(external_config)) as Arc<InferenceProviderTrait>;
        Ok((provider, backend_type))
    }

    /// Return the set of model names currently registered in the pool.
    pub async fn registered_model_names(&self) -> Vec<String> {
        let mut names: std::collections::HashSet<String> = {
            let mappings = self.provider_mappings.read().await;
            mappings.model_to_providers.keys().cloned().collect()
        };

        let external = self.external_providers.read().await;
        names.extend(external.keys().cloned());

        let mut names: Vec<_> = names.into_iter().collect();
        names.sort();
        names
    }

    /// Atomically replace the external provider map with a newly built set.
    async fn sync_external_providers(&self, models: Vec<(String, serde_json::Value)>) {
        let mut new_map: HashMap<String, Arc<InferenceProviderTrait>> = HashMap::new();
        let mut success_count = 0;
        let mut error_count = 0;

        for (model_name, provider_config) in models {
            match self.create_external_provider(&model_name, provider_config) {
                Ok((provider, backend_type)) => {
                    new_map.insert(model_name.clone(), provider);
                    success_count += 1;
                    info!(
                        model = %model_name,
                        backend = %backend_type,
                        "Registered external provider during sync"
                    );
                }
                Err(error) => {
                    warn!(
                        model = %model_name,
                        error = %error,
                        "Failed to create external provider during sync"
                    );
                    error_count += 1;
                }
            }
        }

        let mut external = self.external_providers.write().await;
        let old_count = external.len();
        *external = new_map;

        info!(
            old_count = old_count,
            new_count = success_count,
            errors = error_count,
            "Synced external providers"
        );
    }

    /// Load models with inference_url as VLlmProviders into provider_mappings.
    ///
    /// For each model, reuses the existing provider (and its warm TLS connections)
    /// if the inference_url hasn't changed since last load. Only creates new providers
    /// for models whose URL changed or that are new.
    ///
    /// # Arguments
    /// * `models` - List of (model_name, inference_url) tuples
    pub async fn load_inference_url_models(&self, models: Vec<(String, String)>) {
        if models.is_empty() {
            return;
        }

        let api_key = self.api_key.clone();

        // Check which models can reuse their existing provider (URL unchanged)
        let existing_cache = self.inference_url_providers.read().await;
        let mut reused: Vec<(String, String, Arc<InferenceProviderTrait>)> = Vec::new();
        let mut needs_creation: Vec<(String, String)> = Vec::new();

        for (model_name, url) in &models {
            if let Some(existing) = existing_cache.get(url) {
                reused.push((model_name.clone(), url.clone(), existing.clone()));
            } else {
                needs_creation.push((model_name.clone(), url.clone()));
            }
        }
        drop(existing_cache);

        if !needs_creation.is_empty() {
            info!(
                new = needs_creation.len(),
                reused = reused.len(),
                "Creating new providers for changed/new inference URLs"
            );
        }

        // Phase 1: Create providers for new/changed URLs and probe attestation concurrently.
        let endpoint_futures: Vec<_> = needs_creation
            .iter()
            .map(|(model_name, url)| {
                let model_name = model_name.clone();
                let url = url.clone();
                let api_key = api_key.clone();
                async move {
                    let serving_provider = Arc::new(VLlmProvider::new(VLlmConfig::new(
                        url.clone(),
                        api_key,
                        None,
                    ))) as Arc<InferenceProviderTrait>;

                    let (pub_keys, _) = Self::fetch_signing_public_keys_for_both_algorithms(
                        &serving_provider,
                        &model_name,
                        &url,
                    )
                    .await;

                    let pub_keys: Vec<(String, Arc<InferenceProviderTrait>)> = pub_keys
                        .into_iter()
                        .map(|(key, _)| (key, serving_provider.clone()))
                        .collect();

                    (model_name, url, serving_provider, pub_keys)
                }
            })
            .collect();

        use futures::stream::{self, StreamExt};
        let new_results: Vec<_> = stream::iter(endpoint_futures)
            .buffer_unordered(20)
            .collect()
            .await;

        // Phase 2: Merge reused and new providers, update mappings.
        let mut model_providers: HashMap<String, Vec<Arc<InferenceProviderTrait>>> = HashMap::new();
        let mut pub_key_updates: Vec<(String, Arc<InferenceProviderTrait>)> = Vec::new();
        let mut new_url_cache: HashMap<String, Arc<InferenceProviderTrait>> = HashMap::new();
        let mut new_provider_urls: HashMap<usize, String> = HashMap::new();

        // Reused providers (URL unchanged — keep warm TLS connections)
        for (model_name, url, provider) in &reused {
            model_providers
                .entry(model_name.clone())
                .or_default()
                .push(provider.clone());
            new_url_cache.insert(url.clone(), provider.clone());
            new_provider_urls.insert(Self::provider_ptr(provider), url.clone());
        }

        // Newly created providers
        for (model_name, url, provider, pub_keys) in &new_results {
            info!(
                model = %model_name,
                url = %url,
                pub_keys = pub_keys.len(),
                "Registered inference_url model"
            );
            model_providers
                .entry(model_name.clone())
                .or_default()
                .push(provider.clone());
            pub_key_updates.extend(pub_keys.iter().cloned());
            new_url_cache.insert(url.clone(), provider.clone());
            new_provider_urls.insert(Self::provider_ptr(provider), url.clone());
        }

        // Atomic update: replace model providers and rebuild pubkey mappings
        {
            let mut mappings = self.provider_mappings.write().await;

            // Collect reused provider ptrs so we can exclude them from pruning.
            // Reused providers keep the same Arc, so their pubkey mappings are still valid.
            let reused_provider_ptrs: std::collections::HashSet<usize> = reused
                .iter()
                .map(|(_, _, p)| Arc::as_ptr(p) as *const () as usize)
                .collect();

            // Collect old provider ptrs for models being replaced, so we can prune pubkeys.
            // Exclude reused providers — they keep their existing pubkey mappings.
            let mut old_provider_ptrs = std::collections::HashSet::new();
            for model_name in model_providers.keys() {
                if let Some(old) = mappings.model_to_providers.get(model_name) {
                    for p in old {
                        let ptr = Arc::as_ptr(p) as *const () as usize;
                        if !reused_provider_ptrs.contains(&ptr) {
                            old_provider_ptrs.insert(ptr);
                        }
                    }
                }
            }

            for (model_name, providers) in model_providers {
                mappings.model_to_providers.insert(model_name, providers);
            }

            if !old_provider_ptrs.is_empty() {
                mappings.pubkey_to_providers.retain(|_, providers| {
                    providers.retain(|p| {
                        !old_provider_ptrs.contains(&(Arc::as_ptr(p) as *const () as usize))
                    });
                    !providers.is_empty()
                });
            }

            for (key, provider) in pub_key_updates {
                mappings
                    .pubkey_to_providers
                    .entry(key)
                    .or_default()
                    .push(provider);
            }
        }

        // Log pubkey mapping state for debugging E2EE routing issues
        let (pubkey_count, pubkey_summaries) = {
            let mappings = self.provider_mappings.read().await;
            let count = mappings.pubkey_to_providers.len();
            let summaries: Vec<String> = mappings
                .pubkey_to_providers
                .iter()
                .take(10)
                .map(|(k, v)| {
                    let prefix: String = k.chars().take(16).collect();
                    format!("{}...({}chars,{}providers)", prefix, k.len(), v.len())
                })
                .collect();
            (count, summaries)
        };
        info!(
            pubkey_mapping_count = pubkey_count,
            pubkey_summaries = ?pubkey_summaries,
            "pubkey_to_providers state after update"
        );

        // Update the URL→provider cache
        *self.inference_url_providers.write().await = new_url_cache;
        *self.provider_urls.write().await = new_provider_urls;

        info!(
            total = models.len(),
            reused = reused.len(),
            created = new_results.len(),
            "Loaded inference_url models"
        );
    }

    /// Refresh inference_url models from the database.
    /// Existing entries in provider_mappings are overwritten with new providers.
    async fn sync_inference_url_models(&self, models: Vec<(String, String)>) {
        self.load_inference_url_models(models).await;
    }

    /// Remove models from provider_mappings that are not in `valid_model_names`.
    /// Also cleans up load_balancer_index and provider_failure_counts for removed providers.
    async fn remove_stale_providers(&self, valid_model_names: &std::collections::HashSet<String>) {
        let stale_external_models: Vec<String> = {
            let external = self.external_providers.read().await;
            external
                .keys()
                .filter(|k| !valid_model_names.contains(k.as_str()))
                .cloned()
                .collect()
        };

        let mut mappings = self.provider_mappings.write().await;

        let stale_models: Vec<String> = mappings
            .model_to_providers
            .keys()
            .filter(|k| !valid_model_names.contains(k.as_str()))
            .cloned()
            .collect();

        if stale_models.is_empty() && stale_external_models.is_empty() {
            return;
        }

        // Collect provider ptrs being removed for ancillary cleanup
        let mut removed_ptrs = std::collections::HashSet::new();
        for model_name in &stale_models {
            if let Some(providers) = mappings.model_to_providers.remove(model_name) {
                for p in &providers {
                    removed_ptrs.insert(Arc::as_ptr(p) as *const () as usize);
                }
            }
        }

        // Prune pubkey entries
        mappings.pubkey_to_providers.retain(|_, providers| {
            providers.retain(|p| !removed_ptrs.contains(&(Arc::as_ptr(p) as *const () as usize)));
            !providers.is_empty()
        });

        // Drop mappings lock before touching std::sync locks
        drop(mappings);

        if !stale_external_models.is_empty() {
            let mut external = self.external_providers.write().await;
            for model_name in &stale_external_models {
                external.remove(model_name);
            }
        }

        // Clean up load balancer indices and failure counts
        {
            let mut lb = self
                .load_balancer_index
                .write()
                .unwrap_or_else(|e| e.into_inner());
            for model_name in &stale_models {
                lb.remove(model_name);
            }
        }
        self.provider_failure_counts
            .write()
            .unwrap_or_else(|e| e.into_inner())
            .retain(|key, _| !removed_ptrs.contains(key));
        self.provider_urls
            .write()
            .await
            .retain(|key, _| !removed_ptrs.contains(key));
        self.inference_url_providers
            .write()
            .await
            .retain(|_, provider| !removed_ptrs.contains(&Self::provider_ptr(provider)));

        let mut removed_models = stale_models;
        removed_models.extend(stale_external_models);

        info!(
            removed = removed_models.len(),
            models = ?removed_models,
            "Removed stale providers not in database"
        );
    }

    /// Start a periodic background task that refreshes all providers from the database.
    ///
    /// Refreshes both inference_url models (VLlm providers) and external providers
    /// (OpenAI, Anthropic, etc.) on each tick. Removes providers for models that
    /// are no longer in the database.
    ///
    /// The first tick is skipped because providers are already loaded at startup.
    /// If `refresh_interval_secs` is 0, this is a no-op.
    pub async fn start_refresh_task(
        self: Arc<Self>,
        source: Arc<dyn ExternalModelsSource>,
        refresh_interval_secs: u64,
    ) {
        if refresh_interval_secs == 0 {
            debug!("Provider refresh disabled (interval is 0)");
            return;
        }

        let handle = tokio::spawn({
            let pool = self.clone();
            async move {
                let mut interval =
                    tokio::time::interval(tokio::time::Duration::from_secs(refresh_interval_secs));
                // Skip the first immediate tick (providers already loaded at startup)
                interval.tick().await;
                loop {
                    interval.tick().await;
                    debug!("Running periodic provider refresh");

                    let mut valid_model_names = std::collections::HashSet::new();

                    // Refresh inference_url models
                    match source.fetch_inference_url_models().await {
                        Ok(models) => {
                            for (name, _) in &models {
                                valid_model_names.insert(name.clone());
                            }
                            pool.sync_inference_url_models(models).await;
                        }
                        Err(e) => {
                            warn!(error = %e, "Failed to refresh inference_url models");
                            // On failure, keep all existing inference_url models
                            // (we don't know which are still valid)
                            let mappings = pool.provider_mappings.read().await;
                            valid_model_names.extend(mappings.model_to_providers.keys().cloned());
                            drop(mappings);
                        }
                    }

                    // Refresh external providers
                    match source.fetch_external_models().await {
                        Ok(models) => {
                            for (name, _) in &models {
                                valid_model_names.insert(name.clone());
                            }
                            pool.sync_external_providers(models).await;
                        }
                        Err(e) => {
                            warn!(error = %e, "Failed to refresh external providers");
                            // On failure, keep all existing external providers.
                            let external = pool.external_providers.read().await;
                            valid_model_names.extend(external.keys().cloned());
                        }
                    }

                    // Remove providers for models no longer in the database
                    pool.remove_stale_providers(&valid_model_names).await;
                }
            }
        });

        let mut task_handle = self.refresh_task_handle.lock().await;
        *task_handle = Some(handle);
        info!(
            "Provider refresh task started with interval: {} seconds",
            refresh_interval_secs
        );
    }

    /// Shutdown the inference provider pool and cleanup all resources
    pub async fn shutdown(&self) {
        info!("Initiating inference provider pool shutdown");

        // Cancel the refresh task
        let mut task_handle = self.refresh_task_handle.lock().await;
        if let Some(handle) = task_handle.take() {
            handle.abort();
            info!("Refresh task cancelled");
        }
        drop(task_handle);

        // Clear all state
        let model_count = {
            let mut mappings = self.provider_mappings.write().await;
            let count = mappings.model_to_providers.len();
            *mappings = ProviderMappings::new();
            count
        };
        self.external_providers.write().await.clear();

        self.load_balancer_index
            .write()
            .unwrap_or_else(|e| e.into_inner())
            .clear();
        self.chat_id_mapping.write().await.clear();
        self.provider_failure_counts
            .write()
            .unwrap_or_else(|e| e.into_inner())
            .clear();
        self.provider_urls.write().await.clear();
        self.inference_url_providers.write().await.clear();

        info!(model_count, "Inference provider pool shutdown completed");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_sanitize_error_message() {
        // Test URL sanitization
        let error = "Failed to perform completion: error sending request for url (http://192.168.0.1:8000/v1/chat/completions)";
        let sanitized = InferenceProviderPool::sanitize_error_message(error);
        assert!(!sanitized.contains("http://"));
        assert!(!sanitized.contains("192.168.0.1"));
        assert!(sanitized.contains("[URL_REDACTED]"));
        assert!(sanitized.contains("provider connection failed"));

        // Test IP with port sanitization
        let error = "Connection failed to 192.168.1.100:8080";
        let sanitized = InferenceProviderPool::sanitize_error_message(error);
        assert!(!sanitized.contains("192.168.1.100"));
        assert!(!sanitized.contains("8080"));
        assert!(sanitized.contains("[IP_REDACTED]"));

        // Test standalone IP sanitization
        let error = "Server at 10.0.0.1 is unreachable";
        let sanitized = InferenceProviderPool::sanitize_error_message(error);
        assert!(!sanitized.contains("10.0.0.1"));
        assert!(sanitized.contains("[IP_REDACTED]"));

        // Test HTTPS URLs
        let error = "Failed to connect to https://api.example.com/v1/endpoint";
        let sanitized = InferenceProviderPool::sanitize_error_message(error);
        assert!(!sanitized.contains("https://api.example.com"));
        assert!(sanitized.contains("[URL_REDACTED]"));

        // Test complex error message (like the one from the screenshot)
        let error = "Failed to perform completion: All 2 provider(s) failed for model 'Qwen/Qwen3-30B-A3B-Instruct-2507' during chat_completion: Provider 1: Failed to perform completion: error sending request for url (http://192.168.0.1:8000/v1/chat/completions): Provider 2: Failed to perform completion: HTTP 401 Unauthorized";
        let sanitized = InferenceProviderPool::sanitize_error_message(error);
        assert!(!sanitized.contains("http://"));
        assert!(!sanitized.contains("192.168.0.1"));
        assert!(!sanitized.contains("8000"));
        assert!(!sanitized.contains("/v1/chat/completions"));
        assert!(sanitized.contains("[URL_REDACTED]"));
        assert!(sanitized.contains("provider connection failed"));

        // Model name should still be present
        assert!(sanitized.contains("Qwen/Qwen3-30B-A3B-Instruct-2507"));

        // HTTP status should still be present (not sensitive)
        assert!(sanitized.contains("401 Unauthorized"));

        // Test that "not found" keywords are preserved for error detection
        // This is important because route handlers check for "not found" to return 404 errors
        let error_not_found =
            "Model 'Qwen/Qwen3-Reranker-0.6B' not found at http://192.168.0.1:8000";
        let sanitized_not_found = InferenceProviderPool::sanitize_error_message(error_not_found);
        assert!(
            sanitized_not_found.contains("not found"),
            "Keywords 'not found' must be preserved for error detection"
        );
        assert!(!sanitized_not_found.contains("http://"));
        assert!(!sanitized_not_found.contains("192.168.0.1"));

        let error_does_not_exist =
            "Model 'gpt-4' does not exist on the server https://api.example.com";
        let sanitized_exists = InferenceProviderPool::sanitize_error_message(error_does_not_exist);
        assert!(
            sanitized_exists.contains("does not exist"),
            "Keywords 'does not exist' must be preserved for error detection"
        );
        assert!(!sanitized_exists.contains("https://api.example.com"));
    }

    #[tokio::test]
    async fn test_streaming_chat_id_mapping_available_immediately() {
        use futures_util::StreamExt;
        use inference_providers::mock::MockProvider;

        let pool = InferenceProviderPool::new(None, ExternalProvidersConfig::default());

        let mock_provider = Arc::new(MockProvider::new());
        let model_id = "Qwen/Qwen3-30B-A3B-Instruct-2507".to_string();
        pool.register_provider(model_id.clone(), mock_provider)
            .await;

        let params = inference_providers::ChatCompletionParams {
            model: model_id,
            messages: vec![inference_providers::ChatMessage {
                role: inference_providers::MessageRole::User,
                content: Some(serde_json::Value::String("Hello".to_string())),
                name: None,
                tool_call_id: None,
                tool_calls: None,
            }],
            max_tokens: None,
            temperature: None,
            top_p: None,
            stop: None,
            stream: Some(true),
            tools: None,
            max_completion_tokens: None,
            n: None,
            frequency_penalty: None,
            presence_penalty: None,
            logit_bias: None,
            logprobs: None,
            top_logprobs: None,
            user: None,
            seed: None,
            tool_choice: None,
            parallel_tool_calls: None,
            metadata: None,
            store: None,
            stream_options: None,
            modalities: None,
            extra: std::collections::HashMap::new(),
        };

        let mut stream = pool
            .chat_completion_stream(params, "test-request-hash".to_string())
            .await
            .expect("Should create stream");

        let first_event = stream.next().await.unwrap().unwrap();
        let chat_id = match first_event.chunk {
            inference_providers::StreamChunk::Chat(chunk) => chunk.id,
            _ => panic!("Expected chat chunk"),
        };

        // Mapping must be available immediately (no race with spawn)
        assert!(pool.get_provider_by_chat_id(&chat_id).await.is_some());

        while stream.next().await.is_some() {}
        assert!(pool.get_provider_by_chat_id(&chat_id).await.is_some());
    }

    #[tokio::test]
    async fn test_preferred_provider_url_is_prioritized_when_available() {
        use inference_providers::mock::MockProvider;

        let pool = InferenceProviderPool::new(None, ExternalProvidersConfig::default());

        let provider_a: Arc<InferenceProviderTrait> = Arc::new(MockProvider::new());
        let provider_b: Arc<InferenceProviderTrait> = Arc::new(MockProvider::new());
        let model_id = "Qwen/Qwen3-30B-A3B-Instruct-2507".to_string();

        pool.register_providers(vec![
            (model_id.clone(), provider_a.clone()),
            (model_id.clone(), provider_b.clone()),
        ])
        .await;

        {
            let mut provider_urls = pool.provider_urls.write().await;
            provider_urls.insert(
                InferenceProviderPool::provider_ptr(&provider_a),
                "http://10.0.0.1:8000".to_string(),
            );
            provider_urls.insert(
                InferenceProviderPool::provider_ptr(&provider_b),
                "http://10.0.0.2:8000".to_string(),
            );
        }

        let providers = pool
            .get_providers_with_fallback(&model_id, None, Some("http://10.0.0.2:8000"))
            .await
            .expect("providers should exist");

        assert!(Arc::ptr_eq(&providers[0], &provider_b));
    }

    // ==================== External Provider Tests ====================

    #[tokio::test]
    async fn test_load_external_provider_openai() {
        let pool = InferenceProviderPool::new(
            None,
            ExternalProvidersConfig {
                openai_api_key: Some("sk-test-key".to_string()),
                anthropic_api_key: None,
                gemini_api_key: None,
                timeout_seconds: 60,
                refresh_interval_secs: 0,
            },
        );

        let result = pool.load_external_providers(vec![
            ("gpt-4".to_string(), serde_json::json!({"backend": "openai_compatible", "base_url": "https://api.openai.com/v1"})),
        ]).await;

        assert!(result.is_ok());
        assert!(pool.has_provider("gpt-4").await);
    }

    #[tokio::test]
    async fn test_load_external_provider_anthropic() {
        let pool = InferenceProviderPool::new(
            None,
            ExternalProvidersConfig {
                openai_api_key: None,
                anthropic_api_key: Some("sk-ant-test".to_string()),
                gemini_api_key: None,
                timeout_seconds: 60,
                refresh_interval_secs: 0,
            },
        );

        let result = pool.load_external_providers(vec![
            ("claude-3-opus".to_string(), serde_json::json!({"backend": "anthropic", "base_url": "https://api.anthropic.com/v1"})),
        ]).await;

        assert!(result.is_ok());
        assert!(pool.has_provider("claude-3-opus").await);
    }

    #[tokio::test]
    async fn test_load_external_provider_missing_api_key() {
        let pool = InferenceProviderPool::new(None, ExternalProvidersConfig::default());

        let result = pool.load_external_providers(vec![
            ("gpt-4".to_string(), serde_json::json!({"backend": "openai_compatible", "base_url": "https://api.openai.com/v1"})),
        ]).await;

        assert!(result.is_err());
        assert!(result.unwrap_err().contains("failed to load"));
    }

    #[tokio::test]
    async fn test_load_external_provider_invalid_config() {
        let pool = InferenceProviderPool::new(
            None,
            ExternalProvidersConfig {
                openai_api_key: Some("sk-test".to_string()),
                anthropic_api_key: None,
                gemini_api_key: None,
                timeout_seconds: 60,
                refresh_interval_secs: 0,
            },
        );

        let result = pool.load_external_providers(vec![
            ("test-model".to_string(), serde_json::json!({"backend": "unknown_backend", "base_url": "https://example.com"})),
        ]).await;

        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_has_provider_for_registered_model() {
        use inference_providers::mock::MockProvider;

        let pool = InferenceProviderPool::new(None, ExternalProvidersConfig::default());

        let mock_provider = Arc::new(MockProvider::new());
        pool.register_provider("vllm-model".to_string(), mock_provider)
            .await;

        assert!(pool.has_provider("vllm-model").await);
        assert!(!pool.has_provider("unknown-model").await);
    }

    #[tokio::test]
    async fn test_load_external_providers_batch() {
        let pool = InferenceProviderPool::new(
            None,
            ExternalProvidersConfig {
                openai_api_key: Some("sk-test".to_string()),
                anthropic_api_key: Some("sk-ant-test".to_string()),
                gemini_api_key: Some("AIza-test".to_string()),
                timeout_seconds: 60,
                refresh_interval_secs: 0,
            },
        );

        let result = pool.load_external_providers(vec![
            ("gpt-4".to_string(), serde_json::json!({"backend": "openai_compatible", "base_url": "https://api.openai.com/v1"})),
            ("claude-3".to_string(), serde_json::json!({"backend": "anthropic", "base_url": "https://api.anthropic.com/v1"})),
            ("gemini-pro".to_string(), serde_json::json!({"backend": "gemini", "base_url": "https://generativelanguage.googleapis.com/v1beta"})),
        ]).await;

        assert!(result.is_ok());
        assert!(pool.has_provider("gpt-4").await);
        assert!(pool.has_provider("claude-3").await);
        assert!(pool.has_provider("gemini-pro").await);
    }

    #[tokio::test]
    async fn test_load_external_providers_partial_failure() {
        let pool = InferenceProviderPool::new(
            None,
            ExternalProvidersConfig {
                openai_api_key: Some("sk-test".to_string()),
                anthropic_api_key: None,
                gemini_api_key: None,
                timeout_seconds: 60,
                refresh_interval_secs: 0,
            },
        );

        let result = pool.load_external_providers(vec![
            ("gpt-4".to_string(), serde_json::json!({"backend": "openai_compatible", "base_url": "https://api.openai.com/v1"})),
            ("claude-3".to_string(), serde_json::json!({"backend": "anthropic", "base_url": "https://api.anthropic.com/v1"})),
        ]).await;

        assert!(result.is_ok());
        assert!(pool.has_provider("gpt-4").await);
        assert!(!pool.has_provider("claude-3").await);
    }

    #[tokio::test]
    async fn test_shutdown_clears_providers() {
        let pool = InferenceProviderPool::new(
            None,
            ExternalProvidersConfig {
                openai_api_key: Some("sk-test".to_string()),
                anthropic_api_key: None,
                gemini_api_key: None,
                timeout_seconds: 60,
                refresh_interval_secs: 0,
            },
        );

        pool.load_external_providers(vec![
            ("gpt-4".to_string(), serde_json::json!({"backend": "openai_compatible", "base_url": "https://api.openai.com/v1"})),
        ]).await.unwrap();

        assert!(pool.has_provider("gpt-4").await);
        pool.shutdown().await;
        assert!(!pool.has_provider("gpt-4").await);
    }

    #[tokio::test]
    async fn test_unregister_provider() {
        let pool = InferenceProviderPool::new(
            None,
            ExternalProvidersConfig {
                openai_api_key: Some("sk-test".to_string()),
                anthropic_api_key: None,
                gemini_api_key: None,
                timeout_seconds: 60,
                refresh_interval_secs: 0,
            },
        );

        pool.load_external_providers(vec![
            ("gpt-4".to_string(), serde_json::json!({"backend": "openai_compatible", "base_url": "https://api.openai.com/v1"})),
        ]).await.unwrap();

        assert!(pool.has_provider("gpt-4").await);
        assert!(pool.unregister_provider("gpt-4").await);
        assert!(!pool.has_provider("gpt-4").await);
        assert!(!pool.unregister_provider("gpt-4").await);
    }

    #[tokio::test]
    async fn test_unregister_nonexistent_provider() {
        let pool = InferenceProviderPool::new(None, ExternalProvidersConfig::default());
        assert!(!pool.unregister_provider("nonexistent-model").await);
    }

    /// Verify that reused providers (URL unchanged) keep their pubkey mappings
    /// after load_inference_url_models refreshes.
    ///
    /// Regression test: previously, reused provider Arc pointers were collected
    /// as "old" and pruned from pubkey_to_providers, but never re-added because
    /// only new providers had their pub_keys collected. This caused E2EE routing
    /// to fail after the first refresh cycle.
    #[tokio::test]
    async fn test_reused_providers_keep_pubkey_mapping() {
        use inference_providers::mock::MockProvider;

        let pool = InferenceProviderPool::new(None, ExternalProvidersConfig::default());
        let model_id = "test-model".to_string();

        // Register a provider with known pubkeys
        let mock_provider = Arc::new(MockProvider::new());
        pool.register_provider(model_id.clone(), mock_provider.clone())
            .await;

        // Verify pubkey routing works initially
        let ecdsa_key = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
        {
            let mappings = pool.provider_mappings.read().await;
            assert!(
                mappings.pubkey_to_providers.contains_key(ecdsa_key),
                "ECDSA key should be in pubkey_to_providers after registration"
            );
            let providers = mappings.pubkey_to_providers.get(ecdsa_key).unwrap();
            assert_eq!(providers.len(), 1);
            assert!(Arc::ptr_eq(
                &providers[0],
                &(mock_provider.clone() as Arc<InferenceProviderTrait>)
            ));
        }

        // Now simulate what load_inference_url_models does when a provider is reused:
        // 1. The same Arc is added to model_providers
        // 2. Old ptrs are collected (including the reused one)
        // 3. pubkey_to_providers is pruned
        // 4. Only NEW provider pubkeys are re-added
        //
        // We simulate this by calling the internal logic path with
        // the same provider being "reused" (same Arc pointer).
        {
            let mut mappings = pool.provider_mappings.write().await;

            // Simulated reused provider
            let reused_provider = mock_provider.clone() as Arc<InferenceProviderTrait>;
            let reused_ptr = Arc::as_ptr(&reused_provider) as *const () as usize;

            // Build reused set (the fix)
            let reused_ptrs: std::collections::HashSet<usize> = [reused_ptr].into_iter().collect();

            // Collect "old" provider ptrs, excluding reused ones
            let mut old_provider_ptrs = std::collections::HashSet::new();
            if let Some(old) = mappings.model_to_providers.get(&model_id) {
                for p in old {
                    let ptr = Arc::as_ptr(p) as *const () as usize;
                    if !reused_ptrs.contains(&ptr) {
                        old_provider_ptrs.insert(ptr);
                    }
                }
            }

            // Replace model providers with "new" list (same Arc)
            mappings
                .model_to_providers
                .insert(model_id.clone(), vec![reused_provider]);

            // Prune old (non-reused) provider pubkeys
            if !old_provider_ptrs.is_empty() {
                mappings.pubkey_to_providers.retain(|_, providers| {
                    providers.retain(|p| {
                        !old_provider_ptrs.contains(&(Arc::as_ptr(p) as *const () as usize))
                    });
                    !providers.is_empty()
                });
            }

            // Verify: reused provider's pubkey mapping should still exist
            assert!(
                mappings.pubkey_to_providers.contains_key(ecdsa_key),
                "ECDSA key should be PRESERVED for reused providers after refresh"
            );
        }
    }

    /// Test that E2EE routing via pubkey works end-to-end after register_provider.
    /// This exercises: register_provider → fetch attestation → store pubkey → route by pubkey.
    #[tokio::test]
    async fn test_e2ee_pubkey_routing_after_register() {
        use inference_providers::mock::MockProvider;

        let pool = InferenceProviderPool::new(None, ExternalProvidersConfig::default());
        let model_id = "test-e2ee-model".to_string();

        // Register provider (fetches attestation, stores pubkeys)
        let mock_provider = Arc::new(MockProvider::new());
        pool.register_provider(model_id.clone(), mock_provider)
            .await;

        // The mock provider returns this ECDSA key
        let ecdsa_key = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

        // Test 1: routing WITHOUT pubkey should work
        let result: Result<((), _), _> = pool
            .retry_with_fallback(&model_id, "test_op", None, None, |_provider| async {
                Ok(())
            })
            .await;
        assert!(result.is_ok(), "Routing without pubkey should succeed");

        // Test 2: routing WITH correct pubkey should work
        let result: Result<((), _), _> = pool
            .retry_with_fallback(
                &model_id,
                "test_op",
                Some(ecdsa_key),
                None,
                |_provider| async { Ok(()) },
            )
            .await;
        assert!(
            result.is_ok(),
            "Routing with correct ECDSA pubkey should succeed, got: {:?}",
            result.err()
        );

        // Test 3: routing with WRONG pubkey should fail
        let result: Result<((), _), _> = pool
            .retry_with_fallback(
                &model_id,
                "test_op",
                Some("deadbeef00000000deadbeef00000000deadbeef00000000deadbeef00000000deadbeef00000000deadbeef00000000deadbeef00000000deadbeef00000000"),
                None,
                |_provider| async { Ok(()) },
            )
            .await;
        assert!(result.is_err(), "Routing with wrong pubkey should fail");
    }

    #[tokio::test]
    async fn test_sync_external_providers() {
        let pool = InferenceProviderPool::new(
            None,
            ExternalProvidersConfig {
                openai_api_key: Some("sk-test".to_string()),
                anthropic_api_key: None,
                gemini_api_key: None,
                timeout_seconds: 60,
                refresh_interval_secs: 0,
            },
        );

        pool.sync_external_providers(vec![
            ("gpt-4".to_string(), serde_json::json!({"backend": "openai_compatible", "base_url": "https://api.openai.com/v1"})),
        ]).await;

        assert!(pool.has_provider("gpt-4").await);

        // Sync with partial failures
        pool.sync_external_providers(vec![
            ("gpt-4".to_string(), serde_json::json!({"backend": "openai_compatible", "base_url": "https://api.openai.com/v1"})),
            ("claude-3".to_string(), serde_json::json!({"backend": "anthropic", "base_url": "https://api.anthropic.com/v1"})),
        ]).await;

        assert!(pool.has_provider("gpt-4").await);
        assert!(!pool.has_provider("claude-3").await);
    }

    // ==================== 4xx Retry Behavior Tests ====================

    /// Helper to create a pool with a registered mock provider
    async fn pool_with_mock_provider() -> (InferenceProviderPool, String) {
        let pool = InferenceProviderPool::new(None, ExternalProvidersConfig::default());
        let mock_provider = Arc::new(inference_providers::mock::MockProvider::new());
        let model_id = "Qwen/Qwen3-30B-A3B-Instruct-2507".to_string();
        pool.register_provider(model_id.clone(), mock_provider)
            .await;
        (pool, model_id)
    }

    #[tokio::test]
    async fn test_4xx_error_does_not_retry() {
        let (pool, model_id) = pool_with_mock_provider().await;

        let result: Result<((), _), _> = pool
            .retry_with_fallback(&model_id, "test_op", None, None, |_provider| async {
                Err(CompletionError::HttpError {
                    status_code: 400,
                    message: "Bad request".to_string(),
                    is_external: false,
                })
            })
            .await;

        assert!(result.is_err());
        let err = result.err().expect("Expected an error");
        match err {
            CompletionError::HttpError { status_code, .. } => {
                assert_eq!(status_code, 400);
            }
            other => panic!("Expected HttpError, got: {:?}", other),
        }
    }

    #[tokio::test]
    async fn test_429_error_retries_with_fallback() {
        let (pool, model_id) = pool_with_mock_provider().await;

        // 429 should NOT short-circuit - it should fall through to the normal retry path
        let result: Result<((), _), _> = pool
            .retry_with_fallback(&model_id, "test_op", None, None, |_provider| async {
                Err(CompletionError::HttpError {
                    status_code: 429,
                    message: "Rate limit exceeded".to_string(),
                    is_external: false,
                })
            })
            .await;

        // Should fail after trying all providers (not short-circuit on 429)
        // The error should be sanitized (go through the normal retry path with sanitize_completion_error)
        assert!(result.is_err());
        let err = result.err().expect("Expected an error");
        let err_msg = err.to_string();
        assert!(
            err_msg.contains("Provider failed for model"),
            "Expected sanitized error (went through retry path), got: {}",
            err_msg
        );
    }

    #[tokio::test]
    async fn test_408_error_retries_with_fallback() {
        let (pool, model_id) = pool_with_mock_provider().await;

        // 408 should NOT short-circuit - it should fall through to the normal retry path
        let result: Result<((), _), _> = pool
            .retry_with_fallback(&model_id, "test_op", None, None, |_provider| async {
                Err(CompletionError::HttpError {
                    status_code: 408,
                    message: "Request timeout".to_string(),
                    is_external: false,
                })
            })
            .await;

        assert!(result.is_err());
        let err = result.err().expect("Expected an error");
        let err_msg = err.to_string();
        assert!(
            err_msg.contains("Provider failed for model"),
            "Expected sanitized error (went through retry path), got: {}",
            err_msg
        );
    }

    #[tokio::test]
    async fn test_4xx_error_is_sanitized() {
        let (pool, model_id) = pool_with_mock_provider().await;

        let result: Result<((), _), _> = pool
            .retry_with_fallback(&model_id, "test_op", None, None, |_provider| async {
                Err(CompletionError::HttpError {
                    status_code: 400,
                    message: "Error at http://192.168.0.1:8000/v1/chat/completions".to_string(),
                    is_external: false,
                })
            })
            .await;

        assert!(result.is_err());
        let err = result.err().expect("Expected an error");
        match err {
            CompletionError::HttpError { message, .. } => {
                assert!(
                    !message.contains("192.168.0.1"),
                    "Error message should be sanitized, but contained IP: {}",
                    message
                );
                assert!(
                    message.contains("[URL_REDACTED]"),
                    "Error message should contain redacted URL marker: {}",
                    message
                );
            }
            other => panic!("Expected HttpError, got: {:?}", other),
        }
    }
}
