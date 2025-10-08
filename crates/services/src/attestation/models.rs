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

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GetQuoteResponse {
    /// The attestation quote in hexadecimal format
    pub quote: String,
    /// The event log associated with the quote
    pub event_log: String,
}

impl From<dstack_sdk::dstack_client::GetQuoteResponse> for GetQuoteResponse {
    fn from(response: dstack_sdk::dstack_client::GetQuoteResponse) -> Self {
        Self {
            quote: response.quote,
            event_log: response.event_log,
        }
    }
}
