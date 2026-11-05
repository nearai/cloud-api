// INTEGRATION: register directly as session_service::validation_tests.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use chrono::Utc;
use config::{MppModelPrice, MppSessionSettings};
use serde_json::{json, Value};
use uuid::Uuid;

use super::{SessionService, SessionServiceConfig};
use crate::mpp::abi::{self, ChannelState};
use crate::mpp::channel_memory::InMemoryChannelRepository;
use crate::mpp::eip712::{voucher_digest, MAX_UINT96};
use crate::mpp::error::{
    CloseReason, MppError, MppResponse, ProblemStatus, SessionMismatch, TopUpReason, VoucherReason,
    BACKEND_RETRY_AFTER,
};
use crate::mpp::open_check::OpenMismatch;
use crate::mpp::ports::{ChannelRepository, NewChannel, RpcReceipt};
use crate::mpp::pricing::{LimitViolation, Price};
use crate::mpp::protocol::{self, ParsedAuthorization, ProblemCode};
use crate::mpp::secp256k1::sign_digest;
use crate::mpp::session_vectors::Vectors;
use crate::mpp::session_wire::{
    descriptor_json, hex_prefixed, HighestVoucher, SessionSnapshot, SessionTerms,
};
use crate::mpp::signer::LocalSigner;
use crate::mpp::testing::{credential_header, FakeTempo, SendMode};

const ROUTE: &str = "/v1/chat/completions";
const PRICE: Price = Price {
    amount: 0,
    token_price: Some(2),
};
const HIGH_PRICE: Price = Price {
    amount: 0,
    token_price: Some(7),
};
const NO_TOKEN: Price = Price {
    amount: 10,
    token_price: None,
};
const INITIAL: u128 = 1_250_000;

fn cfg() -> SessionServiceConfig {
    let v = Vectors::load();
    SessionServiceConfig {
        settings: MppSessionSettings {
            signer_key: None,
            min_voucher_delta: Some(100),
            suggested_deposit: v.int("deposit"),
            settle_interval_secs: 3600,
            settle_amount: 5_000_000,
            close_supervision_secs: 60,
        },
        model_prices: [
            ("cheap", PRICE),
            ("expensive", HIGH_PRICE),
            ("charge", NO_TOKEN),
        ]
        .into_iter()
        .map(|(name, price)| {
            (
                name.into(),
                MppModelPrice {
                    amount: price.amount,
                    token_price: price.token_price,
                    max_tokens: 1000,
                    max_request_bytes: 4096,
                },
            )
        })
        .collect(),
        realm: "session.example".into(),
        secret_key: vec![0x53; 32],
        challenge_ttl: Duration::from_secs(300),
        chain_id: v.chain_id(),
        escrow: v.escrow(),
        currency: v.fixed("token"),
    }
}

struct Fixture {
    service: SessionService,
    repo: Arc<InMemoryChannelRepository>,
    rpc: Arc<FakeTempo>,
}

fn build(
    config: SessionServiceConfig,
    repo: Arc<InMemoryChannelRepository>,
    rpc: Arc<FakeTempo>,
) -> Result<SessionService, MppError> {
    SessionService::new(
        config,
        repo,
        rpc,
        Arc::new(LocalSigner::new(Vectors::load().secret_key("payeeKey"))),
    )
}

impl Fixture {
    fn new() -> Self {
        let repo = Arc::new(InMemoryChannelRepository::new());
        let rpc = Arc::new(FakeTempo::default());
        let service = build(cfg(), repo.clone(), rpc.clone()).unwrap();
        Self { service, repo, rpc }
    }

    fn untouched(&self) {
        assert_eq!(self.repo.channel_count(), 0);
        assert_eq!(self.repo.reservation_count(), 0);
        assert_eq!(self.rpc.send_calls(), 0);
        assert_eq!(self.rpc.eth_calls(), 0);
        assert_eq!(self.rpc.lookup_calls(), 0);
    }

