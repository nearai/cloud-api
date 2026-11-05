// INTEGRATION: include as a child test module of session_service.rs.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use chrono::Utc;
use config::{MppModelPrice, MppSessionSettings};
use serde_json::{json, Value};
use uuid::Uuid;

use super::{SessionCredential, SessionService, SessionServiceConfig};
use crate::mpp::abi::{self, ChannelDescriptor, ChannelState};
use crate::mpp::channel_memory::InMemoryChannelRepository;
use crate::mpp::eip712::{close_authorization_digest, voucher_digest};
use crate::mpp::error::{CloseReason, MppError, SessionMismatch, TopUpReason, VoucherReason};
use crate::mpp::open_check::OpenMismatch;
use crate::mpp::ports::{
    ChannelLifecycle, ChannelRepository, ChargeOutcome, NewChannel, PendingClose, ReserveOutcome,
    RpcError, RpcReceipt, TempoRpc,
};
use crate::mpp::pricing::Price;
use crate::mpp::protocol::{self, ParsedAuthorization};
use crate::mpp::secp256k1::sign_digest;
use crate::mpp::session_vectors::Vectors;
use crate::mpp::session_wire::{descriptor_json, hex_prefixed};
use crate::mpp::signer::LocalSigner;
use crate::mpp::tempo_tx::{decode_signed, tx_hash, TempoTx};
use crate::mpp::testing::{credential_header, FakeTempo};

const ROUTE: &str = "/v1/chat/completions";
const PRICE: Price = Price {
    amount: 0,
    token_price: Some(2),
};
const INITIAL: u128 = 1_250_000;
const DELTA: u128 = 100;

pub(super) fn config() -> SessionServiceConfig {
    let v = Vectors::load();
    SessionServiceConfig {
        settings: MppSessionSettings {
            signer_key: None,
            min_voucher_delta: Some(DELTA),
            suggested_deposit: v.int("deposit"),
            settle_interval_secs: 3600,
            settle_amount: 5_000_000,
            close_supervision_secs: 60,
        },
        model_prices: BTreeMap::from([(
            "model".into(),
            MppModelPrice {
                amount: 0,
                token_price: PRICE.token_price,
                max_tokens: 1000,
                max_request_bytes: 4096,
            },
        )]),
        realm: "session.example".into(),
        secret_key: vec![0x53; 32],
        challenge_ttl: Duration::from_secs(300),
        chain_id: v.chain_id(),
        escrow: v.escrow(),
        currency: v.fixed("token"),
    }
}

pub(super) struct Fixture {
    pub(super) service: SessionService,
    pub(super) repo: Arc<InMemoryChannelRepository>,
    pub(super) rpc: Arc<FakeTempo>,
}

impl Fixture {
    pub(super) fn new() -> Self {
        let repo = Arc::new(InMemoryChannelRepository::new());
        let rpc = Arc::new(FakeTempo::default());
        set_chain(&rpc, state());
        let service = service(config(), repo.clone(), rpc.clone());
        Self { service, repo, rpc }
    }

    pub(super) async fn seed(&self) {
        seed(&*self.repo, INITIAL, 0).await;
    }

    pub(super) async fn credential(&self, payload: Value) -> SessionCredential {
        verified(&self.service, payload).await
    }

    pub(super) async fn record(&self) -> crate::mpp::ports::ChannelRecord {
        self.repo
            .get(&Vectors::load().fixed("channelId"))
            .await
            .unwrap()
            .unwrap()
    }
}

pub(super) fn service(
    cfg: SessionServiceConfig,
    repo: Arc<InMemoryChannelRepository>,
    rpc: Arc<dyn TempoRpc>,
) -> SessionService {
    SessionService::new(
        cfg,
        repo,
        rpc,
        Arc::new(LocalSigner::new(Vectors::load().secret_key("payeeKey"))),
    )
    .unwrap()
}

