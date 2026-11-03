//! Machine Payments Protocol (MPP): HTTP 402 payments in a TIP-20 stablecoin on Tempo.
//!
//! Scope: the `tempo` method, `charge` intent, on `POST /v1/chat/completions`, with a flat
//! price per model. See `docs/mpp.md`. The layers:
//!
//! - [`protocol`]: the wire format (challenge, credential, receipt, problem details).
//! - [`tempo`]: verification of a transaction receipt against a challenge.
//! - [`rpc`]: the Tempo JSON-RPC client behind the [`ports::TempoRpc`] port.
//! - [`service`]: [`MppService`], the only entry point the API layer uses.
//! - [`ports`]: the claim store and RPC traits. `memory` (tests and the `test-mocks`
//!   feature only) is an in-memory claim store; deployments use the Postgres repository.

#[cfg(any(test, feature = "test-mocks"))]
pub mod memory;
pub mod ports;
pub mod protocol;
pub mod rpc;
pub mod service;
pub mod tempo;
#[cfg(any(test, feature = "test-mocks"))]
pub mod testing;

#[cfg(any(test, feature = "test-mocks"))]
pub use memory::InMemoryClaimRepository;
pub use ports::{
    ClaimOutcome, ClaimState, PaymentClaimRepository, RpcError, RpcLog, RpcReceipt, ServeOutcome,
    TempoRpc,
};
pub use protocol::{Challenge, ProblemCode};
pub use rpc::HttpTempoRpc;
pub use service::{
    ConsumedReason, LimitViolation, MppError, MppService, Price, Pricing, RequestShape,
    ServingGuard, SettledPayment, UnavailableReason, VerifiedCredential, VerifyFailure,
    CHAT_COMPLETIONS_ROUTE,
};