    async fn credential(&self, payload: Value) -> protocol::SessionCredential {
        let challenge = self.service.challenge(ROUTE, &PRICE, None).await.unwrap();
        parse_envelope(&challenge, payload)
    }

    async fn verified(&self, payload: Value) -> super::SessionCredential {
        let credential = self.credential(payload).await;
        self.service
            .verify_credential(&credential, ROUTE, &PRICE)
            .await
            .unwrap()
    }

    async fn seed(&self) {
        let v = Vectors::load();
        self.repo
            .open_or_refresh(NewChannel {
                channel_id: v.fixed("channelId"),
                chain_id: v.chain_id(),
                escrow: v.escrow(),
                descriptor: v.descriptor(),
                deposit: v.int("deposit"),
                settled_on_chain: 0,
                highest_cumulative: INITIAL,
                highest_signature: signature(INITIAL),
            })
            .await
            .unwrap();
    }
}

fn parse_envelope(challenge: &protocol::Challenge, payload: Value) -> protocol::SessionCredential {
    let header = credential_header(challenge, payload, None);
    let mut envelope: Value = serde_json::from_slice(
        &protocol::b64url_decode(header.strip_prefix("Payment ").unwrap()).unwrap(),
    )
    .unwrap();
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
        panic!("wrong intent")
    };
    credential
}

fn resign(credential: &mut protocol::SessionCredential) {
    credential.challenge.id =
        protocol::compute_challenge_id(&cfg().secret_key, &credential.challenge.bound_fields())
            .unwrap();
}

fn edit_request(credential: &mut protocol::SessionCredential, edit: impl FnOnce(&mut Value)) {
    let mut request: Value =
        serde_json::from_slice(&protocol::b64url_decode(&credential.challenge.request).unwrap())
            .unwrap();
    edit(&mut request);
    credential.challenge.request =
        protocol::b64url_encode(protocol::canonical_json(&request).unwrap().as_bytes());
    resign(credential);
}

fn signature(amount: u128) -> [u8; 65] {
    let v = Vectors::load();
    sign_digest(
        &v.secret_key("payerKey"),
        &voucher_digest(&v.escrow(), v.chain_id(), &v.fixed("channelId"), amount).unwrap(),
    )
    .unwrap()
}

fn voucher(amount: u128) -> Value {
    let v = Vectors::load();
    json!({"action":"voucher","channelId":v.text("channelId"),"descriptor":descriptor_json(&v.descriptor()),
        "cumulativeAmount":amount.to_string(),"signature":hex_prefixed(&signature(amount))})
}

fn open(amount: u128) -> Value {
    let mut payload = voucher(amount);
    payload["action"] = json!("open");
    payload["type"] = json!("transaction");
    payload["transaction"] = json!(Vectors::load().text("open.serialized"));
    payload
}