pub(super) async fn verified(service: &SessionService, payload: Value) -> SessionCredential {
    let challenge = service.challenge(ROUTE, &PRICE, None).await.unwrap();
    let header = credential_header(&challenge, payload, None);
    let bytes = protocol::b64url_decode(header.strip_prefix("Payment ").unwrap()).unwrap();
    let mut envelope: Value = serde_json::from_slice(&bytes).unwrap();
    envelope["challenge"]["intent"] = json!("session");
    envelope["challenge"]
        .as_object_mut()
        .unwrap()
        .remove("digest");
    let header = format!(
        "Payment {}",
        protocol::b64url_encode(envelope.to_string().as_bytes())
    );
    let ParsedAuthorization::Session(credential) =
        protocol::parse_any_authorization(&header).unwrap()
    else {
        panic!("session envelope parsed as charge");
    };
    service
        .verify_credential(&credential, ROUTE, &PRICE)
        .await
        .unwrap()
}

fn state() -> ChannelState {
    ChannelState {
        deposit: Vectors::load().int("deposit"),
        settled: 0,
        close_requested_at: 0,
    }
}

fn set_chain(rpc: &FakeTempo, state: ChannelState) {
    rpc.set_call_handler(Box::new(move |to, input| {
        let v = Vectors::load();
        assert_eq!(to, v.escrow());
        assert_eq!(
            input,
            abi::get_channel_state_calldata(&v.fixed("channelId"))
        );
        Ok([
            abi::uint_word(state.settled),
            abi::uint_word(state.deposit),
            abi::uint_word(u128::from(state.close_requested_at)),
        ]
        .concat())
    }));
}

async fn seed(repo: &dyn ChannelRepository, highest: u128, settled: u128) {
    let v = Vectors::load();
    repo.open_or_refresh(NewChannel {
        channel_id: v.fixed("channelId"),
        chain_id: v.chain_id(),
        escrow: v.escrow(),
        descriptor: v.descriptor(),
        deposit: v.int("deposit"),
        settled_on_chain: settled,
        highest_cumulative: highest,
        highest_signature: voucher_signature(highest, false),
    })
    .await
    .unwrap();
}

fn voucher_signature(amount: u128, wrong_signer: bool) -> [u8; 65] {
    let v = Vectors::load();
    let digest = voucher_digest(&v.escrow(), v.chain_id(), &v.fixed("channelId"), amount).unwrap();
    sign_digest(
        &v.secret_key(if wrong_signer { "payeeKey" } else { "payerKey" }),
        &digest,
    )
    .unwrap()
}

pub(super) fn voucher(amount: u128) -> Value {
    let v = Vectors::load();
    json!({"action":"voucher", "channelId":v.text("channelId"),
        "descriptor":descriptor_json(&v.descriptor()), "cumulativeAmount":amount.to_string(),
        "signature":hex_prefixed(&voucher_signature(amount, false))})
}

fn open() -> Value {
    let v = Vectors::load();
    let mut payload = voucher(INITIAL);
    payload["action"] = json!("open");
    payload["type"] = json!("transaction");
    payload["transaction"] = json!(v.text("open.serialized"));
    payload
}

pub(super) fn close(amount: u128) -> Value {
    let v = Vectors::load();
    let mut payload = voucher(amount);
    payload["action"] = json!("close");
    let digest =
        close_authorization_digest(&v.escrow(), v.chain_id(), &v.fixed("channelId"), amount)
            .unwrap();
    payload["closeSignature"] = json!(hex_prefixed(
        &sign_digest(&v.secret_key("payerKey"), &digest).unwrap()
    ));
    payload
}

fn resigned(edit: impl FnOnce(&mut TempoTx), payer: bool) -> Vec<u8> {
    let v = Vectors::load();
    let (mut tx, _) = decode_signed(&v.bytes("open.serialized")).unwrap();
    edit(&mut tx);
    let signature = sign_digest(
        &v.secret_key(if payer { "payerKey" } else { "payeeKey" }),
        &tx.signing_hash(),
    )
    .unwrap();
    tx.serialize_signed(&signature)
}

pub(super) fn receipt(raw: &[u8]) -> RpcReceipt {
    RpcReceipt {
        tx_hash: tx_hash(raw),
        block_number: 1,
        success: true,
        from: Vectors::load().fixed("payer"),
        logs: vec![],
    }
}

