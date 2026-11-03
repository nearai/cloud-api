//! Ports of the MPP domain: the single-use claim store and the Tempo JSON-RPC.
//!
//! Neither trait names an HTTP, SQL or Axum type, so the service is tested with in-memory
//! fakes (see `service.rs` tests) and implemented in `crates/database` and `rpc.rs`.

use async_trait::async_trait;

use crate::common::RepositoryError;

/// Lifecycle of one claimed payment. A closed set with a strict parser: a value outside it
/// is an error, never a default.
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

    pub fn parse(raw: &str) -> Result<Self, UnknownClaimState> {
        match raw {
            Self::SERVING => Ok(Self::Serving),
            Self::SERVED => Ok(Self::Served),
            Self::UNSERVED => Ok(Self::Unserved),
            _ => Err(UnknownClaimState),
        }
    }
}

/// A stored claim state outside the closed set. Carries no value: it never reaches a log.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("unknown MPP claim state")]
pub struct UnknownClaimState;

/// Result of [`PaymentClaimRepository::claim`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClaimOutcome {
    /// This caller now holds the payment and may serve the request.
    Claimed,
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

    /// Record how a claimed payment ended. Only a `serving` row changes.
    async fn finish(&self, payment_key: &str, outcome: ServeOutcome)
        -> Result<(), RepositoryError>;
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
    fn claim_state_round_trips_and_rejects_unknown_values() {
        for state in [
            ClaimState::Serving,
            ClaimState::Served,
            ClaimState::Unserved,
        ] {
            assert_eq!(ClaimState::parse(state.as_str()), Ok(state));
        }
        for bad in ["", "Serving", "SERVED", "done", "serving ", "unserved\n"] {
            assert_eq!(ClaimState::parse(bad), Err(UnknownClaimState), "{bad:?}");
        }
    }

    #[test]
    fn serve_outcome_only_maps_to_terminal_states() {
        assert_eq!(ServeOutcome::Served.state(), ClaimState::Served);
        assert_eq!(ServeOutcome::Unserved.state(), ClaimState::Unserved);
    }
}
