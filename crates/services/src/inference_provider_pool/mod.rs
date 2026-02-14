use crate::common::encryption_headers;
use config::ExternalProvidersConfig;
use inference_providers::{
    models::{AttestationError, CompletionError, ListModelsError, ModelsResponse},
    AudioTranscriptionError, AudioTranscriptionParams, AudioTranscriptionResponse,
    ChatCompletionParams, ExternalProvider, ExternalProviderConfig, ImageEditError,
    ImageEditParams, ImageEditResponseWithBytes, ImageGenerationError, ImageGenerationParams,
    ImageGenerationResponseWithBytes, InferenceProvider, ProviderConfig, RerankError, RerankParams,
    RerankResponse, StreamingResult, StreamingResultExt, VLlmConfig, VLlmProvider,
};
use regex::Regex;
use serde::Deserialize;
use std::{collections::HashMap, net::IpAddr, sync::Arc, time::Duration};
use tokio::sync::{Mutex, RwLock};
use tracing::{debug, info, warn};

type InferenceProviderTrait = dyn InferenceProvider + Send + Sync;

/// Trait for fetching external model configurations from a data source (e.g., database).
/// This decouples the InferenceProviderPool from the database crate (hexagonal architecture).
#[async_trait::async_trait]
pub trait ExternalModelsSource: Send + Sync {
    async fn fetch_external_models(&self) -> Result<Vec<(String, serde_json::Value)>, String>;
}

/// Discovery entry returned by the discovery layer
#[derive(Debug, Clone, Deserialize)]
struct DiscoveryEntry {
    /// Model identifier (e.g., "Qwen/Qwen3-30B-A3B-Instruct-2507")
    model: String,
    /// Tags for filtering/routing (e.g., ["prod", "dev"])
    #[serde(default)]
    tags: Vec<String>,
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
    /// Discovery URL for dynamic model discovery
    discovery_url: String,
    /// Optional API key for authenticating with discovered providers
    api_key: Option<String>,
    /// HTTP timeout for discovery requests
    discovery_timeout: Duration,
    /// Combined provider mappings (updated atomically to prevent race conditions)
    provider_mappings: Arc<RwLock<ProviderMappings>>,
    /// External providers (keyed by model name, created from database config)
    external_providers: Arc<RwLock<HashMap<String, Arc<InferenceProviderTrait>>>>,
    /// Configuration for external providers (API keys, timeouts, etc.)
    external_configs: ExternalProvidersConfig,
    /// Round-robin index for each model
    load_balancer_index: Arc<RwLock<HashMap<String, usize>>>,
    /// Map of chat_id -> provider for sticky routing
    chat_id_mapping: Arc<RwLock<HashMap<String, Arc<InferenceProviderTrait>>>>,
    /// Background task handle for periodic model discovery refresh
    refresh_task_handle: Arc<Mutex<Option<tokio::task::JoinHandle<()>>>>,
    /// Background task handle for periodic external provider refresh
    external_refresh_task_handle: Arc<Mutex<Option<tokio::task::JoinHandle<()>>>>,
}