#[tokio::test]
async fn vector_open_persists_channel_and_receipt_without_running_inference() {
    let f = Fixture::new();
    let v = Vectors::load();
    f.rpc.mine(receipt(&v.bytes("open.serialized")));
    let credential = f.credential(open()).await;
    let result = f.service.open(&credential).await.unwrap();
    assert_eq!(result.receipt.channel_id, v.fixed::<32>("channelId"));
    assert_eq!(result.receipt.accepted_cumulative, INITIAL);
    assert_eq!(result.receipt.challenge_id, credential.challenge_id());
    assert_eq!(result.receipt.tx_hash, Some(v.fixed("open.txHash")));
    let row = f.record().await;
    assert_eq!(row.descriptor, v.descriptor());
    assert_eq!(
        (row.deposit, row.spent, row.reserved, row.units),
        (v.int("deposit"), 0, 0, 0)
    );
    assert_eq!(row.state, ChannelLifecycle::Open);
    assert_eq!(f.rpc.send_calls(), 1);
}

#[tokio::test]
async fn open_refuses_wrong_payee_token_chain_and_operator_before_broadcast() {
    for (case, expected) in [
        (0, OpenMismatch::Payee),
        (1, OpenMismatch::Token),
        (2, OpenMismatch::ChainId),
        (3, OpenMismatch::Operator),
    ] {
        let f = Fixture::new();
        let raw = resigned(
            |tx| {
                let v = Vectors::load();
                if case == 2 {
                    tx.chain_id += 1;
                    return;
                }
                let mut d = v.descriptor();
                match case {
                    0 => d.payee[0] ^= 1,
                    1 => d.token[0] ^= 1,
                    _ => d.operator[0] = 1,
                }
                tx.calls[0].input = abi::open_calldata(
                    &d.payee,
                    &d.operator,
                    &d.token,
                    v.int("deposit"),
                    &d.salt,
                    &d.authorized_signer,
                )
                .unwrap();
            },
            true,
        );
        let mut payload = open();
        payload["transaction"] = json!(hex_prefixed(&raw));
        let credential = f.credential(payload).await;
        assert_eq!(
            f.service.open(&credential).await.err(),
            Some(MppError::OpenRejected(expected)),
            "case {case}"
        );
        assert_eq!(f.rpc.send_calls(), 0);
        assert!(f
            .repo
            .get(&Vectors::load().fixed("channelId"))
            .await
            .unwrap()
            .is_none());
    }
}

#[tokio::test]
async fn open_refuses_descriptor_id_and_bad_initial_signer() {
    for case in 0..4 {
        let f = Fixture::new();
        let mut payload = open();
        match case {
            0 => {
                payload["descriptor"]["salt"] = json!(hex_prefixed(&[0x33; 32]));
            }
            1 => {
                payload["channelId"] = json!(hex_prefixed(&[0x44; 32]));
            }
            2 => {
                payload["signature"] = json!(hex_prefixed(&voucher_signature(INITIAL, true)));
            }
            _ => {
                payload["signature"] = json!(hex_prefixed(&[0; 65]));
            }
        }
        let credential = f.credential(payload).await;
        let error = f.service.open(&credential).await.err().unwrap();
        assert!(
            match case {
                0 => matches!(
                    error,
                    MppError::SessionMismatch(
                        SessionMismatch::Descriptor | SessionMismatch::ChannelId
                    )
                ),
                1 => matches!(error, MppError::SessionMismatch(SessionMismatch::ChannelId)),
                2 => matches!(
                    error,
                    MppError::VoucherRejected(VoucherReason::SignerMismatch)
                ),
                _ => matches!(
                    error,
                    MppError::VoucherRejected(VoucherReason::InvalidSignature)
                ),
            },
            "case {case}: {error:?}"
        );
        assert_eq!(f.rpc.send_calls(), 0);
    }
}

#[tokio::test]
async fn open_rejects_initial_voucher_above_deposit() {
    let f = Fixture::new();
    let amount = Vectors::load().int("deposit") + 1;
    let mut payload = open();
    payload["cumulativeAmount"] = json!(amount.to_string());
    payload["signature"] = json!(hex_prefixed(&voucher_signature(amount, false)));
    let credential = f.credential(payload).await;
    assert_eq!(
        f.service.open(&credential).await.err(),
        Some(MppError::VoucherRejected(VoucherReason::AboveDeposit))
    );
    assert_eq!(f.rpc.send_calls(), 0);
}

