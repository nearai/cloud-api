//! Ports of the MPP domain: the single-use claim store and the Tempo JSON-RPC.
//!
//! Neither trait names an HTTP, SQL or Axum type, so the service is tested with in-memory
//! fakes (see `service.rs` tests) and implemented in `crates/database` and `rpc.rs`.

use async_trait::async_trait;
use chrono::{DateTime, Utc};

use crate::common::RepositoryError;

/// Lifecycle of one claimed payment. A closed set; the claim store is the only writer of
/// these values (`as_str` is their text form), so nothing parses them back.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClaimState {
    /// The payment is claimed and its request is being served.
    Serving,
    /// The request was served. Terminal: the payment can never be used again.
    Served,
    /// Serving failed after the payment was claimed. The same challenge may claim it again.
    Unserved,
}

impl ClaimState {
    pub const SERVING: &'static str = "serving";
    pub const SERVED: &'static str = "served";
    pub const UNSERVED: &'static str = "unserved";

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Serving => Self::SERVING,
            Self::Served => Self::SERVED,
            Self::Unserved => Self::UNSERVED,
        }
    }
}

/// The generation of one claim attempt: the row's `updated_at` as the claim wrote it.
/// [`PaymentClaimRepository::finish`] only moves the row that still carries it, so a late or
/// retried `finish` of an earlier attempt can never touch a newer attempt's claim.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ClaimLease(DateTime<Utc>);

impl ClaimLease {
    pub fn new(at: DateTime<Utc>) -> Self {
        Self(at)
    }

    pub fn at(self) -> DateTime<Utc> {
        self.0
    }
}

/// Result of [`PaymentClaimRepository::claim`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClaimOutcome {
    /// This caller now holds the payment and may serve the request, under this lease.
    Claimed(ClaimLease),
    /// The payment was already claimed (being served, served, or bound to another challenge).
    AlreadyUsed,
}

/// How a claimed payment ended. Only these two can be written by `finish`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ServeOutcome {
    Served,
    Unserved,
}

impl ServeOutcome {
    pub fn state(self) -> ClaimState {
        match self {
            Self::Served => ClaimState::Served,
            Self::Unserved => ClaimState::Unserved,
        }
    }
}

/// Single-use ledger of paid references. It stores no customer, organization, request or
/// payer data: it proves a payment was used once and nothing else.
#[async_trait]
pub trait PaymentClaimRepository: Send + Sync {
    /// Claim `payment_key` (`<method>:<canonical reference>`) for `challenge_id`.
    ///
    /// A new key is claimed. A key left `unserved` by the same challenge is claimed again
    /// (a retry of a paid request whose inference failed). Every other existing row is
    /// `AlreadyUsed`. Must be one atomic step.
    async fn claim(
        &self,
        payment_key: &str,
        challenge_id: &str,
    ) -> Result<ClaimOutcome, RepositoryError>;

    /// Record how a claimed payment ended. Only the `serving` row still holding `lease`
    /// changes; anything else (a stale attempt's finish) is a no-op.
    async fn finish(
        &self,
        payment_key: &str,
        lease: ClaimLease,
        outcome: ServeOutcome,
    ) -> Result<(), RepositoryError>;

    /// Delete claims created before `cutoff`, in bounded batches; returns how many went. A
    /// claim only has to outlive its challenge (at most an hour), so callers pass a cutoff
    /// well past that.
    async fn purge_older_than(&self, cutoff: DateTime<Utc>) -> Result<u64, RepositoryError>;
}

/// A TIP-20 / EVM log of a Tempo transaction receipt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RpcLog {
    pub address: [u8; 20],
    pub topics: Vec<[u8; 32]>,
    pub data: Vec<u8>,
}

/// The parts of a Tempo transaction receipt the verification reads.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RpcReceipt {
    pub tx_hash: [u8; 32],
    /// `status == 0x1`.
    pub success: bool,
    pub from: [u8; 20],
    pub logs: Vec<RpcLog>,
}

/// Why a Tempo RPC call produced no receipt. No variant carries the node's text, a URL or a
/// transaction: those never reach a log.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum RpcError {
    /// The node could not be reached, timed out, or answered with an HTTP error.
    #[error("Tempo RPC unavailable")]
    Unavailable,
    /// The node answered with a JSON-RPC error object (for a send: the transaction was
    /// refused or is already known).
    #[error("Tempo RPC rejected the call")]
    Rejected,
    /// The node answered with something that is not a valid receipt.
    #[error("Tempo RPC returned an invalid response")]
    InvalidResponse,
}

impl RpcError {
    /// Closed name for logs.
    pub fn kind(self) -> &'static str {
        match self {
            Self::Unavailable => "unavailable",
            Self::Rejected => "rejected",
            Self::InvalidResponse => "invalid_response",
        }
    }
}

/// The Tempo JSON-RPC calls the verification needs.
#[async_trait]
pub trait TempoRpc: Send + Sync {
    /// `eth_sendRawTransactionSync` (EIP-7966): broadcast and wait for the receipt.
    async fn send_raw_transaction_sync(&self, raw: &[u8]) -> Result<RpcReceipt, RpcError>;

    /// `eth_getTransactionReceipt`: `None` while the transaction is unknown or not mined.
    async fn get_transaction_receipt(&self, hash: [u8; 32])
        -> Result<Option<RpcReceipt>, RpcError>;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn serve_outcome_only_maps_to_terminal_states() {
        assert_eq!(ServeOutcome::Served.state(), ClaimState::Served);
        assert_eq!(ServeOutcome::Unserved.state(), ClaimState::Unserved);
    }
}
