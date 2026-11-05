//! Machine Payments Protocol (MPP): HTTP 402 payments in a TIP-20 stablecoin on Tempo.
//!
//! Scope: the `tempo` method, `charge` intent (and the protocol primitives of `session`), on `POST /v1/chat/completions`, with a flat
//! price per model. See `docs/mpp.md`. The layers:
//!
//! - [`protocol`]: the wire format (challenge, credential, receipt, problem details).
//! - [`tempo`]: verification of a transaction receipt against a challenge.
//! - [`rpc`]: the Tempo JSON-RPC client behind the [`ports::TempoRpc`] port.
//! - [`service`]: [`MppService`], the only entry point the API layer uses.
//! - [`session`]: the chain-level primitives of the `session` intent (TIP-1034 payment
//!   channels): [`eip712`] voucher and close digests, [`secp256k1`] recovery and signing,
//!   [`abi`] calldata for the channel reserve precompile, [`channel`] ids, [`tempo_tx`]
//!   Tempo transactions, and [`open_check`], the pre-broadcast check of a client's open.
//! - [`session_wire`], [`session_service`], [`session_settlement`], [`session_supervision`]:
//!   the `session` intent over the [`ports::ChannelRepository`] ledger and
//!   [`ports::TempoSigner`] ([`signer`]).
//! - [`ports`]: the claim store, channel ledger, signer and RPC traits. `memory` (tests and the `test-mocks`
//!   feature only) is an in-memory claim store; deployments use the Postgres repository.

pub mod abi;
pub mod channel;
#[cfg(any(test, feature = "test-mocks"))]
pub mod channel_memory;
#[cfg(test)]
mod channel_memory_tests;
pub mod eip712;
pub mod error;
#[cfg(any(test, feature = "test-mocks"))]
pub mod memory;
pub mod open_check;
pub mod ports;
pub mod pricing;
pub mod protocol;
pub mod retention;
pub mod rlp;
pub mod rpc;
pub mod secp256k1;
pub mod service;
pub mod session_chain;
pub mod session_service;
pub mod session_settlement;
pub mod session_supervision;
#[cfg(test)]
mod session_vectors;
pub mod session_wire;
pub mod signer;
pub mod tempo;
pub mod tempo_tx;
#[cfg(any(test, feature = "test-mocks"))]
pub mod testing;
pub mod transaction;

pub use error::{
    ConsumedReason, MppError, MppResponse, ProblemStatus, UnavailableReason, VerifyFailure,
};
#[cfg(any(test, feature = "test-mocks"))]
pub use memory::InMemoryClaimRepository;
pub use ports::{
    ChannelLifecycle, ChannelRecord, ChannelRepository, ChargeOutcome, ClaimLease, ClaimOutcome,
    ClaimState, CloseTransition, NewChannel, PaymentClaimRepository, PendingClose, ReserveOutcome,
    RetainedVoucher, RpcError, RpcLog, RpcReceipt, ServeOutcome, TempoRpc, TempoSigner,
    VoucherOutcome, MAX_RETAINED_VOUCHERS,
};
pub use pricing::{LimitViolation, Param, Price, Pricing, RequestShape};
pub use protocol::{Challenge, ProblemCode};
pub use rpc::HttpTempoRpc;
pub use service::{
    MppService, PaidCredential, ServingGuard, SettledPayment, VerifiedCredential,
    CHAT_COMPLETIONS_ROUTE,
};
pub use signer::{LocalSigner, SignerKeyError};

/// The chain-level core of the MPP `session` intent. No I/O and no state: digests,
/// signatures, ABI, channel ids and Tempo transactions, each verified against golden
/// vectors from the official ox/viem/mppx libraries (`testdata/session_vectors.json`).
pub mod session {
    pub use super::abi::{
        close_calldata, decode_channel_state, decode_channel_states_batch, decode_open_calldata,
        get_channel_state_calldata, get_channel_states_batch_calldata, open_calldata,
        settle_calldata, AbiError, ChannelDescriptor, ChannelState, OpenArgs, CLOSE_SELECTOR,
        GET_CHANNEL_STATES_BATCH_SELECTOR, GET_CHANNEL_STATE_SELECTOR, MAX_BATCH_CHANNELS,
        MAX_SIGNATURE_BYTES, OPEN_SELECTOR, REQUEST_CLOSE_SELECTOR, SETTLE_SELECTOR,
        TOP_UP_SELECTOR,
    };
    pub use super::channel::{channel_id, voucher_signer};
    pub use super::eip712::{
        close_authorization_digest, domain_separator, voucher_digest, AmountOutOfRange, MAX_UINT96,
    };
    pub use super::open_check::{verify_open_transaction, OpenMismatch, OpenTerms, VerifiedOpen};
    pub use super::secp256k1::{address_of, recover_address, sign_digest, SigError, SIGNATURE_LEN};
    pub use super::tempo_tx::{
        decode_signed, expiring_nonce_hash, tx_hash, Call, TempoTx, TxError, EXPIRING_NONCE_KEY,
        EXPIRING_NONCE_VALIDITY_SECS, MAX_CALLS, MAX_TX_BYTES,
    };
}
