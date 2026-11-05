//! Ports of the MPP domain: the single-use claim store, the channel ledger, the payee's
//! signer and the Tempo JSON-RPC.
//!
//! Neither trait names an HTTP, SQL or Axum type, so the service is tested with in-memory
//! fakes (see `service.rs` tests) and implemented in `crates/database` and `rpc.rs`.

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use uuid::Uuid;

use super::abi::{ChannelDescriptor, ChannelState};
use super::secp256k1::{SigError, SIGNATURE_LEN};
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

    /// Whether `payment_key` can no longer be claimed under `challenge_id`: a row exists
    /// that is not an `unserved` claim of that same challenge. Read-only and advisory: it
    /// refuses a spent credential before any RPC call or the service key's rate limit, while
    /// the atomic [`Self::claim`] still decides races.
    async fn is_spent(
        &self,
        payment_key: &str,
        challenge_id: &str,
    ) -> Result<bool, RepositoryError>;

    /// Record how a claimed payment ended. Only the `serving` row still holding `lease`
    /// changes; anything else (a stale attempt's finish) is a no-op.
    async fn finish(
        &self,
        payment_key: &str,
        lease: ClaimLease,
        outcome: ServeOutcome,
    ) -> Result<(), RepositoryError>;

    /// Release abandoned `serving` attempts last updated before `cutoff`, in bounded
    /// batches. Renew each released row's lease so a stale finish cannot affect a retry.
    /// Served and already unserved claims retain their state and challenge binding.
    async fn reclaim_stale_serving(&self, cutoff: DateTime<Utc>) -> Result<u64, RepositoryError>;

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
    /// The block the transaction was mined in.
    pub block_number: u64,
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
    /// refused or is already known). A rejection without a matching receipt is
    /// ambiguous: keep a pending close until its expiring nonce passes
    /// `valid_before + 30` seconds.
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

    /// `eth_getBlockByNumber`: the block's timestamp (Unix seconds), `None` if unknown.
    async fn get_block_timestamp(&self, number: u64) -> Result<Option<u64>, RpcError>;

    /// `eth_call` of `data` to `to` against the latest block: the raw return data. A revert
    /// is [`RpcError::Rejected`].
    async fn call(&self, to: [u8; 20], data: &[u8]) -> Result<Vec<u8>, RpcError>;

    /// `eth_gasPrice`: the current gas price in the fee token's base units per gas.
    async fn gas_price(&self) -> Result<u128, RpcError>;
}

// ---------------------------------------------------------------------------------------
// `session` intent: the payee's signer and the channel ledger
// ---------------------------------------------------------------------------------------

/// Signs for the payee: settlement and payee-close transactions. Deployments hold the key
/// of `MPP_TEMPO_SIGNER_KEY`; tests use any secp256k1 key.
pub trait TempoSigner: Send + Sync {
    /// The payee address: the recipient of every session challenge.
    fn address(&self) -> [u8; 20];

    /// A 65-byte `r || s || v` secp256k1 signature over a 32-byte digest (a Tempo
    /// transaction's signing hash).
    fn sign(&self, digest: &[u8; 32]) -> Result<[u8; SIGNATURE_LEN], SigError>;
}

/// Lifecycle of a channel row. The ledger is the only writer of these values (`as_str` is
/// their text form); `parse` reads them back from the store.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChannelLifecycle {
    /// Accepting vouchers and reservations (unless a close was requested on chain).
    Open,
    /// A close transaction is built or in flight: no new vouchers or reservations.
    Closing,
    /// Closed on chain, or withdrawn by the payer. Terminal; purged after a retention.
    Finalized,
}

impl ChannelLifecycle {
    pub const OPEN: &'static str = "open";
    pub const CLOSING: &'static str = "closing";
    pub const FINALIZED: &'static str = "finalized";

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Open => Self::OPEN,
            Self::Closing => Self::CLOSING,
            Self::Finalized => Self::FINALIZED,
        }
    }

    pub fn parse(text: &str) -> Option<Self> {
        match text {
            Self::OPEN => Some(Self::Open),
            Self::CLOSING => Some(Self::Closing),
            Self::FINALIZED => Some(Self::Finalized),
            _ => None,
        }
    }
}

/// A voucher the ledger keeps so that settlement can pick one no higher than what was
/// delivered.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RetainedVoucher {
    pub cumulative: u128,
    pub signature: [u8; SIGNATURE_LEN],
}

/// Most earlier vouchers a channel retains beside its highest one.
pub const MAX_RETAINED_VOUCHERS: usize = 16;