fn chain_state() -> ChannelState {
    ChannelState {
        settled: 0,
        deposit: Vectors::load().int("deposit"),
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

fn mine_open(rpc: &FakeTempo, success: bool) {
    let v = Vectors::load();
    rpc.mine(RpcReceipt {
        tx_hash: v.fixed("open.txHash"),
        block_number: 1,
        success,
        from: v.fixed("payer"),
        logs: vec![],
    });
}

fn snapshot() -> SessionSnapshot {
    let v = Vectors::load();
    SessionSnapshot {
        accepted_cumulative: INITIAL,
        chain_id: v.chain_id(),
        channel_id: v.fixed("channelId"),
        deposit: v.int("deposit"),
        descriptor: v.descriptor(),
        escrow: v.escrow(),
        highest_voucher: HighestVoucher {
            channel_id: v.fixed("channelId"),
            cumulative_amount: INITIAL,
            signature: signature(INITIAL),
        },
        required_cumulative: INITIAL + 100,
        settled: 0,
        spent: INITIAL,
        units: 123,
    }
}

#[tokio::test]
async fn verification_rejects_tampered_hmac_and_invalid_echo_fields_without_side_effects() {
    for case in 0..11 {
        let f = Fixture::new();
        let mut credential = f.credential(voucher(INITIAL)).await;
        match case {
            0 => credential.challenge.id = protocol::b64url_encode(&[0; 32]),
            1 => credential.challenge.expires = Some("not-a-timestamp".into()),
            2 => credential.challenge.expires = Some("2000-01-01T00:00:00Z".into()),
            3 => credential.challenge.realm = "foreign.example".into(),
            4 => credential.challenge.method = "foreign-method".into(),
            5 => credential.challenge.intent = "charge".into(),
            6 => credential.challenge.digest = Some("sha-256=:Zm9yZWlnbg==:".into()),
            7 => {
                credential.challenge.opaque = Some(
                    protocol::opaque_for("/v1/responses", &[0x31; protocol::OPAQUE_NONCE_BYTES])
                        .unwrap(),
                )
            }
            8 => credential.challenge.expires = None,
            9 => credential.challenge.opaque = None,
            _ => credential.challenge.request.push('!'),
        }
        if (1..10).contains(&case) {
            resign(&mut credential);
        }
        let error = f
            .service
            .verify_credential(&credential, ROUTE, &PRICE)
            .await
            .err()
            .unwrap();
        assert!(
            match case {
                2 => matches!(error, MppError::Expired),
                4 => matches!(error, MppError::MethodUnsupported),
                _ => matches!(error, MppError::InvalidChallenge(_)),
            },
            "case {case}: {error:?}"
        );
        f.untouched();
    }
}

#[tokio::test]
async fn properly_signed_offer_changes_are_rejected_on_semantics() {
    for field in [
        "amount",
        "currency",
        "recipient",
        "chainId",
        "escrowContract",
        "suggestedDeposit",
        "minVoucherDelta",
    ] {
        let f = Fixture::new();
        let mut credential = f.credential(voucher(INITIAL)).await;
        edit_request(&mut credential, |request| match field {
            "amount" => request["amount"] = json!("3"),
            "currency" => request["currency"] = json!(hex_prefixed(&[0x42; 20])),
            "recipient" => request["recipient"] = json!(hex_prefixed(&[0x43; 20])),
            "chainId" => {
                request["methodDetails"]["chainId"] = json!(Vectors::load().chain_id() + 1)
            }
            "escrowContract" => {
                request["methodDetails"]["escrowContract"] = json!(hex_prefixed(&[0x44; 20]))
            }
            "suggestedDeposit" => request["suggestedDeposit"] = json!("6000000"),
            "minVoucherDelta" => request["methodDetails"]["minVoucherDelta"] = json!("101"),
            _ => unreachable!(),
        });
        assert!(protocol::verify_challenge_id(
            &cfg().secret_key,
            &credential.challenge.id,
            &credential.challenge.bound_fields()
        ));
        assert!(
            matches!(
                f.service
                    .verify_credential(&credential, ROUTE, &PRICE)
                    .await,
                Err(MppError::InvalidChallenge("request"))
            ),
            "field {field}"
        );
        f.untouched();
    }
}

#[tokio::test]
async fn malformed_payload_is_rejected_after_valid_challenge_authentication() {
    for payload in [
        json!(null),
        json!({}),
        json!({"action":"unknown"}),
        json!({"action":"voucher","channelId":"0x11"}),
    ] {
        let f = Fixture::new();
        let credential = f.credential(payload).await;
        assert!(matches!(
            f.service
                .verify_credential(&credential, ROUTE, &PRICE)
                .await,
            Err(MppError::Malformed(_))
        ));
        f.untouched();
    }
}

#[tokio::test]
async fn verification_ignores_signed_snapshot_but_binds_current_model_price() {
    let f = Fixture::new();
    let challenge = f
        .service
        .challenge(ROUTE, &PRICE, Some(snapshot()))
        .await
        .unwrap();
    let credential = parse_envelope(&challenge, voucher(INITIAL));
    let verified = f
        .service
        .verify_credential(&credential, ROUTE, &PRICE)
        .await
        .unwrap();
    assert_eq!(verified.challenge_id(), challenge.id);
    assert_eq!(
        verified.payload().channel_id(),
        &Vectors::load().fixed::<32>("channelId")
    );
    for price in [
        HIGH_PRICE,
        NO_TOKEN,
        Price {
            amount: 0,
            token_price: Some(99),
        },
    ] {
        assert!(matches!(
            f.service
                .verify_credential(&credential, ROUTE, &price)
                .await,
            Err(MppError::InvalidChallenge(_))
        ));
        f.untouched();
    }
    let expensive = f.service.challenge(ROUTE, &HIGH_PRICE, None).await.unwrap();
    let expensive = parse_envelope(&expensive, voucher(INITIAL));
    assert!(f
        .service
        .verify_credential(&expensive, ROUTE, &HIGH_PRICE)
        .await
        .is_ok());
    f.untouched();
}

#[tokio::test]
async fn default_minimum_delta_is_token_price_times_sixty_four() {
    let mut config = cfg();
    config.settings.min_voucher_delta = None;
    let repo = Arc::new(InMemoryChannelRepository::new());
    let rpc = Arc::new(FakeTempo::default());
    let service = build(config, repo.clone(), rpc.clone()).unwrap();
    for price in [PRICE, HIGH_PRICE] {
        let challenge = service.challenge(ROUTE, &price, None).await.unwrap();
        let terms = SessionTerms::from_b64(&challenge.request).unwrap();
        assert_eq!(terms.min_voucher_delta, price.token_price.unwrap() * 64);
    }
    assert_eq!(repo.channel_count(), 0);
    assert_eq!(rpc.eth_calls(), 0);
}

#[tokio::test]
async fn constructor_rejects_default_delta_overflow_and_wire_range_overflow() {
    for token_price in [u128::MAX, MAX_UINT96 / 64 + 1] {
        let mut config = cfg();
        config.settings.min_voucher_delta = None;
        config.model_prices = BTreeMap::from([(
            "overflow".into(),
            MppModelPrice {
                amount: 0,
                token_price: Some(token_price),
                max_tokens: 1,
                max_request_bytes: 1,
            },
        )]);
        let repo = Arc::new(InMemoryChannelRepository::new());
        let rpc = Arc::new(FakeTempo::default());
        assert!(matches!(
            build(config, repo.clone(), rpc.clone()),
            Err(MppError::Config)
        ));
        assert_eq!(repo.channel_count(), 0);
        assert_eq!(repo.reservation_count(), 0);
        assert_eq!(rpc.send_calls(), 0);
        assert_eq!(rpc.eth_calls(), 0);
    }
}

#[tokio::test]
async fn no_token_price_cannot_issue_session_challenge() {
    let f = Fixture::new();
    assert_eq!(
        f.service.challenge(ROUTE, &NO_TOKEN, None).await.err(),
        Some(MppError::Limit(LimitViolation::ModelNotPriced))
    );
    f.untouched();
}

#[tokio::test]
async fn voucher_descriptor_mismatch_does_not_read_chain_or_reserve() {
    let f = Fixture::new();
    f.seed().await;
    let mut payload = voucher(INITIAL + 100);
    payload["descriptor"]["salt"] = json!(hex_prefixed(&[0x22; 32]));
    let credential = f.verified(payload).await;
    assert_eq!(
        f.service
            .voucher(&credential, 10, Uuid::new_v4())
            .await
            .err(),
        Some(MppError::SessionMismatch(SessionMismatch::Descriptor))
    );
    assert_eq!(f.repo.reservation_count(), 0);
    assert_eq!(f.rpc.eth_calls(), 0);
    assert_eq!(f.rpc.send_calls(), 0);
    assert_eq!(
        f.repo
            .get(&Vectors::load().fixed("channelId"))
            .await
            .unwrap()
            .unwrap()
            .highest_cumulative,
        INITIAL
    );
}

#[tokio::test]
async fn first_open_voucher_is_not_subject_to_minimum_delta() {
    let f = Fixture::new();
    mine_open(&f.rpc, true);
    set_chain(&f.rpc, chain_state());
    let credential = f.verified(open(1)).await;
    let receipt = f.service.open(&credential).await.unwrap().receipt;
    assert_eq!(receipt.accepted_cumulative, 1);
    assert_eq!(receipt.spent, 0);
    assert_eq!(
        f.repo
            .get(&Vectors::load().fixed("channelId"))
            .await
            .unwrap()
            .unwrap()
            .highest_cumulative,
        1
    );
}

#[tokio::test]
async fn open_refuses_chain_zero_deposit_close_request_and_below_settled() {
    for case in 0..4 {
        let f = Fixture::new();
        mine_open(&f.rpc, true);
        let state = match case {
            0 => ChannelState {
                deposit: 0,
                ..chain_state()
            },
            1 => ChannelState {
                close_requested_at: 1,
                ..chain_state()
            },
            2 => ChannelState {
                settled: INITIAL + 1,
                ..chain_state()
            },
            _ => ChannelState {
                deposit: INITIAL - 1,
                ..chain_state()
            },
        };
        set_chain(&f.rpc, state);
        let credential = f.verified(open(INITIAL)).await;
        assert_eq!(
            f.service.open(&credential).await.err(),
            Some(match case {
                2 => MppError::VoucherRejected(VoucherReason::BelowSettled),
                3 => MppError::VoucherRejected(VoucherReason::AboveDeposit),
                _ => MppError::ChannelClosed,
            })
        );
        assert_eq!(f.rpc.send_calls(), 1);
        assert_eq!(f.repo.channel_count(), 0);
        assert_eq!(f.repo.reservation_count(), 0);
    }
}

#[tokio::test]
async fn open_reverted_receipt_does_not_create_channel_and_known_rejection_reconciles() {
    for reverted in [false, true] {
        let f = Fixture::new();
        mine_open(&f.rpc, !reverted);
        set_chain(&f.rpc, chain_state());
        if !reverted {
            f.rpc.set_send_mode(SendMode::Reject);
        }
        let credential = f.verified(open(INITIAL)).await;
        let result = f.service.open(&credential).await;
        if reverted {
            assert!(matches!(result, Err(MppError::ChainTxFailed)));
            assert_eq!(f.repo.channel_count(), 0);
            assert_eq!(f.rpc.eth_calls(), 0);
            assert_eq!(f.rpc.lookup_calls(), 0);
        } else {
            assert_eq!(
                result.unwrap().receipt.tx_hash,
                Some(Vectors::load().fixed("open.txHash"))
            );
            assert_eq!(f.repo.channel_count(), 1);
            assert_eq!(f.rpc.lookup_calls(), 1);
        }
        assert_eq!(f.rpc.send_calls(), 1);
    }
}

#[tokio::test]
async fn expired_chain_cache_refreshes_even_for_exact_voucher_replay() {
    let f = Fixture::new();
    f.seed().await;
    let id = Vectors::load().fixed("channelId");
    f.repo
        .record_chain_state(
            &id,
            chain_state(),
            Utc::now() - chrono::Duration::seconds(61),
        )
        .await
        .unwrap();
    set_chain(
        &f.rpc,
        ChannelState {
            close_requested_at: 1,
            ..chain_state()
        },
    );
    let credential = f.verified(voucher(INITIAL)).await;
    assert!(matches!(
        f.service.voucher(&credential, 10, Uuid::new_v4()).await,
        Err(MppError::ChannelClosing)
    ));
    assert_eq!(f.rpc.eth_calls(), 1);
    assert_eq!(f.repo.reservation_count(), 0);
    let row = f.repo.get(&id).await.unwrap().unwrap();
    assert_eq!(row.close_requested_at, 1);
    assert_eq!(row.spent, 0);
}

#[test]
fn session_error_mapping_preserves_problem_codes_status_kinds_and_details() {
    let cases = [
        (
            MppError::ChannelNotFound,
            MppResponse::Problem(ProblemStatus::Gone, ProblemCode::ChannelNotFound),
            "channel_not_found",
            "The channel was not found.",
        ),
        (
            MppError::ChannelClosed,
            MppResponse::Problem(ProblemStatus::Gone, ProblemCode::ChannelClosed),
            "channel_closed",
            "The channel is not accepting requests.",
        ),
        (
            MppError::ChannelClosing,
            MppResponse::Problem(ProblemStatus::Gone, ProblemCode::ChannelClosed),
            "channel_closing",
            "The channel is not accepting requests.",
        ),
        (
            MppError::VoucherRejected(VoucherReason::AboveDeposit),
            MppResponse::Challenge(ProblemCode::AmountExceedsDeposit),
            "amount_exceeds_deposit",
            "The voucher was rejected.",
        ),
        (
            MppError::VoucherRejected(VoucherReason::DeltaTooSmall),
            MppResponse::Challenge(ProblemCode::DeltaTooSmall),
            "delta_too_small",
            "The voucher was rejected.",
        ),
        (
            MppError::VoucherRejected(VoucherReason::InvalidSignature),
            MppResponse::Challenge(ProblemCode::InvalidSignature),
            "invalid_signature",
            "The voucher was rejected.",
        ),
        (
            MppError::VoucherRejected(VoucherReason::SignerMismatch),
            MppResponse::Challenge(ProblemCode::SignerMismatch),
            "signer_mismatch",
            "The voucher was rejected.",
        ),
        (
            MppError::VoucherRejected(VoucherReason::BelowSettled),
            MppResponse::Challenge(ProblemCode::VerificationFailed),
            "voucher_below_settled",
            "The voucher was rejected.",
        ),
        (
            MppError::VoucherRejected(VoucherReason::NotHigher),
            MppResponse::Challenge(ProblemCode::VerificationFailed),
            "voucher_not_higher",
            "The voucher was rejected.",
        ),
        (
            MppError::CloseRejected(CloseReason::BelowCapture),
            MppResponse::Challenge(ProblemCode::VerificationFailed),
            "close_below_capture",
            "The close was rejected.",
        ),
        (
            MppError::CloseRejected(CloseReason::InvalidSignature),
            MppResponse::Challenge(ProblemCode::InvalidSignature),
            "close_invalid_signature",
            "The close was rejected.",
        ),
        (
            MppError::CloseRejected(CloseReason::SignerMismatch),
            MppResponse::Challenge(ProblemCode::SignerMismatch),
            "close_signer_mismatch",
            "The close was rejected.",
        ),
        (
            MppError::CloseRejected(CloseReason::Busy),
            MppResponse::Retry(BACKEND_RETRY_AFTER),
            "close_busy",
            "The close was rejected.",
        ),
        (
            MppError::CloseRejected(CloseReason::StateChanged),
            MppResponse::Retry(BACKEND_RETRY_AFTER),
            "close_state_changed",
            "The close was rejected.",
        ),
        (
            MppError::ChainTxFailed,
            MppResponse::Retry(BACKEND_RETRY_AFTER),
            "chain_tx_failed",
            "The chain transaction could not be confirmed.",
        ),
        (
            MppError::ReservationMissing,
            MppResponse::Challenge(ProblemCode::VerificationFailed),
            "reservation_missing",
            "The request reservation is no longer available.",
        ),
    ];
    for (error, response, kind, detail) in cases {
        assert_eq!(error.response(), response);
        assert_eq!(error.kind(), kind);
        assert_eq!(error.detail(), detail);
        assert!(error.session_snapshot().is_none());
        let (status, code) = match response {
            MppResponse::Challenge(code) => (402, code),
            MppResponse::Problem(status, code) => (status.code(), code),
            MppResponse::Retry(duration) => {
                assert_eq!(duration, BACKEND_RETRY_AFTER);
                continue;
            }
            MppResponse::Limit(_) => panic!("unexpected limit response"),
        };
        let body: Value = serde_json::from_str(&code.body(status, error.detail())).unwrap();
        assert_eq!(body["status"], status);
        assert_eq!(body["type"], code.type_uri());
        assert_eq!(body["detail"], detail);
    }
}

#[test]
fn insufficient_balance_retains_snapshot_and_redacts_debug() {
    let snapshot = snapshot();
    let error = MppError::VoucherTooLow(Box::new(snapshot.clone()));
    assert_eq!(
        error.response(),
        MppResponse::Challenge(ProblemCode::InsufficientBalance)
    );
    assert_eq!(error.kind(), "insufficient_balance");
    assert_eq!(
        error.detail(),
        "The accepted voucher does not cover this request."
    );
    assert_eq!(error.session_snapshot(), Some(&snapshot));
    assert_eq!(format!("{snapshot:?}"), "SessionSnapshot { .. }");
    assert_eq!(
        format!("{:?}", snapshot.highest_voucher),
        "HighestVoucher { .. }"
    );
    assert_eq!(format!("{:?}", cfg()), "SessionServiceConfig { .. }");
    let text = format!("{error:?} {error} {} {}", error.kind(), error.detail());
    let v = Vectors::load();
    for private in [
        v.text("channelId"),
        v.text("voucher.signature"),
        v.text("payer"),
        "session.example",
    ] {
        assert!(!text.contains(private));
    }
}

#[test]
fn session_problem_codes_keep_public_wire_names_and_titles() {
    for (code, name, title) in [
        (
            ProblemCode::InsufficientBalance,
            "insufficient-balance",
            "Insufficient Balance",
        ),
        (
            ProblemCode::ChannelNotFound,
            "channel-not-found",
            "Channel Not Found",
        ),
        (
            ProblemCode::ChannelClosed,
            "channel-closed",
            "Channel Closed",
        ),
        (
            ProblemCode::InvalidSignature,
            "invalid-signature",
            "Invalid Signature",
        ),
        (
            ProblemCode::SignerMismatch,
            "signer-mismatch",
            "Signer Mismatch",
        ),
        (
            ProblemCode::AmountExceedsDeposit,
            "amount-exceeds-deposit",
            "Amount Exceeds Deposit",
        ),
        (
            ProblemCode::DeltaTooSmall,
            "delta-too-small",
            "Delta Too Small",
        ),
    ] {
        assert_eq!(code.as_str(), name);
        assert_eq!(code.title(), title);
        assert_eq!(
            code.type_uri(),
            format!("https://paymentauth.org/problems/{name}")
        );
    }
}

#[test]
fn all_closed_session_reasons_are_input_free_and_keep_verification_failure_mapping() {
    let mut errors = vec![];
    for reason in [
        SessionMismatch::Descriptor,
        SessionMismatch::ChannelId,
        SessionMismatch::Action,
    ] {
        errors.push(MppError::SessionMismatch(reason));
    }
    for reason in [
        TopUpReason::Malformed,
        TopUpReason::ChainId,
        TopUpReason::CallCount,
        TopUpReason::Target,
        TopUpReason::Value,
        TopUpReason::Selector,
        TopUpReason::Calldata,
        TopUpReason::Descriptor,
        TopUpReason::Amount,
        TopUpReason::Sender,
        TopUpReason::Signature,
        TopUpReason::ExpiringNonce,
    ] {
        errors.push(MppError::TopUpRejected(reason));
    }
    for reason in [
        OpenMismatch::Malformed,
        OpenMismatch::UnsupportedSignature,
        OpenMismatch::ChainId,
        OpenMismatch::CallCount,
        OpenMismatch::Target,
        OpenMismatch::Value,
        OpenMismatch::Selector,
        OpenMismatch::Calldata,
        OpenMismatch::Operator,
        OpenMismatch::Payee,
        OpenMismatch::Token,
        OpenMismatch::Deposit,
        OpenMismatch::ExpiringNonce,
        OpenMismatch::Signature,
    ] {
        errors.push(MppError::OpenRejected(reason));
    }
    let v = Vectors::load();
    for error in errors {
        assert_eq!(
            error.response(),
            MppResponse::Challenge(ProblemCode::VerificationFailed)
        );
        assert!(error
            .kind()
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte == b'_'));
        assert!(!error.kind().is_empty());
        assert!(error.session_snapshot().is_none());
        let text = format!("{error:?} {error} {} {}", error.kind(), error.detail());
        for private in [
            v.text("channelId"),
            v.text("payer"),
            v.text("voucher.signature"),
            "session.example",
        ] {
            assert!(!text.contains(private));
        }
    }
}

#[tokio::test]
async fn reopen_cannot_bypass_delta_or_revive_a_finalized_channel() {
    for finalized in [false, true] {
        let f = Fixture::new();
        f.seed().await;
        if finalized {
            f.repo
                .finalize(&Vectors::load().fixed("channelId"), 0)
                .await
                .unwrap();
        }
        let credential = f.verified(open(INITIAL + 99)).await;
        assert_eq!(
            f.service.open(&credential).await.err(),
            Some(if finalized {
                MppError::ChannelClosed
            } else {
                MppError::VoucherRejected(VoucherReason::DeltaTooSmall)
            })
        );
        assert_eq!(f.rpc.send_calls(), 0);
        assert_eq!(
            f.repo
                .get(&Vectors::load().fixed("channelId"))
                .await
                .unwrap()
                .unwrap()
                .highest_cumulative,
            INITIAL
        );
    }
}

#[tokio::test]
async fn exact_stored_replay_does_not_reverify_signature() {
    let f = Fixture::new();
    let v = Vectors::load();
    // The repository is trusted to have verified its stored bytes. Deliberately invalid
    // bytes distinguish the fast replay path from redundant cryptographic verification.
    f.repo
        .open_or_refresh(NewChannel {
            channel_id: v.fixed("channelId"),
            chain_id: v.chain_id(),
            escrow: v.escrow(),
            descriptor: v.descriptor(),
            deposit: v.int("deposit"),
            settled_on_chain: 0,
            highest_cumulative: INITIAL,
            highest_signature: [0; 65],
        })
        .await
        .unwrap();
    set_chain(&f.rpc, chain_state());
    let mut payload = voucher(INITIAL);
    payload["signature"] = json!(hex_prefixed(&[0; 65]));
    let credential = f.verified(payload).await;
    let reserved = f
        .service
        .voucher(&credential, 10, Uuid::new_v4())
        .await
        .unwrap();
    assert_eq!(reserved.reserved, 10);
    assert_eq!(f.repo.reservation_count(), 1);
}

#[tokio::test]
async fn unauthenticated_small_delta_never_discloses_stored_voucher() {
    let f = Fixture::new();
    f.seed().await;
    set_chain(&f.rpc, chain_state());
    let mut payload = voucher(INITIAL + 1);
    payload["signature"] = json!(hex_prefixed(&[0; 65]));
    let credential = f.verified(payload).await;
    let error = f
        .service
        .voucher(&credential, 10, Uuid::new_v4())
        .await
        .err()
        .unwrap();
    assert_eq!(
        error,
        MppError::VoucherRejected(VoucherReason::InvalidSignature)
    );
    assert!(error.session_snapshot().is_none());
    assert_eq!(f.repo.reservation_count(), 0);
}
