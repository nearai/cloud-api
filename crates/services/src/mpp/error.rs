//! Every way MPP processing can end other than success, and the one place that decides how
//! each is answered.
//!
//! [`MppError::response`] is a closed mapping: the API layer matches [`MppResponse`]
//! exhaustively, so a new error cannot reach the wire without a decision about whether it
//! carries a challenge, a problem document or a retry hint. Variants carry closed reasons
//! only; no value here is, or embeds, request content, a credential or a transaction.

use std::time::Duration;

use super::pricing::LimitViolation;
use super::protocol::{ProblemCode, ProtocolError};
use super::transaction::TransactionMismatch;
use crate::common::RepositoryError;

/// `Retry-After` for a payment that is not on chain yet: the same credential is good, the
/// transaction only needs a moment to be found.
pub const RECEIPT_RETRY_AFTER: Duration = Duration::from_secs(2);
/// `Retry-After` when the Tempo RPC or the claim store did not answer.
pub const BACKEND_RETRY_AFTER: Duration = Duration::from_secs(5);

/// Why chain verification failed in a way another payment can fix.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VerifyFailure {
    Reverted,
    NoMatchingTransfer,
    /// The node answered with a receipt for another transaction.
    ReceiptMismatch,
    SourceChain,
    /// The `transaction` payload does not pay this challenge; it was not broadcast.
    Transaction(TransactionMismatch),
}

/// Why a verified payment can not be used.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConsumedReason {
    AlreadyClaimed,
    /// The funds moved but not for this challenge: paying again would not help.
    MemoNotBound,
}

/// Which dependency was unreachable.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UnavailableReason {
    Rpc,
    Store,
}

/// Every way MPP processing can end other than success. Variants carry closed reasons only.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum MppError {
    #[error("malformed credential")]
    Malformed(&'static str),
    #[error("method unsupported")]
    MethodUnsupported,
    #[error("invalid challenge")]
    InvalidChallenge(&'static str),
    #[error("challenge expired")]
    Expired,
    #[error("payment verification failed")]
    VerificationFailed(VerifyFailure),
    /// No receipt was found for the payment: not mined yet, or the node refused it and holds
    /// no record of it. The credential stays good, so the answer is a retry, not a new
    /// challenge: a client that paid must not be asked to pay again.
    #[error("payment receipt not found")]
    ReceiptNotFound,
    #[error("payment already used")]
    Consumed(ConsumedReason),
    #[error("payment backend unavailable")]
    Unavailable(UnavailableReason),
    #[error("request limit violated")]
    Limit(LimitViolation),
    #[error("invalid MPP configuration")]
    Config,
}

/// The status of a problem document that carries no challenge.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProblemStatus {
    BadRequest,
    Forbidden,
}

impl ProblemStatus {
    pub fn code(self) -> u16 {
        match self {
            Self::BadRequest => 400,
            Self::Forbidden => 403,
        }
    }
}

/// How an [`MppError`] is answered on the wire.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MppResponse {
    /// `402` with a fresh challenge and this problem type: paying (again) can fix it.
    Challenge(ProblemCode),
    /// A problem document with no challenge: nothing the client pays can fix it.
    Problem(ProblemStatus, ProblemCode),
    /// `400 validation_error` naming the limit the request breaks.
    Limit(LimitViolation),
    /// `503` with `Retry-After`: the same credential may be presented again.
    Retry(Duration),
}