#[tokio::test]
async fn voucher_amount_rules_refuse_above_deposit_below_settled_replay_and_small_delta() {
    for (amount, settled, expected) in [
        (5_000_001, 0, VoucherReason::AboveDeposit),
        (
            INITIAL + DELTA,
            INITIAL + DELTA + 1,
            VoucherReason::BelowSettled,
        ),
        (INITIAL - 1, 0, VoucherReason::NotHigher),
        (INITIAL + DELTA - 1, 0, VoucherReason::DeltaTooSmall),
    ] {
        let f = Fixture::new();
        seed(&*f.repo, INITIAL, settled).await;
        set_chain(&f.rpc, ChannelState { settled, ..state() });
        let credential = f.credential(voucher(amount)).await;
        let error = f
            .service
            .voucher(&credential, 10, Uuid::new_v4())
            .await
            .err()
            .unwrap();
        if expected == VoucherReason::DeltaTooSmall {
            let MppError::VoucherDeltaTooSmall(snapshot) = error else {
                panic!("expected delta snapshot: {error:?}")
            };
            assert_eq!(snapshot.required_cumulative, INITIAL + DELTA);
        } else {
            assert_eq!(
                error,
                MppError::VoucherRejected(expected),
                "amount {amount}"
            );
        }
        let row = f.record().await;
        assert_eq!(
            (row.highest_cumulative, row.spent, row.reserved),
            (INITIAL, 0, 0)
        );
    }
}

#[tokio::test]
async fn exact_voucher_replay_reserves_again_but_changed_signature_does_not() {
    let f = Fixture::new();
    f.seed().await;
    let credential = f.credential(voucher(INITIAL)).await;
    let first = f
        .service
        .voucher(&credential, 10, Uuid::new_v4())
        .await
        .unwrap();
    let second = f
        .service
        .voucher(&credential, 10, Uuid::new_v4())
        .await
        .unwrap();
    assert_eq!(
        (first.reserved, second.reserved, f.record().await.reserved),
        (10, 10, 20)
    );
    let mut changed = voucher(INITIAL);
    changed["signature"] = json!(hex_prefixed(&voucher_signature(INITIAL, true)));
    let changed = f.credential(changed).await;
    assert_eq!(
        f.service.voucher(&changed, 10, Uuid::new_v4()).await.err(),
        Some(MppError::VoucherRejected(VoucherReason::NotHigher))
    );
    assert_eq!(f.record().await.reserved, 20);
}

#[tokio::test]
async fn concurrent_reservations_preserve_balance_and_exact_shortfall_snapshot() {
    let f = Fixture::new();
    f.seed().await;
    // Pre-warm the chain cache so both futures compete at the ledger reservation boundary.
    f.repo
        .record_chain_state(&Vectors::load().fixed("channelId"), state(), Utc::now())
        .await
        .unwrap();
    let credential = f.credential(voucher(INITIAL)).await;
    let start = tokio::sync::Barrier::new(3);
    let reserve = |id| {
        let (start, service, credential) = (&start, &f.service, &credential);
        async move {
            start.wait().await;
            service.voucher(credential, 750_000, id).await
        }
    };
    let (a, b, _) = tokio::join!(
        reserve(Uuid::new_v4()),
        reserve(Uuid::new_v4()),
        start.wait()
    );
    let results = [a, b];
    assert_eq!(results.iter().filter(|result| result.is_ok()).count(), 1);
    let error = results.into_iter().find_map(Result::err).unwrap();
    let MppError::VoucherTooLow(snapshot) = error else {
        panic!("expected shortfall, got {error:?}")
    };
    assert_eq!(snapshot.required_cumulative, 1_500_000);
    assert_eq!(snapshot.accepted_cumulative, INITIAL);
    assert_eq!(
        (snapshot.spent, snapshot.highest_voucher.cumulative_amount),
        (750_000, INITIAL)
    );
    let row = f.record().await;
    assert_eq!((row.spent, row.reserved), (750_000, 750_000));
}

