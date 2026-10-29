//! Adapters that serve existing `services` ports from cloud-platform.
//!
//! Each area is chosen at composition time by its `CP_*` flag
//! (`off|shadow|on`, see [`config::AreaMode`]); the local repositories stay
//! as the `off` implementation and as the comparison in `shadow`.
//!
//! The DTOs here hold only the fields cloud-api reads and ignore unknown
//! fields, so cloud-platform can add fields without breaking cloud-api.

pub mod cache;
pub mod catalog;
pub mod client;
pub mod config;
pub mod signature;

pub use catalog::CatalogModels;
pub use client::{
    current_caller_credential, with_caller_credential, with_request_id, BreakerState, CallType,
    CpClient, CpError, CpResponse, Credential,
};
pub use config::{AreaFlags, AreaMode, CpClientConfig};
pub use signature::ChatSignatureReads;