impl MppError {
    /// Closed code for logs and metrics.
    pub fn kind(&self) -> &'static str {
        match self {
            Self::Malformed(_) => "malformed_credential",
            Self::MethodUnsupported => "method_unsupported",
            Self::InvalidChallenge(_) => "invalid_challenge",
            Self::Expired => "payment_expired",
            Self::VerificationFailed(VerifyFailure::Reverted) => "tx_reverted",
            Self::VerificationFailed(VerifyFailure::NoMatchingTransfer) => "no_matching_transfer",
            Self::VerificationFailed(VerifyFailure::ReceiptMismatch) => "receipt_mismatch",
            Self::VerificationFailed(VerifyFailure::SourceChain) => "source_chain_mismatch",
            Self::VerificationFailed(VerifyFailure::Transaction(mismatch)) => mismatch.code(),
            Self::ReceiptNotFound => "receipt_not_found",
            Self::Consumed(ConsumedReason::AlreadyClaimed) => "already_claimed",
            Self::Consumed(ConsumedReason::MemoNotBound) => "memo_not_bound",
            Self::Unavailable(UnavailableReason::Rpc) => "rpc_unavailable",
            Self::Unavailable(UnavailableReason::Store) => "store_unavailable",
            Self::Limit(limit) => limit.code(),
            Self::Config => "config",
        }
    }

    /// How this error is answered.
    pub fn response(&self) -> MppResponse {
        match self {
            Self::Malformed(_) => MppResponse::Challenge(ProblemCode::MalformedCredential),
            Self::InvalidChallenge(_) => MppResponse::Challenge(ProblemCode::InvalidChallenge),
            Self::Expired => MppResponse::Challenge(ProblemCode::PaymentExpired),
            Self::VerificationFailed(_) => MppResponse::Challenge(ProblemCode::VerificationFailed),
            Self::MethodUnsupported => {
                MppResponse::Problem(ProblemStatus::BadRequest, ProblemCode::MethodUnsupported)
            }
            Self::Consumed(_) => {
                MppResponse::Problem(ProblemStatus::Forbidden, ProblemCode::VerificationFailed)
            }
            Self::Limit(limit) => MppResponse::Limit(*limit),
            Self::ReceiptNotFound => MppResponse::Retry(RECEIPT_RETRY_AFTER),
            Self::Unavailable(_) | Self::Config => MppResponse::Retry(BACKEND_RETRY_AFTER),
        }
    }

    /// A fixed, input-free sentence for the problem `detail`.
    pub fn detail(&self) -> &'static str {
        match self {
            Self::Malformed(_) => "The Payment credential could not be read.",
            Self::MethodUnsupported => "This payment method is not supported.",
            Self::InvalidChallenge(_) => {
                "The challenge in the credential is not valid for this request."
            }
            Self::Expired => "The challenge has expired.",
            Self::VerificationFailed(_) => "Payment verification failed.",
            Self::ReceiptNotFound => "The transaction was not found or is not yet mined.",
            Self::Consumed(_) => "This payment has already been used.",
            Self::Unavailable(_) | Self::Config => {
                "Payment verification is temporarily unavailable."
            }
            Self::Limit(_) => "The request is not eligible for payment.",
        }
    }
}

impl From<ProtocolError> for MppError {
    fn from(error: ProtocolError) -> Self {
        match error {
            ProtocolError::Malformed(reason) => Self::Malformed(reason),
            ProtocolError::MethodUnsupported => Self::MethodUnsupported,
        }
    }
}

/// Closed name of a repository failure, for logs: the variant, never its message (which may
/// carry SQL, keys or a connection string).
pub fn repository_error_kind(error: &RepositoryError) -> &'static str {
    match error {
        RepositoryError::NotFound(_) => "not_found",
        RepositoryError::AlreadyExists => "already_exists",
        RepositoryError::RequiredFieldMissing(_) => "required_field_missing",
        RepositoryError::ForeignKeyViolation(_) => "foreign_key_violation",
        RepositoryError::ValidationFailed(_) => "validation_failed",
        RepositoryError::DependencyExists(_) => "dependency_exists",
        RepositoryError::TransactionConflict => "transaction_conflict",
        RepositoryError::ConnectionFailed(_) => "connection_failed",
        RepositoryError::AuthenticationFailed => "authentication_failed",
        RepositoryError::QueryTimeout => "query_timeout",
        RepositoryError::PoolError(_) => "pool_error",
        RepositoryError::DatabaseError(_) => "database_error",
        RepositoryError::DataConversionError(_) => "data_conversion_error",
    }
}