#[tokio::test]
async fn voucher_accepts_exact_minimum_delta_and_deposit_boundary() {
    for amount in [INITIAL + DELTA, Vectors::load().int("deposit")] {
        let f = Fixture::new();
        f.seed().await;
        let credential = f.credential(voucher(amount)).await;
        let request = Uuid::new_v4();
        let reserved = f.service.voucher(&credential, 10, request).await.unwrap();
        assert_eq!(
            (reserved.channel_id, reserved.request_id, reserved.reserved),
            (Vectors::load().fixed("channelId"), request, 10)
        );
        assert_eq!(f.record().await.highest_cumulative, amount);
    }
}

#[tokio::test]
async fn voucher_rejects_invalid_signature_and_wrong_signer_without_reserving() {
    for wrong_signer in [false, true] {
        let f = Fixture::new();
        f.seed().await;
        let amount = INITIAL + DELTA;
        let mut payload = voucher(amount);
        payload["signature"] = json!(hex_prefixed(&if wrong_signer {
            voucher_signature(amount, true)
        } else {
            [0; 65]
        }));
        let credential = f.credential(payload).await;
        assert_eq!(
            f.service
                .voucher(&credential, 10, Uuid::new_v4())
                .await
                .err(),
            Some(MppError::VoucherRejected(if wrong_signer {
                VoucherReason::SignerMismatch
            } else {
                VoucherReason::InvalidSignature
            }))
        );
        assert_eq!(f.record().await.spent, 0);
    }
}

#[tokio::test]
async fn vouchers_refuse_missing_closing_finalized_and_chain_closed_channels() {
    for case in 0..6 {
        let f = Fixture::new();
        if case > 0 {
            f.seed().await;
        }
        let id = Vectors::load().fixed("channelId");
        match case {
            1 => {
                f.repo
                    .mark_closing(
                        &id,
                        PendingClose {
                            tx_hash: [9; 32],
                            cumulative: INITIAL,
                            capture: 0,
                            valid_before: 1,
                        },
                    )
                    .await
                    .unwrap();
            }
            2 => {
                f.repo.finalize(&id, 0).await.unwrap();
            }
            3 => set_chain(
                &f.rpc,
                ChannelState {
                    close_requested_at: 1,
                    ..state()
                },
            ),
            4 => set_chain(
                &f.rpc,
                ChannelState {
                    deposit: 0,
                    ..state()
                },
            ),
            5 => {
                f.repo
                    .record_chain_state(
                        &id,
                        ChannelState {
                            close_requested_at: 1,
                            ..state()
                        },
                        Utc::now(),
                    )
                    .await
                    .unwrap();
            }
            _ => {}
        }
        let credential = f.credential(voucher(INITIAL + DELTA)).await;
        let error = f
            .service
            .voucher(&credential, 10, Uuid::new_v4())
            .await
            .err()
            .unwrap();
        assert!(
            match case {
                0 => matches!(error, MppError::ChannelNotFound),
                1 | 3 | 5 => matches!(error, MppError::ChannelClosing),
                _ => matches!(error, MppError::ChannelClosed),
            },
            "case {case}: {error:?}"
        );
        if case > 0 {
            assert_eq!(f.record().await.spent, 0);
        }
    }
}

#[tokio::test]
async fn chain_cache_reuses_fresh_read_and_refreshes_after_sixty_seconds() {
    let f = Fixture::new();
    f.seed().await;
    let id = Vectors::load().fixed("channelId");
    f.repo
        .record_chain_state(&id, state(), Utc::now())
        .await
        .unwrap();
    let first = f.credential(voucher(INITIAL + DELTA)).await;
    let reserved = f.service.voucher(&first, 10, Uuid::new_v4()).await.unwrap();
    assert_eq!(f.rpc.eth_calls(), 0);
    f.service.release(&reserved).await.unwrap();
    f.repo
        .record_chain_state(&id, state(), Utc::now() - chrono::Duration::seconds(61))
        .await
        .unwrap();
    set_chain(
        &f.rpc,
        ChannelState {
            close_requested_at: 1,
            ..state()
        },
    );
    let second = f.credential(voucher(INITIAL + 2 * DELTA)).await;
    assert!(matches!(
        f.service.voucher(&second, 10, Uuid::new_v4()).await,
        Err(MppError::ChannelClosing)
    ));
    assert_eq!(f.rpc.eth_calls(), 1);
}

