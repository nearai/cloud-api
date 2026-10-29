//! Adapters that serve existing `services` ports from cloud-platform.
//!
//! They are used only when the client is configured (`CLOUD_PLATFORM_URL` and
//! `CLOUD_PLATFORM_API_KEY_FILE` both set). Unconfigured, cloud-api never builds
//! them; the local repositories serve everything.
//!
//! The DTOs here hold only the fields cloud-api reads and ignore unknown
//! fields, so cloud-platform can add fields without breaking cloud-api.

pub mod cache;
pub mod catalog;
pub mod check_api_key;
pub mod client;
pub mod config;
pub mod signature;

pub use catalog::CatalogModels;
pub use check_api_key::{ApiKeyVerdicts, Verdict};
pub use client::{
    current_caller_credential, with_caller_credential, with_request_id, BreakerState, CallType,
    CpClient, CpError, CpResponse, Credential,
};
pub use config::CpClientConfig;
pub use signature::ChatSignatureReads;