/// A payee close that was built and may have reached the chain. Stored before the
/// broadcast so a lost reply can be reconciled by looking the hash up.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PendingClose {
    pub tx_hash: [u8; 32],
    /// The voucher amount the close carries.
    pub cumulative: u128,
    /// What the payee captures; the rest of the deposit returns to the payer.
    pub capture: u128,
    /// The transaction's `valid_before` (Unix seconds): past it plus a margin, an
    /// unconfirmed close never lands.
    pub valid_before: u64,
}

/// What `open` records: the identity of the channel and its first voucher.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NewChannel {
    pub channel_id: [u8; 32],
    pub chain_id: u64,
    pub escrow: [u8; 20],
    pub descriptor: ChannelDescriptor,
    pub deposit: u128,
    pub settled_on_chain: u128,
    pub highest_cumulative: u128,
    pub highest_signature: [u8; SIGNATURE_LEN],
}

/// One channel as the ledger holds it. Amounts are token base units.
///
/// Invariants the ledger keeps: `reserved <= spent`, `spent <= highest_cumulative`,
/// `settled_on_chain <= deposit` (once read from chain), and every retained voucher is
/// below `highest_cumulative`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChannelRecord {
    pub channel_id: [u8; 32],
    pub chain_id: u64,
    pub escrow: [u8; 20],
    pub descriptor: ChannelDescriptor,
    /// The escrowed deposit as last read from chain (never decreased by a read).
    pub deposit: u128,
    /// The amount settled on chain, as last read or as this server settled it.
    pub settled_on_chain: u128,
    /// The highest voucher accepted.
    pub highest_cumulative: u128,
    pub highest_signature: [u8; SIGNATURE_LEN],
    /// Earlier vouchers, highest first, at most [`MAX_RETAINED_VOUCHERS`].
    pub retained_vouchers: Vec<RetainedVoucher>,
    /// Charged plus reserved: what the channel is committed to so far.
    pub spent: u128,
    /// The part of `spent` held by in-flight requests.
    pub reserved: u128,
    /// Tokens charged so far.
    pub units: u64,
    pub state: ChannelLifecycle,
    /// Unix seconds the payer requested a close on chain; `0` when none.
    pub close_requested_at: u64,
    pub pending_close: Option<PendingClose>,
    pub last_chain_read_at: Option<DateTime<Utc>>,
    pub last_settlement_at: Option<DateTime<Utc>>,
    /// `spent` at the last settlement, for the amount-based schedule.
    pub last_settlement_spent: u128,
    pub updated_at: DateTime<Utc>,
}

impl ChannelRecord {
    /// What the highest voucher still covers: `highest_cumulative - spent`.
    pub fn available(&self) -> u128 {
        self.highest_cumulative.saturating_sub(self.spent)
    }

    /// What was actually delivered: `spent - reserved`. Settlement never goes above it.
    pub fn delivered(&self) -> u128 {
        self.spent.saturating_sub(self.reserved)
    }

    /// Whether new vouchers and reservations are taken: open, and no close requested.
    pub fn accepts_vouchers(&self) -> bool {
        self.state == ChannelLifecycle::Open && self.close_requested_at == 0
    }

    /// The voucher to settle now: the highest voucher (the current one or a retained one)
    /// with `settled_on_chain < cumulative <= delivered()`. `None` when nothing new can
    /// be settled without claiming more than was delivered.
    pub fn settleable_voucher(&self) -> Option<RetainedVoucher> {
        let delivered = self.delivered();
        let highest = RetainedVoucher {
            cumulative: self.highest_cumulative,
            signature: self.highest_signature,
        };
        std::iter::once(highest)
            .chain(self.retained_vouchers.iter().copied())
            .filter(|voucher| {
                voucher.cumulative > self.settled_on_chain && voucher.cumulative <= delivered
            })
            .max_by_key(|voucher| voucher.cumulative)
    }
}

/// Result of [`ChannelRepository::accept_voucher`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VoucherOutcome {
    /// The voucher is now the channel's highest; the record after the write.
    Accepted(Box<ChannelRecord>),
    /// Not above the stored highest voucher: nothing changed.
    NotHigher,
    /// The increase above the stored highest is below the required minimum.
    DeltaTooSmall,
    /// Below the settled chain floor when that rule is enabled.
    BelowSettled,
    /// Above the stored deposit.
    AboveDeposit,
    /// The channel is closing, finalized, or has a close requested on chain.
    NotAccepting,
    /// No such channel.
    Missing,
}