#[tokio::test]
async fn shortfall_snapshot_has_exact_required_cumulative_and_no_reservation() {
    let f = Fixture::new();
    f.seed().await;
    let amount = INITIAL + DELTA;
    let request = Uuid::new_v4();
    assert!(matches!(
        f.repo
            .reserve(&Vectors::load().fixed("channelId"), request, 1_000_000)
            .await
            .unwrap(),
        ReserveOutcome::Reserved(_)
    ));
    let credential = f.credential(voucher(amount)).await;
    let error = f
        .service
        .voucher(&credential, 300_000, Uuid::new_v4())
        .await
        .err()
        .unwrap();
    let MppError::VoucherTooLow(snapshot) = error else {
        panic!("expected snapshot, got {error:?}")
    };
    assert_eq!(snapshot.required_cumulative, 1_300_000);
    assert_eq!(snapshot.spent, 1_000_000);
    assert_eq!(snapshot.accepted_cumulative, amount);
    assert_eq!(snapshot.highest_voucher.cumulative_amount, amount);
    assert_eq!(f.record().await.reserved, 1_000_000);
}

#[tokio::test]
async fn metering_uses_total_tokens_releases_unused_cap_and_cannot_double_charge() {
    let f = Fixture::new();
    f.seed().await;
    let credential = f.credential(voucher(INITIAL + DELTA)).await;
    let reserved = f
        .service
        .voucher(&credential, 1000, Uuid::new_v4())
        .await
        .unwrap();
    assert_eq!(
        (f.record().await.spent, f.record().await.reserved),
        (1000, 1000)
    );
    let receipt = f.service.charge(&reserved, 7, 11).await.unwrap();
    assert_eq!((receipt.spent, receipt.units), (36, 18));
    assert_eq!(
        (
            f.record().await.spent,
            f.record().await.reserved,
            f.record().await.units
        ),
        (36, 0, 18)
    );
    assert!(matches!(
        f.service.charge(&reserved, 7, 11).await,
        Err(MppError::ReservationMissing)
    ));
    assert_eq!(f.record().await.spent, 36);
}

#[tokio::test]
async fn release_restores_balance_and_stale_reservations_cannot_charge() {
    let f = Fixture::new();
    f.seed().await;
    let credential = f.credential(voucher(INITIAL + DELTA)).await;
    let reserved = f
        .service
        .voucher(&credential, 1000, Uuid::new_v4())
        .await
        .unwrap();
    f.service.release(&reserved).await.unwrap();
    assert_eq!((f.record().await.spent, f.record().await.reserved), (0, 0));
    assert!(matches!(
        f.service.charge(&reserved, 100, 100).await,
        Err(MppError::ReservationMissing)
    ));
}

#[tokio::test]
async fn close_below_capture_is_rejected_before_broadcast() {
    let f = Fixture::new();
    f.seed().await;
    // A higher settled amount can arrive from another instance after the voucher was issued.
    f.repo
        .record_chain_state(
            &Vectors::load().fixed("channelId"),
            ChannelState {
                settled: INITIAL,
                ..state()
            },
            Utc::now(),
        )
        .await
        .unwrap();
    let credential = f.credential(close(INITIAL - 1)).await;
    assert_eq!(
        f.service.close(&credential).await.err(),
        Some(MppError::CloseRejected(CloseReason::BelowCapture))
    );
    assert_eq!(f.rpc.send_calls(), 0);
    assert_eq!(f.record().await.state, ChannelLifecycle::Open);
}

