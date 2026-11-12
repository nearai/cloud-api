use serde::{Deserialize, Serialize};
use services::files::FileServiceError;

/// Operator-facing failures carry only fixed messages, never storage inputs.
#[derive(Clone, Copy, Debug, Deserialize, Serialize, thiserror::Error)]
pub(super) enum JobError {
    #[serde(rename = "object_listing_unavailable")]
    #[error("object listing is temporarily unavailable")]
    ListingUnavailable,
    #[serde(rename = "object_listing_failed")]
    #[error("object listing failed permanently; check storage access and configuration")]
    ListingFailed,
    #[serde(rename = "object_listing_attempts_exhausted")]
    #[error("object listing consecutive retry budget exhausted")]
    ListingAttemptsExhausted,
    #[serde(rename = "object_worker_failed")]
    #[error("object worker failed before completing the job")]
    WorkerFailed,
    #[serde(rename = "object_write_disabled")]
    #[error("object rewrap write gate is disabled")]
    WriteDisabled,
    #[serde(rename = "object_invalid_page")]
    #[error("object listing returned an invalid page")]
    InvalidPage,
}

impl JobError {
    pub const fn class(self) -> &'static str {
        match self {
            Self::ListingUnavailable => "object_listing_unavailable",
            Self::ListingFailed => "object_listing_failed",
            Self::ListingAttemptsExhausted => "object_listing_attempts_exhausted",
            Self::WorkerFailed => "object_worker_failed",
            Self::WriteDisabled => "object_write_disabled",
            Self::InvalidPage => "object_invalid_page",
        }
    }

    pub fn from_error(error: &anyhow::Error) -> Self {
        error
            .downcast_ref::<Self>()
            .copied()
            .unwrap_or(Self::WorkerFailed)
    }
}

pub(super) fn transient(error: &anyhow::Error) -> bool {
    error.is::<tokio::time::error::Elapsed>()
        || matches!(
            error.downcast_ref::<FileServiceError>(),
            Some(FileServiceError::TransientStorageError(_))
        )
}