/// Result of [`ChannelRepository::reserve`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReserveOutcome {
    /// `amount` is held for the request; the record after the write.
    Reserved(Box<ChannelRecord>),
    /// The highest voucher does not cover `amount` on top of `spent`; nothing is held.
    Insufficient { available: u128 },
    /// The channel is closing, finalized, or has a close requested on chain.
    NotAccepting,
    /// No such channel.
    Missing,
}

/// Result of [`ChannelRepository::charge`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ChargeOutcome {
    /// The reservation became a charge of `charged` (the actual cost, capped at what the
    /// highest voucher still covered); the record after the write.
    Charged {
        record: Box<ChannelRecord>,
        charged: u128,
    },
    /// No reservation with that request id (released, charged, or reaped already).
    NoReservation,
}

/// Result of [`ChannelRepository::mark_closing`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CloseTransition {
    /// The channel is now `closing` with this pending close; the record after the write.
    Marked(Box<ChannelRecord>),
    /// The channel was not `open`.
    NotOpen,
    /// A settlement lease is still live; no close was stored.
    SettlementInFlight,
    /// No such channel.
    Missing,
}

/// Atomic amount checks applied to an authenticated voucher under the channel lock.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VoucherRules {
    pub min_delta: u128,
    pub settled_floor_from_chain: bool,
}

/// The exact stored deadline of a settlement claim. Earlier holders cannot write or
/// release a replacement lease. PostgreSQL timestamps have microsecond precision;
/// both adapters return the deadline after that same normalization.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SettlementLease(DateTime<Utc>);

impl SettlementLease {
    pub fn new(until: DateTime<Utc>) -> Self {
        Self(until)
    }

    pub fn until(self) -> DateTime<Utc> {
        self.0
    }
}

/// The channel ledger of the `session` intent: one row per channel the server is payee
/// of, plus one row per in-flight reservation. It stores no organization, workspace,
/// request content or API key.
///
/// Every method that reads and then writes a channel is one atomic step (one transaction
/// with the channel row locked), so two instances serving the same channel never both
/// spend the same balance. Amounts never go negative: a write that would make
/// `reserved > spent` or `spent > highest_cumulative` does not happen.
#[async_trait]
pub trait ChannelRepository: Send + Sync {
    /// Insert the channel, or refresh an existing row with the same id: `deposit` and
    /// `settled_on_chain` take the greater value, while the existing voucher remains
    /// unchanged. Use `accept_voucher` to promote it with atomic rules. The state of an
    /// existing row is left as it is. Returns the row after the write.
    async fn open_or_refresh(&self, channel: NewChannel) -> Result<ChannelRecord, RepositoryError>;

    async fn get(&self, channel_id: &[u8; 32]) -> Result<Option<ChannelRecord>, RepositoryError>;

    /// Make `cumulative` the channel's highest voucher. Only an accepting channel takes a
    /// voucher, and only one above the stored highest; the old highest joins the retained
    /// vouchers (highest first, the lowest dropped past [`MAX_RETAINED_VOUCHERS`]). The
    /// caller has verified the signature. Under the same lock, enforce `rules`, the
    /// stored deposit ceiling and (when enabled) the settled chain floor.
    async fn accept_voucher(
        &self,
        channel_id: &[u8; 32],
        cumulative: u128,
        signature: [u8; SIGNATURE_LEN],
        rules: VoucherRules,
    ) -> Result<VoucherOutcome, RepositoryError>;

    /// Hold `amount` for `request_id` before inference runs: `spent += amount`,
    /// `reserved += amount`, one reservation row. Refused without any change when the
    /// channel is not accepting or `highest_cumulative - spent < amount`. `amount` is
    /// never zero; a second reservation under the same request id is an error.
    async fn reserve(
        &self,
        channel_id: &[u8; 32],
        request_id: Uuid,
        amount: u128,
    ) -> Result<ReserveOutcome, RepositoryError>;

    /// Turn the reservation of `request_id` into a charge of `actual`, capped at what the
    /// highest voucher still covers (`highest_cumulative - (spent - amount)`):
    /// `spent = spent - amount + charged`, `reserved -= amount`, `units += units`, and the
    /// reservation row goes.
    async fn charge(
        &self,
        request_id: Uuid,
        actual: u128,
        units: u64,
    ) -> Result<ChargeOutcome, RepositoryError>;

    /// Drop the reservation of `request_id` without a charge: `spent -= amount`,
    /// `reserved -= amount`. `Ok(false)` when there is no such reservation.
    async fn release(&self, request_id: Uuid) -> Result<bool, RepositoryError>;