#[tokio::test]
async fn close_refuses_bad_authorization_and_inflight_reservations() {
    for case in 0..6 {
        let f = Fixture::new();
        f.seed().await;
        let mut payload = close(match case {
            3 => Vectors::load().int("deposit") + 1,
            4 => INITIAL - 1,
            5 => INITIAL + DELTA - 1,
            _ => INITIAL,
        });
        if case == 0 {
            payload["closeSignature"] = json!(hex_prefixed(&[0; 65]));
        }
        if case == 1 {
            let v = Vectors::load();
            let digest = close_authorization_digest(
                &v.escrow(),
                v.chain_id(),
                &v.fixed("channelId"),
                INITIAL,
            )
            .unwrap();
            payload["closeSignature"] = json!(hex_prefixed(
                &sign_digest(&v.secret_key("payeeKey"), &digest).unwrap()
            ));
        }
        if case == 2 {
            f.repo
                .reserve(&Vectors::load().fixed("channelId"), Uuid::new_v4(), 10)
                .await
                .unwrap();
        }
        if case == 4 {
            // Closing may reduce unused authorization, but must cover captured usage.
            let request = Uuid::new_v4();
            f.repo
                .reserve(&Vectors::load().fixed("channelId"), request, INITIAL)
                .await
                .unwrap();
            f.repo.charge(request, INITIAL, 0).await.unwrap();
        }
        let credential = f.credential(payload).await;
        assert_eq!(
            f.service.close(&credential).await.err(),
            Some(match case {
                0 => MppError::CloseRejected(CloseReason::InvalidSignature),
                1 => MppError::CloseRejected(CloseReason::SignerMismatch),
                2 => MppError::CloseRejected(CloseReason::Busy),
                3 => MppError::VoucherRejected(VoucherReason::AboveDeposit),
                4 => MppError::CloseRejected(CloseReason::BelowCapture),
                _ => MppError::VoucherRejected(VoucherReason::DeltaTooSmall),
            })
        );
        assert_eq!(f.rpc.send_calls(), 0);
    }
}

struct ObservedCloseRpc {
    inner: Arc<FakeTempo>,
    repo: Arc<InMemoryChannelRepository>,
    observed: AtomicBool,
    fail: bool,
}

#[async_trait]
impl TempoRpc for ObservedCloseRpc {
    async fn send_raw_transaction_sync(&self, raw: &[u8]) -> Result<RpcReceipt, RpcError> {
        let row = self
            .repo
            .get(&Vectors::load().fixed("channelId"))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(row.state, ChannelLifecycle::Closing);
        let pending = row.pending_close.unwrap();
        assert_eq!(pending.tx_hash, tx_hash(raw));
        assert_eq!(pending.capture, 36);
        self.observed.store(true, Ordering::SeqCst);
        let mut mined = receipt(raw);
        mined.success = !self.fail;
        self.inner.mine(mined);
        self.inner.send_raw_transaction_sync(raw).await
    }
    async fn get_transaction_receipt(
        &self,
        hash: [u8; 32],
    ) -> Result<Option<RpcReceipt>, RpcError> {
        self.inner.get_transaction_receipt(hash).await
    }
    async fn get_block_timestamp(&self, number: u64) -> Result<Option<u64>, RpcError> {
        self.inner.get_block_timestamp(number).await
    }
    async fn call(&self, to: [u8; 20], data: &[u8]) -> Result<Vec<u8>, RpcError> {
        self.inner.call(to, data).await
    }
    async fn gas_price(&self) -> Result<u128, RpcError> {
        self.inner.gas_price().await
    }
}

#[tokio::test]
async fn close_marks_pending_before_send_and_failure_clears_closing() {
    for (fail, already_settled) in [(false, false), (true, false), (false, true)] {
        let f = Fixture::new();
        f.seed().await;
        let id = Vectors::load().fixed("channelId");
        if already_settled {
            f.repo
                .record_chain_state(
                    &id,
                    ChannelState {
                        settled: 36,
                        ..state()
                    },
                    Utc::now(),
                )
                .await
                .unwrap();
        } else {
            let request = Uuid::new_v4();
            f.repo.reserve(&id, request, 100).await.unwrap();
            assert!(matches!(
                f.repo.charge(request, 36, 18).await.unwrap(),
                ChargeOutcome::Charged { charged: 36, .. }
            ));
        }
        let rpc = Arc::new(ObservedCloseRpc {
            inner: f.rpc.clone(),
            repo: f.repo.clone(),
            observed: AtomicBool::new(false),
            fail,
        });
        let service = service(config(), f.repo.clone(), rpc.clone());
        let credential = verified(&service, close(INITIAL)).await;
        let result = service.close(&credential).await;
        assert!(rpc.observed.load(Ordering::SeqCst));
        let row = f.record().await;
        if fail {
            assert!(matches!(result, Err(MppError::ChainTxFailed)));
            assert_eq!(row.state, ChannelLifecycle::Open);
            assert!(row.pending_close.is_none());
        } else {
            let receipt = result.unwrap().receipt;
            assert_eq!(
                (receipt.spent, receipt.units),
                if already_settled { (0, 0) } else { (36, 18) }
            );
            assert!(receipt.tx_hash.is_some());
            assert_eq!(row.state, ChannelLifecycle::Finalized);
            assert_eq!(row.settled_on_chain, 36);
        }
    }
}

