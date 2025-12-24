use crate::common::encryption_headers;
use inference_providers::{
    models::{AttestationError, CompletionError, ListModelsError, ModelsResponse},
    ChatCompletionParams, InferenceProvider, StreamingResult, StreamingResultExt, VLlmConfig,
    VLlmProvider,
};
use regex::Regex;
use serde::Deserialize;
use std::{collections::HashMap, net::IpAddr, sync::Arc, time::Duration};
use tokio::sync::{Mutex, RwLock};
use tracing::{debug, info};

type InferenceProviderTrait = dyn InferenceProvider + Send + Sync;

/// Discovery entry returned by the discovery layer
#[derive(Debug, Clone, Deserialize)]
struct DiscoveryEntry {
    /// Model identifier (e.g., "Qwen/Qwen3-30B-A3B-Instruct-2507")
    model: String,
    /// Tags for filtering/routing (e.g., ["prod", "dev"])
    #[serde(default)]
    tags: Vec<String>,
}

#[derive(Clone)]
pub struct InferenceProviderPool {
    /// Discovery URL for dynamic model discovery
    discovery_url: String,
    /// Optional API key for authenticating with discovered providers
    api_key: Option<String>,
    /// HTTP timeout for discovery requests
    discovery_timeout: Duration,
    /// HTTP timeout for model inference requests
    inference_timeout_secs: i64,
    /// Map of model name -> list of providers (for load balancing)
    model_mapping: Arc<RwLock<HashMap<String, Vec<Arc<InferenceProviderTrait>>>>>,
    /// Round-robin index for each model
    load_balancer_index: Arc<RwLock<HashMap<String, usize>>>,
    /// Map of chat_id -> provider for sticky routing
    chat_id_mapping: Arc<RwLock<HashMap<String, Arc<InferenceProviderTrait>>>>,
    /// Map of chat_id -> (request_hash, response_hash) for MockProvider signature generation
    signature_hashes: Arc<RwLock<HashMap<String, (String, String)>>>,
    /// Map of model signing public key -> provider for routing by model public key
    model_pub_key_mapping: Arc<RwLock<HashMap<String, Arc<InferenceProviderTrait>>>>,
    /// Background task handle for periodic model discovery refresh
    refresh_task_handle: Arc<Mutex<Option<tokio::task::JoinHandle<()>>>>,
}

impl InferenceProviderPool {
    /// Create a new pool with discovery URL and optional API key
    pub fn new(
        discovery_url: String,
        api_key: Option<String>,
        discovery_timeout_secs: i64,
        inference_timeout_secs: i64,
    ) -> Self {
        Self {
            discovery_url,
            api_key,
            discovery_timeout: Duration::from_secs(discovery_timeout_secs as u64),
            inference_timeout_secs,
            model_mapping: Arc::new(RwLock::new(HashMap::new())),
            load_balancer_index: Arc::new(RwLock::new(HashMap::new())),
            chat_id_mapping: Arc::new(RwLock::new(HashMap::new())),
            signature_hashes: Arc::new(RwLock::new(HashMap::new())),
            model_pub_key_mapping: Arc::new(RwLock::new(HashMap::new())),
            refresh_task_handle: Arc::new(Mutex::new(None)),
        }
    }

