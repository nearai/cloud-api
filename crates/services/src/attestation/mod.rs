mod chat_signatures;
pub mod chutes;
mod environment;
mod gateway_quote;
mod gateway_signatures;
pub mod ita;
mod keys;
mod lifecycle;
pub mod measurement;
pub mod models;
pub mod ports;
mod report;
pub mod report_data;
mod service_trait;
pub mod verification;

use std::sync::Arc;

use config::ItaAttestationConfig;
use ed25519_dalek::{SigningKey, VerifyingKey};
use k256::ecdsa::{SigningKey as EcdsaSigningKey, VerifyingKey as EcdsaVerifyingKey};

pub use environment::{
    compute_spki_hash, load_tls_cert_fingerprint, load_vpc_info, load_vpc_shared_secret,
};
pub use gateway_quote::{DstackGatewayQuoteCollector, GatewayQuoteCollector, GatewayQuoteInput};
pub use ita::{ModelAttestationCollector, ModelAttestationInput};
pub use measurement::MeasurementPolicy;
pub use models::{AttestationError, ChatSignature, SignatureLookupResult};
pub(in crate::attestation) use report::{decode_nonce_hex, generate_nonce_hex};
pub use report_data::{ReportDataVerifier, StrictBoundReportDataVerifier};
pub use verification::{AttestationVerificationError, AttestationVerifier, VerifiedAttestation};

use crate::{
    attestation::{ita::ItaClient, models::VpcInfo, ports::AttestationRepository},
    inference_provider_pool::InferenceProviderPool,
    metrics::MetricsServiceTrait,
    models::ModelsRepository,
    usage::UsageRepository,
};

pub struct AttestationService {
    pub repository: Arc<dyn AttestationRepository + Send + Sync>,
    pub inference_provider_pool: Arc<InferenceProviderPool>,
    pub models_repository: Arc<dyn ModelsRepository>,
    pub metrics_service: Arc<dyn MetricsServiceTrait>,
    pub usage_repository: Arc<dyn UsageRepository>,
    pub vpc_info: Option<VpcInfo>,
    pub vpc_shared_secret: Option<String>,
    pub tls_cert_fingerprint: Option<String>,
    ed25519_signing_key: Arc<SigningKey>,
    ed25519_verifying_key: Arc<VerifyingKey>,
    ecdsa_signing_key: Arc<EcdsaSigningKey>,
    ecdsa_verifying_key: Arc<EcdsaVerifyingKey>,
    ita_config: ItaAttestationConfig,
    ita_client: Option<ItaClient>,
    gateway_quote_collector: Arc<dyn GatewayQuoteCollector>,
    model_attestation_collector: Arc<dyn ModelAttestationCollector>,
    /// Short-TTL cache for attestation reports of **nonce-less** requests only.
    /// Keyed on (model, algo, tls_fp, provider_filter, signing_address) — never
    /// on a nonce. moka's `try_get_with` also single-flights concurrent misses,
    /// so a burst of identical no-nonce probes triggers ONE backend build.
    /// `None` when disabled (TTL=0). Nonce-bearing requests bypass it entirely
    /// because the nonce is cryptographically bound into the TDX report_data and
    /// the GPU evidence — serving a cached report for a different nonce would
    /// defeat the freshness/replay guarantee.
    report_cache: Option<moka::future::Cache<String, Arc<models::AttestationReport>>>,
}
