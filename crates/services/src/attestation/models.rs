use serde::{Deserialize, Serialize};
/// Error types for attestation operations
#[derive(Debug, thiserror::Error)]
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
    /// The attestation quote in hexadecimal format
    pub intel_quote: String,
    /// The event log associated with the quote
    pub event_log: String,
    /// The report data
    #[serde(default)]
    pub report_data: String,
    /// The nonce used in the attestation request
    pub request_nonce: String,
    /// Application info from Dstack
    pub info: serde_json::Value,
    /// VPC information (optional)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub vpc: Option<VpcInfo>,
}

impl DstackCpuQuote {
    pub fn from_quote_and_nonce(
        vpc: Option<VpcInfo>,
        info: dstack_sdk::dstack_client::InfoResponse,
        quote: dstack_sdk::dstack_client::GetQuoteResponse,
        nonce: String,
    ) -> Self {
        Self {
            intel_quote: quote.quote,
            event_log: quote.event_log,
            report_data: quote.report_data,
            request_nonce: nonce,
            info: serde_json::to_value(info).unwrap_or_default(),
            vpc,
        }
    }
}

pub struct AttestationReport {
    pub gateway_attestation: DstackCpuQuote,
    pub all_attestations: Vec<serde_json::Map<String, serde_json::Value>>,
}

pub type DstackAppInfo = dstack_sdk::dstack_client::InfoResponse;