    /// Register a provider for a model manually (useful for testing with mock providers)
    /// Also populates model_pub_key_mapping by fetching the attestation report
    /// Fetches attestation reports for both ECDSA and Ed25519 to support both signing algorithms
    pub async fn register_provider(&self, model_id: String, provider: Arc<InferenceProviderTrait>) {
        // Fetch signing public keys for both algorithms
        // Use "mock" as URL identifier for logging (since this is typically used for mock providers)
        let (pub_key_updates, _has_valid_attestation) =
            Self::fetch_signing_public_keys_for_both_algorithms(&provider, &model_id, "mock").await;

        // Update model_pub_key_mapping
        {
            let mut model_pub_key_mapping = self.model_pub_key_mapping.write().await;
            for (key, provider) in pub_key_updates {
                model_pub_key_mapping.insert(key, provider);
            }
        }

        // Replace existing providers for this model_id (consistent with discover_models behavior)
        let mut model_mapping = self.model_mapping.write().await;
        model_mapping.insert(model_id, vec![provider]);
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

        // Phase 2: Bulk update mappings under lock (minimal lock duration)
        // Replace existing providers for each model_id (consistent with discover_models behavior)
        {
            let mut model_mapping = self.model_mapping.write().await;
            for (model_id, providers) in model_providers {
                model_mapping.insert(model_id, providers);
            }
        }

        {
            let mut model_pub_key_mapping = self.model_pub_key_mapping.write().await;
            for (key, provider) in pub_key_updates {
                model_pub_key_mapping.insert(key, provider);
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
        let model_mapping = self.model_mapping.read().await;

        // If mapping is empty, we need to discover models
        if model_mapping.is_empty() {
            drop(model_mapping); // Release read lock
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
                    Some(self.inference_timeout_secs),
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

        // Phase 2: Bulk update mappings under lock (minimal lock duration)
        {
            let mut model_mapping = self.model_mapping.write().await;
            model_mapping.clear();
            for (model_name, providers) in model_providers {
                model_mapping.insert(model_name, providers);
            }
        }

        {
            let mut model_pub_key_mapping = self.model_pub_key_mapping.write().await;
            model_pub_key_mapping.clear();
            for (key, provider) in model_pub_key_updates {
                model_pub_key_mapping.insert(key, provider);
            }
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
        let model_mapping = self.model_mapping.read().await;
        model_mapping.get(model_id).cloned()
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

    /// Register signature hashes for a chat_id
    /// MockProvider will check this when get_signature is called
    pub async fn register_signature_hashes_for_chat(
        &self,
        chat_id: &str,
        request_hash: String,
        response_hash: String,
    ) {
        let mut hashes = self.signature_hashes.write().await;
        hashes.insert(chat_id.to_string(), (request_hash, response_hash));
        tracing::debug!("Registered signature hashes for chat_id: {}", chat_id);
    }

    /// Get signature hashes for a chat_id (used by MockProvider)
    pub async fn get_signature_hashes_for_chat(&self, chat_id: &str) -> Option<(String, String)> {
        let hashes = self.signature_hashes.read().await;
        hashes.get(chat_id).cloned()
    }

    /// Lookup provider by model signing public key
    /// Used when X-Model-Pub-Key header is provided to route to a specific model provider
    pub async fn get_provider_by_model_pub_key(
        &self,
        model_pub_key: &str,
    ) -> Option<Arc<InferenceProviderTrait>> {
        let mapping = self.model_pub_key_mapping.read().await;
        mapping.get(model_pub_key).cloned()
    }

    /// Get providers for a model in priority order for fallback
    /// Returns providers with the round-robin selected one first, followed by others
    async fn get_providers_with_fallback(
        &self,
        model_id: &str,
    ) -> Option<Vec<Arc<InferenceProviderTrait>>> {
        let model_mapping = self.model_mapping.read().await;
        let providers = model_mapping.get(model_id)?;

        if providers.is_empty() {
            return None;
        }

        if providers.len() == 1 {
            return Some(vec![providers[0].clone()]);
        }

        // Get current index for round-robin
        let mut indices = self.load_balancer_index.write().await;
        let index = indices.entry(model_id.to_string()).or_insert(0);
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
            model = %model_id,
            providers_count = providers.len(),
            selected_index = selected_index,
            "Prepared providers for fallback with round-robin priority"
        );

        Some(ordered_providers)
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
        // Ensure models are discovered first
        self.ensure_models_discovered().await?;

        // If model_pub_key is provided, route to the specific provider by signing public key.
        // This is required for encryption - the client expects a specific provider with a specific
        // public key. We must not fall back to model_id routing as that could route to a different
        // provider that doesn't support the expected encryption.
        if let Some(pub_key) = model_pub_key {
            tracing::debug!(
                model_id = %model_id,
                model_pub_key = %pub_key,
                operation = operation_name,
                "Attempting to get provider by model public key"
            );
            match self.get_provider_by_model_pub_key(pub_key).await {
                Some(provider) => match provider_fn(provider.clone()).await {
                    Ok(result) => {
                        return Ok((result, provider));
                    }
                    Err(e) => {
                        tracing::warn!(
                            model_id = %model_id,
                            model_pub_key = %pub_key,
                            operation = operation_name,
                            error = %e,
                            "Getting provider by model public key failed. Cannot fall back to model_id routing as encryption requirements must be met."
                        );
                        return Err(CompletionError::CompletionError(format!(
                            "Getting provider for model public key '{}...' failed: {}. Encryption requires routing to the specific provider with this public key.",
                            pub_key.chars().take(32).collect::<String>(),
                            e
                        )));
                    }
                },
                None => {
                    tracing::warn!(
                        model_id = %model_id,
                        model_pub_key = %pub_key,
                        operation = operation_name,
                        "No provider found for model public key. Cannot fall back to model_id routing as encryption requirements must be met."
                    );
                    return Err(CompletionError::CompletionError(format!(
                        "No provider found for model public key '{}...'. Encryption requires routing to the specific provider with this public key.",
                        pub_key.chars().take(32).collect::<String>()
                    )));
                }
            }
        }

        // Get all providers with fallback priority
        let providers = match self.get_providers_with_fallback(model_id).await {
            Some(p) => p,
            None => {
                let model_mapping = self.model_mapping.read().await;
                let available_models: Vec<_> = model_mapping.keys().collect();

                tracing::error!(
                    model_id = %model_id,
                    available_models = ?available_models,
                    operation = operation_name,
                    "Model not found in provider pool"
                );
                return Err(CompletionError::CompletionError(format!(
                    "Model '{model_id}' not found in any configured provider"
                )));
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

        // Collect sanitized errors for user-facing message
        let mut sanitized_errors = Vec::new();
        // Keep detailed errors for logging only
        let mut detailed_errors = Vec::new();

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
                    let error_str = e.to_string();

                    // Log the full detailed error for debugging
                    tracing::warn!(
                        model_id = %model_id,
                        attempt = attempt + 1,
                        operation = operation_name,
                        "Provider failed, will try next provider if available"
                    );

                    // Store detailed error for logging
                    detailed_errors.push(format!("Provider {}: {}", attempt + 1, error_str));

                    // Store sanitized error for user-facing response
                    let sanitized = Self::sanitize_error_message(&error_str);
                    sanitized_errors.push(format!("Provider {}: {}", attempt + 1, sanitized));
                }
            }
        }

        // All providers failed - log detailed errors but return sanitized message to user
        // let detailed_error_msg = detailed_errors.join("; ");
        let sanitized_error_msg = sanitized_errors.join("; ");

        tracing::error!(
            model_id = %model_id,
            providers_tried = providers.len(),
            operation = operation_name,
            "All providers failed for model"
        );

        // Return sanitized error to user
        Err(CompletionError::CompletionError(format!(
            "All {} provider(s) failed for model '{}': {}",
            providers.len(),
            model_id,
            sanitized_error_msg
        )))
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
        let mut peekable = StreamingResultExt::peekable(stream);
        if let Some(Ok(event)) = peekable.peek().await {
            if let inference_providers::StreamChunk::Chat(chat_chunk) = &event.chunk {
                let chat_id = chat_chunk.id.clone();
                let pool = self.clone();
                tokio::spawn(async move {
                    tracing::info!(
                        chat_id = %chat_id,
                        "Storing chat_id mapping"
                    );
                    pool.store_chat_id_mapping(chat_id, provider).await;
                });
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

        // Step 1: Cancel the refresh task
        debug!("Step 1: Cancelling model discovery refresh task");
        let mut task_handle = self.refresh_task_handle.lock().await;
        if let Some(handle) = task_handle.take() {
            debug!("Cancelling model discovery refresh task");
            handle.abort();
            info!("Model discovery refresh task cancelled successfully");
        } else {
            debug!("No active refresh task to cancel");
        }
        drop(task_handle); // Explicitly drop the lock

        // Step 2: Clear model mappings and provider references
        debug!("Step 2: Clearing model mappings");
        let mut model_mapping = self.model_mapping.write().await;
        let model_count = model_mapping.len();
        model_mapping.clear();
        debug!("Cleared {} model mappings", model_count);
        drop(model_mapping);

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

        // Step 5: Clear signature hashes
        debug!("Step 5: Clearing signature hash tracking");
        let mut sig_hashes = self.signature_hashes.write().await;
        let sig_count = sig_hashes.len();
        sig_hashes.clear();
        debug!("Cleared {} signature hash entries", sig_count);
        drop(sig_hashes);

        // Step 6: Clear model public key mappings
        debug!("Step 6: Clearing model public key mappings");
        let mut model_pub_key_mapping = self.model_pub_key_mapping.write().await;
        let model_pub_key_count = model_pub_key_mapping.len();
        model_pub_key_mapping.clear();
        debug!("Cleared {} model public key mappings", model_pub_key_count);
        drop(model_pub_key_mapping);

        info!(
            "Inference provider pool shutdown completed. Cleaned up: {} models, {} load balancer indices, {} chat mappings, {} signatures, {} model public key mappings",
            model_count, index_count, chat_count, sig_count, model_pub_key_count
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
    }
}
