use serde::{Deserialize, Serialize};
/// Error types for attestation operations
#[derive(Debug, Clone, thiserror::Error)]
pub enum AttestationError {
    #[error("Signature not found: {0}")]
    SignatureNotFound(String),

    #[error("Provider error: {0}")]
    ProviderError(String),

    #[error("Repository error: {0}")]
    RepositoryError(String),

    #[error("Client error: {0}")]
    ClientError(String),

    #[error("Invalid parameter: {0}")]
    InvalidParameter(String),

    #[error("Internal error: {0}")]
    InternalError(String),

    #[error("ITA attestation is unavailable: {reason}")]
    ItaUnavailable { reason: String },

    #[error("ITA rate limited")]
    ItaRateLimited { retry_after: Option<String> },

    #[error("ITA request timed out")]
    ItaTimeout,

    #[error("ITA upstream error: {reason}")]
    ItaBadUpstream { reason: String },

    #[error("ITA evidence is invalid: {reason}")]
    ItaInvalidEvidence { reason: String },
}

/// Result of looking up a signature that includes fallback cases
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum SignatureLookupResult {
    /// Signature found successfully
    Found(ChatSignature),
    /// Signature unavailable (e.g., due to client disconnect)
    Unavailable { error_code: String, message: String },
}

/// Which key produced a stored chat signature.
///
/// The two kinds sign different payloads:
/// - [`SignatureKind::ProviderTee`]: signed inside the model-serving TEE over
///   `"{model_id}:{request_hash}:{response_hash}"` (the provider's canonical
///   text), covering the exact bytes the model backend emitted.
/// - [`SignatureKind::Gateway`]: signed by the cloud-api gateway TEE over
///   `"{request_hash}:{response_hash}"`, covering the exact bytes the client
///   received. Used when the gateway rewrites the stream (usage accounting or
///   stripping, redaction), so the provider's byte-exact signature can no
///   longer match, and for attested providers without per-response signatures.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SignatureKind {
    /// Signed by the model-serving TEE (provider signature, stored verbatim).
    ProviderTee,
    /// Signed by the cloud-api gateway TEE over the client-visible bytes.
    Gateway,
}

impl SignatureKind {
    /// Database string representation (matches the serde snake_case form).
    pub fn as_str(&self) -> &'static str {
        match self {
            SignatureKind::ProviderTee => "provider_tee",
            SignatureKind::Gateway => "gateway",
        }
    }

    /// Parse the database string representation. Returns `None` for unknown
    /// values so an unexpected row degrades to "kind unknown" instead of
    /// failing the whole signature lookup.
    pub fn from_db_str(s: &str) -> Option<Self> {
        match s {
            "provider_tee" => Some(SignatureKind::ProviderTee),
            "gateway" => Some(SignatureKind::Gateway),
            _ => None,
        }
    }
}

/// Chat signature for cryptographic verification
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChatSignature {
    /// The text being signed (typically contains hashes)
    pub text: String,
    /// The cryptographic signature
    pub signature: String,
    /// The address that created the signature
    pub signing_address: String,
    /// The signing algorithm used (e.g., "ecdsa")
    pub signing_algo: String,
    /// Which key produced this signature. `None` for rows stored before the
    /// kind was recorded (their provenance is unknown — both provider-TEE and
    /// gateway writes predate the column), so it is surfaced as absent rather
    /// than guessed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub signature_kind: Option<SignatureKind>,
}

/// VPC (Virtual Private Cloud) metadata included in attestation reports
/// This information is helpful to identify the VPC server and this VPC node.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VpcInfo {
    /// VPC server app ID
    pub vpc_server_app_id: Option<String>,
    /// VPC hostname of this node
    pub vpc_hostname: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DstackCpuQuote {
    /// The signing address used for the attestation
    pub signing_address: String,
    /// The signing algorithm used for the attestation (ecdsa or ed25519)
    pub signing_algo: String,
    /// The attestation quote in hexadecimal format
    pub intel_quote: String,
    /// The event log associated with the quote
    pub event_log: String,
    /// The report data that contains signing address and nonce
    #[serde(default)]
    pub report_data: String,
    /// The nonce used in the attestation request
    pub request_nonce: String,
    /// Application info from Dstack
    pub info: serde_json::Value,
    /// VPC information (optional)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub vpc: Option<VpcInfo>,
    /// SHA-256 hash of the TLS certificate's SPKI, if requested.
    /// When present, report_data[..32] = SHA256(signing_address_bytes || cert_fingerprint_bytes).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tls_cert_fingerprint: Option<String>,
}

impl DstackCpuQuote {
    pub fn from_quote_and_nonce(
        signing_address: String,
        signing_algo: String,
        vpc: Option<VpcInfo>,
        info: dstack_sdk::dstack_client::InfoResponse,
        quote: dstack_sdk::dstack_client::GetQuoteResponse,
        nonce: String,
        tls_cert_fingerprint: Option<String>,
    ) -> Self {
        Self {
            signing_address,
            signing_algo,
            intel_quote: quote.quote,
            event_log: quote.event_log,
            report_data: quote.report_data,
            request_nonce: nonce,
            info: serde_json::to_value(info).unwrap_or_default(),
            vpc,
            tls_cert_fingerprint,
        }
    }
}

// `Clone` so the no-nonce report cache can store an `Arc<AttestationReport>` and
// hand owned copies back to the trait method (which returns by value).
#[derive(Clone)]
pub struct AttestationReport {
    pub gateway_attestation: DstackCpuQuote,
    pub model_attestations: Vec<serde_json::Map<String, serde_json::Value>>,
    pub tls_certificate: Option<String>,
}

pub type DstackAppInfo = dstack_sdk::dstack_client::InfoResponse;