impl InferenceProviderPool {
    /// Create a new pool with discovery URL and optional API key
    pub fn new(
        discovery_url: String,
        api_key: Option<String>,
        discovery_timeout_secs: i64,
        external_configs: ExternalProvidersConfig,
    ) -> Self {
        Self {
            discovery_url,
            api_key,
            discovery_timeout: Duration::from_secs(discovery_timeout_secs as u64),
            provider_mappings: Arc::new(RwLock::new(ProviderMappings::new())),
            external_providers: Arc::new(RwLock::new(HashMap::new())),
            external_configs,
            load_balancer_index: Arc::new(RwLock::new(HashMap::new())),
            chat_id_mapping: Arc::new(RwLock::new(HashMap::new())),
            refresh_task_handle: Arc::new(Mutex::new(None)),
            external_refresh_task_handle: Arc::new(Mutex::new(None)),
        }
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

    /// Load and register external providers from a list of model configurations
    ///
    /// This should be called during application startup after fetching external
    /// models from the database.
    ///
    /// # Arguments
    /// * `models` - List of (model_name, provider_config) tuples
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

    /// Unregister an external provider by model name
    ///
    /// This removes the provider from the pool, preventing future requests to this model.
    /// Should be called when a model is deleted or deactivated.
    ///
    /// # Arguments
    /// * `model_name` - The model name to unregister
    ///
    /// # Returns
    /// * `true` if the provider was found and removed
    /// * `false` if the provider was not found (may have already been removed)
    pub async fn unregister_external_provider(&self, model_name: &str) -> bool {
        let mut external = self.external_providers.write().await;
        let removed = external.remove(model_name).is_some();
        if removed {
            info!(model = %model_name, "Unregistered external provider");
        } else {
            debug!(model = %model_name, "External provider not found for unregistration (may be vLLM model)");
        }
        removed
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

    /// Initialize model discovery - should be called during application startup
    pub async fn initialize(&self) -> Result<(), ListModelsError> {
        tracing::debug!(
            url = %self.discovery_url,
            "Initializing model discovery from discovery server"
        );

        match self.discover_models().await {
            Ok(models_response) => {
                tracing::info!(
                    "Successfully discovered {} models",
                    models_response.data.len()
                );
                Ok(())
            }
            Err(e) => {
                tracing::error!("Failed to initialize model discovery");
                Err(e)
            }
        }
    }

    /// Fetch and parse models from discovery endpoint
    async fn fetch_from_discovery(
        &self,
    ) -> Result<HashMap<String, DiscoveryEntry>, ListModelsError> {
        tracing::debug!(
            url = %self.discovery_url,
            "Fetching models from discovery server"
        );

        let client = reqwest::Client::builder()
            .timeout(self.discovery_timeout)
            .build()
            .map_err(|e| {
                ListModelsError::FetchError(format!("Failed to create HTTP client: {e}"))
            })?;

        let response = client
            .get(&self.discovery_url)
            .send()
            .await
            .map_err(|e| ListModelsError::FetchError(format!("HTTP request failed: {e}")))?;

        let discovery_map: HashMap<String, DiscoveryEntry> = response
            .json()
            .await
            .map_err(|e| ListModelsError::FetchError(format!("Failed to parse JSON: {e}")))?;

        tracing::debug!(entries = discovery_map.len(), "Received discovery response");

        Ok(discovery_map)
    }

    /// Parse IP-based keys like "192.0.2.1:8000"
    /// Returns None for keys that don't match IP:PORT format (e.g., "redpill:...")
    fn parse_ip_port(key: &str) -> Option<(String, u16)> {
        let parts: Vec<&str> = key.split(':').collect();
        if parts.len() != 2 {
            return None;
        }

        let ip = parts[0];
        let port = parts[1].parse::<u16>().ok()?;

        // Verify it's a valid IP address
        if ip.parse::<IpAddr>().is_err() {
            return None;
        }

        Some((ip.to_string(), port))
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

    /// Ensure models are discovered before using them
    async fn ensure_models_discovered(&self) -> Result<(), CompletionError> {
        let mappings = self.provider_mappings.read().await;

        // If mapping is empty, we need to discover models
        if mappings.model_to_providers.is_empty() {
            drop(mappings); // Release read lock
            tracing::warn!("Model mapping is empty, triggering model discovery");
            self.discover_models().await.map_err(|e| {
                CompletionError::CompletionError(format!("Failed to discover models: {e}"))
            })?;
        }

        Ok(())
    }

    async fn discover_models(&self) -> Result<ModelsResponse, ListModelsError> {
        tracing::info!("Starting model discovery from discovery endpoint");

        // Fetch from discovery server
        let discovery_map = self.fetch_from_discovery().await?;

        // Group by model name
        let mut model_to_endpoints: HashMap<String, Vec<(String, u16)>> = HashMap::new();

        for (key, entry) in discovery_map {
            // Filter out non-IP keys
            if let Some((ip, port)) = Self::parse_ip_port(&key) {
                tracing::debug!(
                    key = %key,
                    model = %entry.model,
                    tags = ?entry.tags,
                    ip = %ip,
                    port = port,
                    "Adding IP-based provider"
                );

                model_to_endpoints
                    .entry(entry.model)
                    .or_default()
                    .push((ip, port));
            } else {
                tracing::debug!(
                    key = %key,
                    model = %entry.model,
                    tags = ?entry.tags,
                    "Skipping non-IP key"
                );
            }
        }

        // Phase 1: Collect all attestation reports and create providers (no locks held)
        // This minimizes lock duration by doing all network I/O before acquiring locks
        let mut all_models = Vec::new();
        let mut model_providers: HashMap<String, Vec<Arc<InferenceProviderTrait>>> = HashMap::new();
        let mut model_pub_key_updates: Vec<(String, Arc<InferenceProviderTrait>)> = Vec::new();

        for (model_name, endpoints) in model_to_endpoints {
            tracing::info!(
                model = %model_name,
                providers_count = endpoints.len(),
                "Discovered model with {} provider(s)",
                endpoints.len()
            );

            let mut providers_for_model = Vec::new();

            for (ip, port) in endpoints {
                let url = format!("http://{ip}:{port}");
                tracing::debug!(
                    model = %model_name,
                    url = %url,
                    "Creating provider for model"
                );

                let provider = Arc::new(VLlmProvider::new(VLlmConfig::new(
                    url.clone(),
                    self.api_key.clone(),
                    None,
                ))) as Arc<InferenceProviderTrait>;

                // Fetch attestation report with retries to handle transient network failures
                // We need at least one successful attestation report to include the provider
                // But we fetch both ECDSA and Ed25519 keys to register the provider for both algorithms
                let (pub_keys, has_valid_attestation) =
                    Self::fetch_signing_public_keys_for_both_algorithms(
                        &provider,
                        &model_name,
                        &url,
                    )
                    .await;

                // If we got at least one successful attestation report, the provider is valid and should be included
                // Note: signing_public_key may not be present in the attestation report (e.g., for non-encrypted providers)
                if has_valid_attestation {
                    model_pub_key_updates.extend(pub_keys);
                    providers_for_model.push(provider);
                }
            }

            if !providers_for_model.is_empty() {
                model_providers.insert(model_name.clone(), providers_for_model);

                all_models.push(inference_providers::models::ModelInfo {
                    id: model_name,
                    object: "model".to_string(),
                    created: 0,
                    owned_by: "discovered".to_string(),
                });
            }
        }

        // Calculate metrics before acquiring locks
        let total_providers: usize = model_providers.values().map(|v| v.len()).sum();

        // Phase 2: Atomic bulk update of both mappings under a single lock
        // This ensures consistency - both mappings are updated together atomically
        // Build new mappings structure
        let mut new_mappings = ProviderMappings::new();
        for (model_name, providers) in model_providers {
            new_mappings
                .model_to_providers
                .insert(model_name, providers);
        }
        for (key, provider) in model_pub_key_updates {
            new_mappings
                .pubkey_to_providers
                .entry(key)
                .or_default()
                .push(provider);
        }

        // Atomic swap: replace entire mappings structure in one operation
        {
            let mut mappings = self.provider_mappings.write().await;
            *mappings = new_mappings;
        }

        tracing::info!(
            total_models = all_models.len(),
            total_providers = total_providers,
            model_ids = ?all_models.iter().map(|m| &m.id).collect::<Vec<_>>(),
            "Model discovery from endpoint completed"
        );

        Ok(ModelsResponse {
            object: "list".to_string(),
            data: all_models,
        })
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

        let mut indices = self.load_balancer_index.write().await;
        let index = indices.entry(index_key.clone()).or_insert(0);
        let selected_index = *index % providers.len();

        // Increment for next request
        *index = (*index + 1) % providers.len();

        // Build ordered list following round-robin pattern:
        // selected provider first, then continue round-robin (selected+1, selected+2, ...)
        let mut ordered_providers = Vec::with_capacity(providers.len());
        for i in 0..providers.len() {
            let provider_index = (selected_index + i) % providers.len();
            ordered_providers.push(providers[provider_index].clone());
        }

        tracing::debug!(
            index_key = %index_key,
            providers_count = providers.len(),
            selected_index = selected_index,
            "Prepared providers for fallback with round-robin priority"
        );

        Some(ordered_providers)
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

    /// Generic retry helper that tries each provider in order with automatic fallback
    /// Returns both the result and the provider that succeeded (for chat_id mapping)
    /// If model_pub_key is provided, routes to the specific provider by signing public key first
    ///
    /// Provider resolution order:
    /// 1. External providers (exact match by model name)
    /// 2. vLLM providers (with model_pub_key routing if specified)
    async fn retry_with_fallback<T, F, Fut>(
        &self,
        model_id: &str,
        operation_name: &str,
        model_pub_key: Option<&str>,
        provider_fn: F,
    ) -> Result<(T, Arc<InferenceProviderTrait>), CompletionError>
    where
        F: Fn(Arc<InferenceProviderTrait>) -> Fut,
        Fut: std::future::Future<Output = Result<T, CompletionError>>,
    {
        // Check external providers first (exact match)
        if let Some(external_provider) = self.get_external_provider(model_id).await {
            tracing::info!(
                model_id = %model_id,
                operation = operation_name,
                "Using external provider"
            );

            // External providers don't support model_pub_key routing
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
                Err(e) => {
                    return Err(Self::sanitize_completion_error(e, model_id));
                }
            }
        }

        // Ensure vLLM models are discovered
        self.ensure_models_discovered().await?;

        // Get vLLM providers with load balancing (handles both model_id and model_pub_key cases)
        let providers = match self
            .get_providers_with_fallback(model_id, model_pub_key)
            .await
        {
            Some(p) => p,
            None => {
                // Handle error cases based on whether model_pub_key was provided
                if let Some(pub_key) = model_pub_key {
                    tracing::warn!(
                            model_id = %model_id,
                            model_pub_key = %pub_key,
                            operation = operation_name,
                        "No provider found for model public key."
                    );
                    return Err(CompletionError::CompletionError(format!(
                        "No provider found for model {} with public key '{}...'. Encryption requires routing to the specific provider with this public key.",
                        model_id,
                        pub_key.chars().take(32).collect::<String>()
                    )));
                } else {
                    let mappings = self.provider_mappings.read().await;
                    let available_models: Vec<_> = mappings.model_to_providers.keys().collect();
                    let external = self.external_providers.read().await;
                    let external_models: Vec<_> = external.keys().collect();

                    tracing::error!(
                        model_id = %model_id,
                        available_vllm_models = ?available_models,
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
                    tracing::info!(
                        model_id = %model_id,
                        attempt = attempt + 1,
                        operation = operation_name,
                        "Successfully completed request with provider"
                    );
                    return Ok((result, provider.clone()));
                }
                Err(e) => {
                    // Log the failure for debugging
                    tracing::warn!(
                        model_id = %model_id,
                        attempt = attempt + 1,
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
    ) -> Result<Vec<serde_json::Map<String, serde_json::Value>>, AttestationError> {
        // Get all providers for this model
        let mut model_attestations = vec![];

        if let Some(providers) = self.get_providers_for_model(&model).await {
            // Broadcast to all providers
            for provider in providers {
                match provider
                    .get_attestation_report(
                        model.clone(),
                        signing_algo.clone(),
                        nonce.clone(),
                        signing_address.clone(),
                    )
                    .await
                {
                    Ok(mut attestation) => {
                        // Remove 'all_attestations' field if present
                        attestation.remove("all_attestations");
                        model_attestations.push(attestation);
                    }
                    Err(e) => {
                        // Log and continue to next provider (404 is expected when
                        // signing_address doesn't match)
                        tracing::debug!(
                            model = %model,
                            error = %e,
                            "Provider returned error for attestation request, continuing to next provider"
                        );
                    }
                }
            }
        }

        if model_attestations.is_empty() {
            return Err(AttestationError::ProviderNotFound(model));
        }

        Ok(model_attestations)
    }

    pub async fn models(&self) -> Result<ModelsResponse, ListModelsError> {
        self.discover_models().await
    }

    pub async fn chat_completion_stream(
        &self,
        mut params: ChatCompletionParams,
        request_hash: String,
    ) -> Result<StreamingResult, CompletionError> {
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
                |provider| {
                    let params = params_for_provider.clone();
                    let request_hash = request_hash.clone();
                    async move { provider.chat_completion_stream(params, request_hash).await }
                },
            )
            .await?;

        // Store chat_id mapping for sticky routing by peeking at the first event
        // Must be synchronous to ensure attestation service can find the provider
        let mut peekable = StreamingResultExt::peekable(stream);
        if let Some(Ok(event)) = peekable.peek().await {
            if let inference_providers::StreamChunk::Chat(chat_chunk) = &event.chunk {
                let chat_id = chat_chunk.id.clone();
                tracing::info!(
                    chat_id = %chat_id,
                    "Storing chat_id mapping for streaming completion"
                );
                self.store_chat_id_mapping(chat_id, provider).await;
            }
        }
        Ok(Box::pin(peekable))
    }

    pub async fn chat_completion(
        &self,
        mut params: ChatCompletionParams,
        request_hash: String,
    ) -> Result<inference_providers::ChatCompletionResponseWithBytes, CompletionError> {
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
            .retry_with_fallback(&model_id, "chat_completion", model_pub_key, |provider| {
                let params = params_for_provider.clone();
                let request_hash = request_hash.clone();
                async move { provider.chat_completion(params, request_hash).await }
            })
            .await?;

        // Store the chat_id mapping SYNCHRONOUSLY before returning
        // This ensures the attestation service can find the provider
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

        Ok(response)
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
            .retry_with_fallback(&model_id, "image_generation", model_pub_key, |provider| {
                let params = cloned_params.clone();
                let request_hash = request_hash.clone();
                async move {
                    provider
                        .image_generation(params, request_hash)
                        .await
                        .map_err(|e| CompletionError::CompletionError(e.to_string()))
                }
            })
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
            .retry_with_fallback(&model_id, "audio_transcription", None, |provider| {
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
            .retry_with_fallback(&model_id, "image_edit", None, |provider| {
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

        // Ensure vLLM models are discovered
        self.ensure_models_discovered().await.map_err(|e| {
            RerankError::GenerationError(Self::sanitize_error_message(&e.to_string()))
        })?;

        // Get vLLM providers with load balancing
        let providers = match self.get_providers_with_fallback(&model_id, None).await {
            Some(p) => p,
            None => {
                let mappings = self.provider_mappings.read().await;
                let available_models: Vec<_> = mappings.model_to_providers.keys().collect();

                tracing::error!(
                    model_id = %model_id,
                    available_models = ?available_models,
                    operation = "rerank",
                    "No vLLM provider found for model"
                );

                return Err(RerankError::GenerationError(format!(
                    "Model '{}' not found in vLLM providers. Available models: {:?}",
                    model_id, available_models
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

    pub async fn score(
        &self,
        params: inference_providers::ScoreParams,
        request_hash: String,
    ) -> Result<inference_providers::ScoreResponse, inference_providers::ScoreError> {
        let model_id = params.model.clone();

        tracing::debug!(
            model = %model_id,
            "Starting score request"
        );

        // Check if this model exists as an external provider - scoring is not supported for external models
        if self.get_external_provider(&model_id).await.is_some() {
            return Err(inference_providers::ScoreError::GenerationError(format!(
                "Scoring is not supported for external provider models. \
                 Model '{}' is configured as an external provider. \
                 Scoring is only available for vLLM providers.",
                model_id
            )));
        }

        // Ensure vLLM models are discovered
        self.ensure_models_discovered().await.map_err(|e| {
            inference_providers::ScoreError::GenerationError(Self::sanitize_error_message(
                &e.to_string(),
            ))
        })?;

        // Get vLLM providers with load balancing
        let providers = match self.get_providers_with_fallback(&model_id, None).await {
            Some(p) => p,
            None => {
                let mappings = self.provider_mappings.read().await;
                let available_models: Vec<_> = mappings.model_to_providers.keys().collect();

                tracing::error!(
                    model_id = %model_id,
                    available_models = ?available_models,
                    operation = "score",
                    "No vLLM provider found for model"
                );

                return Err(inference_providers::ScoreError::GenerationError(format!(
                    "Model '{}' not found in vLLM providers. Available models: {:?}",
                    model_id, available_models
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
        let config: ProviderConfig = serde_json::from_value(provider_config)
            .map_err(|e| format!("Failed to parse provider config: {e}"))?;

        let backend_type = match &config {
            ProviderConfig::OpenAiCompatible { .. } => "openai_compatible".to_string(),
            ProviderConfig::Anthropic { .. } => "anthropic".to_string(),
            ProviderConfig::Gemini { .. } => "gemini".to_string(),
        };

        let api_key = self
            .external_configs
            .get_api_key(&backend_type)
            .ok_or_else(|| {
                format!(
                    "No API key configured for backend type '{}'. \
                     Set the appropriate environment variable (e.g., OPENAI_API_KEY, ANTHROPIC_API_KEY, GEMINI_API_KEY)",
                    backend_type
                )
            })?
            .to_string();

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

    /// Atomically replace all external providers with a new set built from the given models.
    ///
    /// This is safe for in-flight requests because existing `Arc` references remain valid
    /// until dropped. New requests will use the updated provider map.
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
                Err(e) => {
                    warn!(
                        model = %model_name,
                        error = %e,
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

    /// Start a periodic background task that refreshes external providers from a data source.
    ///
    /// The first tick is skipped because providers are already loaded at startup.
    /// If `refresh_interval_secs` is 0, this is a no-op.
    pub async fn start_external_refresh_task(
        self: Arc<Self>,
        source: Arc<dyn ExternalModelsSource>,
        refresh_interval_secs: u64,
    ) {
        if refresh_interval_secs == 0 {
            debug!("External provider refresh disabled (interval is 0)");
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
                    debug!("Running periodic external provider refresh");
                    match source.fetch_external_models().await {
                        Ok(models) => {
                            pool.sync_external_providers(models).await;
                        }
                        Err(e) => {
                            warn!(
                                error = %e,
                                "Failed to fetch external models for refresh, will retry on next interval"
                            );
                        }
                    }
                }
            }
        });

        let mut task_handle = self.external_refresh_task_handle.lock().await;
        *task_handle = Some(handle);
        info!(
            "External provider refresh task started with interval: {} seconds",
            refresh_interval_secs
        );
    }

    /// Start the periodic model discovery refresh task and store the handle
    pub async fn start_refresh_task(self: Arc<Self>, refresh_interval_secs: u64) {
        let handle = tokio::spawn({
            let pool = self.clone();
            async move {
                let mut interval =
                    tokio::time::interval(tokio::time::Duration::from_secs(refresh_interval_secs));
                loop {
                    interval.tick().await;
                    debug!("Running periodic model discovery refresh");
                    // Re-run model discovery
                    if pool.initialize().await.is_err() {
                        info!("Failed to refresh model discovery, will retry on next interval");
                    }
                }
            }
        });

        let mut task_handle = self.refresh_task_handle.lock().await;
        *task_handle = Some(handle);
        info!(
            "Model discovery refresh task started with interval: {} seconds",
            refresh_interval_secs
        );
    }

    /// Shutdown the inference provider pool and cleanup all resources
    pub async fn shutdown(&self) {
        info!("Initiating inference provider pool shutdown");

        // Step 1a: Cancel the model discovery refresh task
        debug!("Step 1a: Cancelling model discovery refresh task");
        let mut task_handle = self.refresh_task_handle.lock().await;
        if let Some(handle) = task_handle.take() {
            debug!("Cancelling model discovery refresh task");
            handle.abort();
            info!("Model discovery refresh task cancelled successfully");
        } else {
            debug!("No active refresh task to cancel");
        }
        drop(task_handle); // Explicitly drop the lock

        // Step 1b: Cancel the external provider refresh task
        debug!("Step 1b: Cancelling external provider refresh task");
        let mut ext_task_handle = self.external_refresh_task_handle.lock().await;
        if let Some(handle) = ext_task_handle.take() {
            debug!("Cancelling external provider refresh task");
            handle.abort();
            info!("External provider refresh task cancelled successfully");
        } else {
            debug!("No active external refresh task to cancel");
        }
        drop(ext_task_handle);

        // Step 2: Clear provider mappings (both model and pubkey mappings cleared atomically)
        debug!("Step 2: Clearing provider mappings");
        let mut mappings = self.provider_mappings.write().await;
        let model_count = mappings.model_to_providers.len();
        let pubkey_count = mappings.pubkey_to_providers.len();
        *mappings = ProviderMappings::new();
        debug!(
            "Cleared {} model mappings and {} pubkey mappings",
            model_count, pubkey_count
        );
        drop(mappings);

        // Step 2.5: Clear external providers
        debug!("Step 2.5: Clearing external providers");
        let mut external = self.external_providers.write().await;
        let external_count = external.len();
        external.clear();
        debug!("Cleared {} external providers", external_count);
        drop(external);

        // Step 3: Clear load balancer indices
        debug!("Step 3: Clearing load balancer indices");
        let mut lb_index = self.load_balancer_index.write().await;
        let index_count = lb_index.len();
        lb_index.clear();
        debug!("Cleared {} load balancer indices", index_count);
        drop(lb_index);

        // Step 4: Clear chat_id to provider mappings
        debug!("Step 4: Clearing chat session mappings");
        let mut chat_mapping = self.chat_id_mapping.write().await;
        let chat_count = chat_mapping.len();
        chat_mapping.clear();
        debug!("Cleared {} chat session mappings", chat_count);
        drop(chat_mapping);

        info!(
            "Inference provider pool shutdown completed. Cleaned up: {} models, {} pubkeys, {} external providers, {} load balancer indices, {} chat mappings",
            model_count, pubkey_count, external_count, index_count, chat_count
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_ip_port() {
        // Valid IP-based keys
        assert_eq!(
            InferenceProviderPool::parse_ip_port("192.0.2.1:8000"),
            Some(("192.0.2.1".to_string(), 8000))
        );
        assert_eq!(
            InferenceProviderPool::parse_ip_port("192.0.2.2:8001"),
            Some(("192.0.2.2".to_string(), 8001))
        );

        // Invalid keys (should be filtered out)
        assert_eq!(
            InferenceProviderPool::parse_ip_port("redpill:phala/qwen-2.5-7b-instruct"),
            None
        );
        assert_eq!(InferenceProviderPool::parse_ip_port("invalid"), None);
        assert_eq!(InferenceProviderPool::parse_ip_port("not-an-ip:8000"), None);
        assert_eq!(
            InferenceProviderPool::parse_ip_port("256.256.256.256:8000"),
            None
        ); // Invalid IP
        assert_eq!(
            InferenceProviderPool::parse_ip_port("192.168.1.1:70000"),
            None
        ); // Invalid port
    }

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

        let pool = InferenceProviderPool::new(
            "http://localhost:8080/models".to_string(),
            None,
            5,
            ExternalProvidersConfig::default(),
        );

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

    // ==================== External Provider Tests ====================

    #[tokio::test]
    async fn test_register_external_provider_openai() {
        let pool = InferenceProviderPool::new(
            "http://localhost:8080/models".to_string(),
            None,
            5,
            ExternalProvidersConfig {
                openai_api_key: Some("sk-test-key".to_string()),
                anthropic_api_key: None,
                gemini_api_key: None,
                timeout_seconds: 60,
                refresh_interval_secs: 0,
            },
        );

        let config = serde_json::json!({
            "backend": "openai_compatible",
            "base_url": "https://api.openai.com/v1"
        });

        let result = pool
            .register_external_provider("gpt-4".to_string(), config)
            .await;

        assert!(result.is_ok());
        assert!(pool.is_external_provider("gpt-4").await);
    }

    #[tokio::test]
    async fn test_register_external_provider_anthropic() {
        let pool = InferenceProviderPool::new(
            "http://localhost:8080/models".to_string(),
            None,
            5,
            ExternalProvidersConfig {
                openai_api_key: None,
                anthropic_api_key: Some("sk-ant-test".to_string()),
                gemini_api_key: None,
                timeout_seconds: 60,
                refresh_interval_secs: 0,
            },
        );

        let config = serde_json::json!({
            "backend": "anthropic",
            "base_url": "https://api.anthropic.com/v1"
        });

        let result = pool
            .register_external_provider("claude-3-opus".to_string(), config)
            .await;

        assert!(result.is_ok());
        assert!(pool.is_external_provider("claude-3-opus").await);
    }

    #[tokio::test]
    async fn test_register_external_provider_gemini() {
        let pool = InferenceProviderPool::new(
            "http://localhost:8080/models".to_string(),
            None,
            5,
            ExternalProvidersConfig {
                openai_api_key: None,
                anthropic_api_key: None,
                gemini_api_key: Some("AIza-test".to_string()),
                timeout_seconds: 60,
                refresh_interval_secs: 0,
            },
        );

        let config = serde_json::json!({
            "backend": "gemini",
            "base_url": "https://generativelanguage.googleapis.com/v1beta"
        });

        let result = pool
            .register_external_provider("gemini-1.5-pro".to_string(), config)
            .await;

        assert!(result.is_ok());
        assert!(pool.is_external_provider("gemini-1.5-pro").await);
    }

    #[tokio::test]
    async fn test_register_external_provider_missing_api_key() {
        let pool = InferenceProviderPool::new(
            "http://localhost:8080/models".to_string(),
            None,
            5,
            ExternalProvidersConfig::default(), // No API keys configured
        );

        let config = serde_json::json!({
            "backend": "openai_compatible",
            "base_url": "https://api.openai.com/v1"
        });

        let result = pool
            .register_external_provider("gpt-4".to_string(), config)
            .await;

        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(err.contains("API key"));
    }

    #[tokio::test]
    async fn test_register_external_provider_invalid_config() {
        let pool = InferenceProviderPool::new(
            "http://localhost:8080/models".to_string(),
            None,
            5,
            ExternalProvidersConfig {
                openai_api_key: Some("sk-test".to_string()),
                anthropic_api_key: None,
                gemini_api_key: None,
                timeout_seconds: 60,
                refresh_interval_secs: 0,
            },
        );

        let config = serde_json::json!({
            "backend": "unknown_backend",
            "base_url": "https://example.com"
        });

        let result = pool
            .register_external_provider("test-model".to_string(), config)
            .await;

        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_is_external_provider_false_for_vllm() {
        use inference_providers::mock::MockProvider;

        let pool = InferenceProviderPool::new(
            "http://localhost:8080/models".to_string(),
            None,
            5,
            ExternalProvidersConfig::default(),
        );

        // Register a vLLM-style provider (via mock)
        let mock_provider = Arc::new(MockProvider::new());
        pool.register_provider("vllm-model".to_string(), mock_provider)
            .await;

        // vLLM providers should not be external
        assert!(!pool.is_external_provider("vllm-model").await);
    }

    #[tokio::test]
    async fn test_is_external_provider_false_for_unknown() {
        let pool = InferenceProviderPool::new(
            "http://localhost:8080/models".to_string(),
            None,
            5,
            ExternalProvidersConfig::default(),
        );

        // Unknown model should not be external
        assert!(!pool.is_external_provider("unknown-model").await);
    }

    #[tokio::test]
    async fn test_load_external_providers_batch() {
        let pool = InferenceProviderPool::new(
            "http://localhost:8080/models".to_string(),
            None,
            5,
            ExternalProvidersConfig {
                openai_api_key: Some("sk-test".to_string()),
                anthropic_api_key: Some("sk-ant-test".to_string()),
                gemini_api_key: Some("AIza-test".to_string()),
                timeout_seconds: 60,
                refresh_interval_secs: 0,
            },
        );

        let models = vec![
            (
                "gpt-4".to_string(),
                serde_json::json!({
                    "backend": "openai_compatible",
                    "base_url": "https://api.openai.com/v1"
                }),
            ),
            (
                "claude-3".to_string(),
                serde_json::json!({
                    "backend": "anthropic",
                    "base_url": "https://api.anthropic.com/v1"
                }),
            ),
            (
                "gemini-pro".to_string(),
                serde_json::json!({
                    "backend": "gemini",
                    "base_url": "https://generativelanguage.googleapis.com/v1beta"
                }),
            ),
        ];

        let result = pool.load_external_providers(models).await;

        assert!(result.is_ok());
        assert!(pool.is_external_provider("gpt-4").await);
        assert!(pool.is_external_provider("claude-3").await);
        assert!(pool.is_external_provider("gemini-pro").await);
    }

    #[tokio::test]
    async fn test_load_external_providers_partial_failure() {
        let pool = InferenceProviderPool::new(
            "http://localhost:8080/models".to_string(),
            None,
            5,
            ExternalProvidersConfig {
                openai_api_key: Some("sk-test".to_string()),
                anthropic_api_key: None, // Missing Anthropic key
                gemini_api_key: None,
                timeout_seconds: 60,
                refresh_interval_secs: 0,
            },
        );

        let models = vec![
            (
                "gpt-4".to_string(),
                serde_json::json!({
                    "backend": "openai_compatible",
                    "base_url": "https://api.openai.com/v1"
                }),
            ),
            (
                "claude-3".to_string(),
                serde_json::json!({
                    "backend": "anthropic",
                    "base_url": "https://api.anthropic.com/v1"
                }),
            ),
        ];

        // Should still succeed (continues on individual failures)
        let result = pool.load_external_providers(models).await;
        assert!(result.is_ok());

        // OpenAI should be registered
        assert!(pool.is_external_provider("gpt-4").await);

        // Anthropic should fail to register (missing API key)
        assert!(!pool.is_external_provider("claude-3").await);
    }

    #[tokio::test]
    async fn test_external_provider_config_from_env() {
        // Test the ExternalProvidersConfig::from_env() behavior
        let config = ExternalProvidersConfig::default();

        // Default should have no API keys
        assert!(config.openai_api_key.is_none());
        assert!(config.anthropic_api_key.is_none());
        assert!(config.gemini_api_key.is_none());
    }

    #[tokio::test]
    async fn test_external_provider_config_get_api_key() {
        let config = ExternalProvidersConfig {
            openai_api_key: Some("openai-key".to_string()),
            anthropic_api_key: Some("anthropic-key".to_string()),
            gemini_api_key: Some("gemini-key".to_string()),
            timeout_seconds: 60,
            refresh_interval_secs: 0,
        };

        assert_eq!(config.get_api_key("openai_compatible"), Some("openai-key"));
        assert_eq!(config.get_api_key("anthropic"), Some("anthropic-key"));
        assert_eq!(config.get_api_key("gemini"), Some("gemini-key"));
        assert_eq!(config.get_api_key("unknown"), None);
    }

    #[tokio::test]
    async fn test_shutdown_clears_external_providers() {
        let pool = InferenceProviderPool::new(
            "http://localhost:8080/models".to_string(),
            None,
            5,
            ExternalProvidersConfig {
                openai_api_key: Some("sk-test".to_string()),
                anthropic_api_key: None,
                gemini_api_key: None,
                timeout_seconds: 60,
                refresh_interval_secs: 0,
            },
        );

        let config = serde_json::json!({
            "backend": "openai_compatible",
            "base_url": "https://api.openai.com/v1"
        });

        pool.register_external_provider("gpt-4".to_string(), config)
            .await
            .unwrap();

        assert!(pool.is_external_provider("gpt-4").await);

        pool.shutdown().await;

        assert!(!pool.is_external_provider("gpt-4").await);
    }

    #[tokio::test]
    async fn test_unregister_external_provider() {
        let pool = InferenceProviderPool::new(
            "http://localhost:8080/models".to_string(),
            None,
            5,
            ExternalProvidersConfig {
                openai_api_key: Some("sk-test".to_string()),
                anthropic_api_key: None,
                gemini_api_key: None,
                timeout_seconds: 60,
                refresh_interval_secs: 0,
            },
        );

        let config = serde_json::json!({
            "backend": "openai_compatible",
            "base_url": "https://api.openai.com/v1"
        });

        // Register a provider
        pool.register_external_provider("gpt-4".to_string(), config)
            .await
            .unwrap();
        assert!(pool.is_external_provider("gpt-4").await);

        // Unregister it
        let removed = pool.unregister_external_provider("gpt-4").await;
        assert!(removed);
        assert!(!pool.is_external_provider("gpt-4").await);

        // Unregistering again should return false
        let removed_again = pool.unregister_external_provider("gpt-4").await;
        assert!(!removed_again);
    }

    #[tokio::test]
    async fn test_unregister_nonexistent_provider() {
        let pool = InferenceProviderPool::new(
            "http://localhost:8080/models".to_string(),
            None,
            5,
            ExternalProvidersConfig::default(),
        );

        // Unregistering a provider that was never registered should return false
        let removed = pool.unregister_external_provider("nonexistent-model").await;
        assert!(!removed);
    }

    #[tokio::test]
    async fn test_register_update_external_provider() {
        let pool = InferenceProviderPool::new(
            "http://localhost:8080/models".to_string(),
            None,
            5,
            ExternalProvidersConfig {
                openai_api_key: Some("sk-test".to_string()),
                anthropic_api_key: None,
                gemini_api_key: None,
                timeout_seconds: 60,
                refresh_interval_secs: 0,
            },
        );

        let config1 = serde_json::json!({
            "backend": "openai_compatible",
            "base_url": "https://api.openai.com/v1"
        });

        let config2 = serde_json::json!({
            "backend": "openai_compatible",
            "base_url": "https://api.together.xyz/v1"
        });

        // Register initial config
        pool.register_external_provider("my-model".to_string(), config1)
            .await
            .unwrap();
        assert!(pool.is_external_provider("my-model").await);

        // Re-register with updated config (should overwrite)
        pool.register_external_provider("my-model".to_string(), config2)
            .await
            .unwrap();
        assert!(pool.is_external_provider("my-model").await);

        // Should still only have one entry (not duplicated)
        let external = pool.external_providers.read().await;
        assert_eq!(external.len(), 1);
    }

    // ==================== Sync External Providers Tests ====================

    #[tokio::test]
    async fn test_sync_replaces_old_providers() {
        let pool = InferenceProviderPool::new(
            "http://localhost:8080/models".to_string(),
            None,
            5,
            ExternalProvidersConfig {
                openai_api_key: Some("sk-test".to_string()),
                anthropic_api_key: Some("sk-ant-test".to_string()),
                gemini_api_key: None,
                timeout_seconds: 60,
                refresh_interval_secs: 0,
            },
        );

        // Load initial providers via register
        pool.register_external_provider(
            "gpt-4".to_string(),
            serde_json::json!({
                "backend": "openai_compatible",
                "base_url": "https://api.openai.com/v1"
            }),
        )
        .await
        .unwrap();
        pool.register_external_provider(
            "claude-3".to_string(),
            serde_json::json!({
                "backend": "anthropic",
                "base_url": "https://api.anthropic.com/v1"
            }),
        )
        .await
        .unwrap();

        assert!(pool.is_external_provider("gpt-4").await);
        assert!(pool.is_external_provider("claude-3").await);

        // Sync with a new set that removes claude-3 and adds gpt-4o
        let new_models = vec![
            (
                "gpt-4".to_string(),
                serde_json::json!({
                    "backend": "openai_compatible",
                    "base_url": "https://api.openai.com/v1"
                }),
            ),
            (
                "gpt-4o".to_string(),
                serde_json::json!({
                    "backend": "openai_compatible",
                    "base_url": "https://api.openai.com/v1"
                }),
            ),
        ];

        pool.sync_external_providers(new_models).await;

        // gpt-4 should still exist
        assert!(pool.is_external_provider("gpt-4").await);
        // gpt-4o should be added
        assert!(pool.is_external_provider("gpt-4o").await);
        // claude-3 should be gone (removed from DB)
        assert!(!pool.is_external_provider("claude-3").await);

        let external = pool.external_providers.read().await;
        assert_eq!(external.len(), 2);
    }

    #[tokio::test]
    async fn test_sync_with_empty_list_clears_providers() {
        let pool = InferenceProviderPool::new(
            "http://localhost:8080/models".to_string(),
            None,
            5,
            ExternalProvidersConfig {
                openai_api_key: Some("sk-test".to_string()),
                anthropic_api_key: None,
                gemini_api_key: None,
                timeout_seconds: 60,
                refresh_interval_secs: 0,
            },
        );

        // Load an initial provider
        pool.register_external_provider(
            "gpt-4".to_string(),
            serde_json::json!({
                "backend": "openai_compatible",
                "base_url": "https://api.openai.com/v1"
            }),
        )
        .await
        .unwrap();
        assert!(pool.is_external_provider("gpt-4").await);

        // Sync with empty list should clear all external providers
        pool.sync_external_providers(vec![]).await;

        assert!(!pool.is_external_provider("gpt-4").await);
        let external = pool.external_providers.read().await;
        assert_eq!(external.len(), 0);
    }

    #[tokio::test]
    async fn test_sync_with_partial_failures_keeps_successful() {
        let pool = InferenceProviderPool::new(
            "http://localhost:8080/models".to_string(),
            None,
            5,
            ExternalProvidersConfig {
                openai_api_key: Some("sk-test".to_string()),
                anthropic_api_key: None, // Missing Anthropic key
                gemini_api_key: None,
                timeout_seconds: 60,
                refresh_interval_secs: 0,
            },
        );

        let models = vec![
            (
                "gpt-4".to_string(),
                serde_json::json!({
                    "backend": "openai_compatible",
                    "base_url": "https://api.openai.com/v1"
                }),
            ),
            (
                "claude-3".to_string(),
                serde_json::json!({
                    "backend": "anthropic",
                    "base_url": "https://api.anthropic.com/v1"
                }),
            ),
        ];

        pool.sync_external_providers(models).await;

        // gpt-4 should succeed (has OpenAI key)
        assert!(pool.is_external_provider("gpt-4").await);
        // claude-3 should fail (no Anthropic key)
        assert!(!pool.is_external_provider("claude-3").await);

        let external = pool.external_providers.read().await;
        assert_eq!(external.len(), 1);
    }
}