fn top_up_input(descriptor: &ChannelDescriptor, amount: u128) -> Vec<u8> {
    let mut input = vec![0xdc, 0x48, 0x47, 0x1e];
    for address in [
        descriptor.payer,
        descriptor.payee,
        descriptor.operator,
        descriptor.token,
    ] {
        input.extend(abi::address_word(&address));
    }
    input.extend(descriptor.salt);
    input.extend(abi::address_word(&descriptor.authorized_signer));
    input.extend(descriptor.expiring_nonce_hash);
    input.extend(abi::uint_word(amount));
    input
}

fn top_up(raw: &[u8], additional: u128) -> Value {
    let v = Vectors::load();
    json!({"action":"topUp","type":"transaction", "channelId":v.text("channelId"),
        "descriptor":descriptor_json(&v.descriptor()), "additionalDeposit":additional.to_string(), "transaction":hex_prefixed(raw)})
}

#[tokio::test]
async fn top_up_validates_call_descriptor_sender_amount_and_chain_before_send() {
    for (case, expected) in [
        (0, TopUpReason::Selector),
        (1, TopUpReason::Descriptor),
        (2, TopUpReason::Sender),
        (3, TopUpReason::Amount),
        (4, TopUpReason::ChainId),
        (5, TopUpReason::CallCount),
        (6, TopUpReason::Target),
        (7, TopUpReason::Value),
        (8, TopUpReason::Calldata),
        (9, TopUpReason::ExpiringNonce),
    ] {
        let f = Fixture::new();
        f.seed().await;
        let raw = resigned(
            |tx| {
                let mut d = Vectors::load().descriptor();
                if case == 1 {
                    d.salt[0] ^= 1;
                }
                tx.calls[0].input = top_up_input(&d, if case == 3 { 201 } else { 200 });
                match case {
                    0 => tx.calls[0].input[0] ^= 1,
                    4 => tx.chain_id += 1,
                    5 => tx.calls.push(tx.calls[0].clone()),
                    6 => tx.calls[0].to[0] ^= 1,
                    7 => tx.calls[0].value = 1,
                    8 => {
                        tx.calls[0].input.pop();
                    }
                    9 => tx.nonce_key = [0; 32],
                    _ => {}
                }
            },
            case != 2,
        );
        let credential = f.credential(top_up(&raw, 200)).await;
        assert_eq!(
            f.service.top_up(&credential).await.err(),
            Some(MppError::TopUpRejected(expected)),
            "case {case}"
        );
        assert_eq!(f.rpc.send_calls(), 0);
        assert_eq!(f.record().await.deposit, Vectors::load().int("deposit"));
    }
}

#[tokio::test]
async fn valid_top_up_broadcasts_and_refreshes_deposit_without_spending() {
    let f = Fixture::new();
    f.seed().await;
    let raw = resigned(
        |tx| tx.calls[0].input = top_up_input(&Vectors::load().descriptor(), 200),
        true,
    );
    f.rpc.mine(receipt(&raw));
    set_chain(
        &f.rpc,
        ChannelState {
            deposit: Vectors::load().int("deposit") + 200,
            ..state()
        },
    );
    let credential = f.credential(top_up(&raw, 200)).await;
    let receipt = f.service.top_up(&credential).await.unwrap().receipt;
    assert_eq!(receipt.tx_hash, Some(tx_hash(&raw)));
    assert_eq!(receipt.accepted_cumulative, INITIAL);
    let row = f.record().await;
    assert_eq!((row.deposit, row.spent, row.reserved), (5_000_200, 0, 0));
    assert_eq!(f.rpc.send_calls(), 1);
}
