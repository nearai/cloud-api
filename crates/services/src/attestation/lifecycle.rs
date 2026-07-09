use std::sync::Arc;

use config::ItaAttestationConfig;

use super::{
    environment::{load_tls_cert_fingerprint, load_vpc_info, load_vpc_shared_secret},
    ita::{ita_client_error_class, ItaClient, ProviderPoolModelAttestationCollector},
    AttestationError, AttestationRepository, AttestationService, DstackGatewayQuoteCollector,
    InferenceProviderPool, MetricsServiceTrait, ModelsRepository, UsageRepository,
};

/// Default TTL (seconds) for the no-nonce attestation-report cache. Reports for
/// nonce-less requests are short-lived snapshots; ~10s collapses the bursty
/// monitoring herd (bursts arrive within seconds) while keeping reports fresh.
/// Override with `ATTESTATION_REPORT_CACHE_TTL_SECS`; `0` disables the cache.
const DEFAULT_ATTESTATION_REPORT_CACHE_TTL_SECS: u64 = 10;

impl AttestationService {
    pub async fn init(
        repository: Arc<dyn AttestationRepository + Send + Sync>,
        inference_provider_pool: Arc<InferenceProviderPool>,
        models_repository: Arc<dyn ModelsRepository>,
        metrics_service: Arc<dyn MetricsServiceTrait>,
        usage_repository: Arc<dyn UsageRepository>,
        ita_config: ItaAttestationConfig,
    ) -> Result<Self, AttestationError> {
        #[cfg(not(debug_assertions))]
        if std::env::var("DEV").is_ok() {
            tracing::error!(
                "SECURITY: DEV environment variable is set in a release build. \
                 DEV mode is not available in release builds and will be ignored. \
                 Remove the DEV variable from your environment."
            );
        }

        let vpc_info = load_vpc_info();
        let vpc_shared_secret = load_vpc_shared_secret();
        let tls_cert_fingerprint = load_tls_cert_fingerprint();
        if vpc_shared_secret.is_none() {
            tracing::warn!(
                "Cannot load VPC shared secret. VPC-based authentication will be disabled"
            );
        }

        let (ed25519_signing_key, ed25519_verifying_key, ecdsa_signing_key, ecdsa_verifying_key) =
            match Self::derive_signing_keys_from_dstack().await {
                Ok(keys) => keys,
                Err(e) => {
                    #[cfg(debug_assertions)]
                    {
                        if std::env::var("DEV").is_ok() {
                            tracing::warn!(
                                "DEV mode: Unable to derive signing keys from dstack ({}); falling back to ephemeral keys",
                                e
                            );
                            Self::generate_ephemeral_signing_keys()
                        } else {
                            tracing::error!(
                                "Failed to derive signing keys from dstack ({}). \
                                 This service must run in a CVM/TEE with dstack available.",
                                e
                            );
                            return Err(AttestationError::InternalError(format!(
                                "Failed to derive signing keys from dstack: {}. \
                                 Ensure this service runs in a CVM/TEE with dstack available.",
                                e
                            )));
                        }
                    }
                    #[cfg(not(debug_assertions))]
                    {
                        tracing::error!(
                            "Failed to derive signing keys from dstack ({}). \
                             This service must run in a CVM/TEE with dstack available.",
                            e
                        );
                        return Err(AttestationError::InternalError(format!(
                            "Failed to derive signing keys from dstack: {}. \
                             Ensure this service runs in a CVM/TEE with dstack available.",
                            e
                        )));
                    }
                }
            };

        let ita_client = if ita_config.enabled {
            match ItaClient::from_config(&ita_config) {
                Ok(client) => Some(client),
                Err(error) => {
                    tracing::warn!(
                        error = %ita_client_error_class(&error),
                        "ITA attestation client is not available"
                    );
                    None
                }
            }
        } else {
            None
        };
        let model_attestation_collector = Arc::new(ProviderPoolModelAttestationCollector::new(
            inference_provider_pool.clone(),
        ));

        // Build the no-nonce attestation-report cache from env (TTL=0 disables it).
        let cache_ttl_secs = std::env::var("ATTESTATION_REPORT_CACHE_TTL_SECS")
            .ok()
            .and_then(|s| s.parse::<u64>().ok())
            .unwrap_or(DEFAULT_ATTESTATION_REPORT_CACHE_TTL_SECS);
        let report_cache = if cache_ttl_secs == 0 {
            tracing::info!("Attestation-report cache disabled (TTL=0)");
            None
        } else {
            tracing::info!(
                ttl_secs = cache_ttl_secs,
                "Attestation-report cache enabled"
            );
            Some(
                moka::future::Cache::builder()
                    // One entry per (model, algo, tls_fp, provider, signing_addr)
                    // combination; the live model catalog is well under this.
                    .max_capacity(1024)
                    .time_to_live(std::time::Duration::from_secs(cache_ttl_secs))
                    .build(),
            )
        };

        Ok(Self {
            repository,
            inference_provider_pool,
            models_repository,
            metrics_service,
            usage_repository,
            vpc_info,
            vpc_shared_secret,
            tls_cert_fingerprint,
            ed25519_signing_key: Arc::new(ed25519_signing_key),
            ed25519_verifying_key: Arc::new(ed25519_verifying_key),
            ecdsa_signing_key: Arc::new(ecdsa_signing_key),
            ecdsa_verifying_key: Arc::new(ecdsa_verifying_key),
            ita_config,
            ita_client,
            gateway_quote_collector: Arc::new(DstackGatewayQuoteCollector),
            model_attestation_collector,
            report_cache,
        })
    }
}