    /// Record a chain read: `deposit` takes the greater of the stored and the on-chain
    /// value unless the on-chain deposit is zero (a withdrawn channel; the caller
    /// finalizes it), `settled_on_chain` takes the greater value, a non-zero
    /// `close_requested_at` is stored, and `last_chain_read_at` takes the greater of its
    /// stored value and `read_at`, preserving newer refresh claims. Returns the
    /// row after the write, `None` for an unknown channel.
    async fn record_chain_state(
        &self,
        channel_id: &[u8; 32],
        state: ChannelState,
        read_at: DateTime<Utc>,
    ) -> Result<Option<ChannelRecord>, RepositoryError>;

    /// `open` to `closing`, storing `pending` before its transaction is broadcast.
    /// Refuse while a settlement lease deadline is strictly greater than `now`.
    async fn mark_closing(
        &self,
        channel_id: &[u8; 32],
        pending: PendingClose,
        now: DateTime<Utc>,
    ) -> Result<CloseTransition, RepositoryError>;

    /// The channel is closed or withdrawn on chain: state `finalized`,
    /// `settled_on_chain = max(stored, settled_on_chain)`, `pending_close` cleared. Any
    /// state may finalize; a no-op for an unknown channel.
    async fn finalize(
        &self,
        channel_id: &[u8; 32],
        settled_on_chain: u128,
    ) -> Result<(), RepositoryError>;

    /// A close that did not land: `closing` back to `open`, `pending_close` cleared. A
    /// no-op returning false for any other state or a different pending hash.
    async fn clear_closing(
        &self,
        channel_id: &[u8; 32],
        expected_tx_hash: &[u8; 32],
    ) -> Result<bool, RepositoryError>;

    /// Finalize only the closing row whose pending transaction still matches.
    /// A stale reconciler is a no-op returning false.
    async fn finalize_pending_close(
        &self,
        channel_id: &[u8; 32],
        expected_tx_hash: &[u8; 32],
        settled_on_chain: u128,
    ) -> Result<bool, RepositoryError>;

    /// Claim a chain refresh under the row lock, writing `now` only if no read was
    /// claimed within `min_interval`. Equality at the interval boundary is eligible.
    async fn claim_chain_read(
        &self,
        channel_id: &[u8; 32],
        now: DateTime<Utc>,
        min_interval: chrono::Duration,
    ) -> Result<bool, RepositoryError>;

    /// Take the settlement lease of a channel until `until`, if no instance holds one
    /// at or past `now`, and only while Open. Returns the row and the exact stored
    /// deadline fence. An invalid deadline (`until <= now`) is refused.
    async fn claim_settlement_lease(
        &self,
        channel_id: &[u8; 32],
        now: DateTime<Utc>,
        until: DateTime<Utc>,
    ) -> Result<Option<(ChannelRecord, SettlementLease)>, RepositoryError>;

    /// A settlement of `cumulative` landed: `settled_on_chain = max(stored, cumulative)`,
    /// `last_settlement_at = at`, `last_settlement_spent = spent`, retained vouchers at or
    /// below `cumulative` dropped, and the lease released. Only the Open row still
    /// holding `lease` changes; a stale holder returns false without writing.
    async fn record_settlement(
        &self,
        channel_id: &[u8; 32],
        lease: SettlementLease,
        cumulative: u128,
        at: DateTime<Utc>,
    ) -> Result<bool, RepositoryError>;

    /// Give the settlement lease back without a settlement. A stale holder returns
    /// false without writing.
    async fn release_settlement_lease(
        &self,
        channel_id: &[u8; 32],
        lease: SettlementLease,
    ) -> Result<bool, RepositoryError>;

    /// Channels that are not finalized, ordered by channel id, those after `after` first,
    /// at most `limit`. Pages the close supervision and settlement tasks.
    async fn list_active(
        &self,
        after: Option<[u8; 32]>,
        limit: usize,
    ) -> Result<Vec<ChannelRecord>, RepositoryError>;

    /// Release every reservation created before `cutoff` (a crashed instance's), as
    /// [`Self::release`] would; returns how many went.
    async fn release_stale_reservations(
        &self,
        cutoff: DateTime<Utc>,
    ) -> Result<u64, RepositoryError>;

    /// Delete finalized channels last updated before `cutoff` (their reservations go with
    /// them); returns how many went.
    async fn purge_finalized(&self, cutoff: DateTime<Utc>) -> Result<u64, RepositoryError>;
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
