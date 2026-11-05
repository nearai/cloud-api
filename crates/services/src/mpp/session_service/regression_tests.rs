// INTEGRATION: register directly as session_service::regression_tests.

use std::sync::Arc;

use async_trait::async_trait;
use chrono::Utc;
use serde_json::{json, Value};
use tokio::sync::Notify;
use uuid::Uuid;

use super::SessionService;
use crate::mpp::error::{
    CloseReason, MppError, MppResponse, UnavailableReason, BACKEND_RETRY_AFTER,
};
use crate::mpp::ports::{
    ChannelLifecycle, ChannelRepository, ChargeOutcome, PendingClose, ReserveOutcome, RpcError,
    RpcReceipt, TempoRpc,
};
use crate::mpp::pricing::Price;
use crate::mpp::protocol::{self, ParsedAuthorization, ProblemCode};
use crate::mpp::session_service::tests::{close, config, service, voucher, Fixture};
use crate::mpp::session_vectors::Vectors;
use crate::mpp::testing::{credential_header, FakeTempo, SendMode};

const INITIAL: u128 = 1_250_000;
const ROUTE: &str = "/v1/chat/completions";

async fn deliver(f: &Fixture, amount: u128, units: u64) {
    let id = Uuid::new_v4();
    assert!(matches!(
        f.repo
            .reserve(&Vectors::load().fixed("channelId"), id, amount)
            .await
            .unwrap(),
        ReserveOutcome::Reserved(_)
    ));
    assert!(matches!(
        f.repo.charge(id, amount, units).await.unwrap(),
        ChargeOutcome::Charged { charged, .. } if charged == amount
    ));
}

fn pending_receipt(pending: PendingClose, success: bool) -> RpcReceipt {
    RpcReceipt {
        tx_hash: pending.tx_hash,
        block_number: 1,
        success,
        from: Vectors::load().fixed("payee"),
        logs: vec![],
    }
}

fn assert_retry(error: &MppError) {
    assert_eq!(error.response(), MppResponse::Retry(BACKEND_RETRY_AFTER));
}

#[tokio::test]
async fn lost_close_reply_keeps_pending_fence_and_retry_reconciles_without_resend() {
    let f = Fixture::new();
    f.seed().await;
    deliver(&f, 36, 18).await;
    f.rpc.set_send_mode(SendMode::Unavailable);
    let credential = f.credential(close(INITIAL)).await;

    let error = f.service.close(&credential).await.err().unwrap();
    assert_retry(&error);
    let row = f.record().await;
    assert_eq!(row.state, ChannelLifecycle::Closing);
    let pending = row.pending_close.unwrap();
    assert_eq!((pending.cumulative, pending.capture), (INITIAL, 36));
    assert_eq!(f.rpc.send_calls(), 1);

    let spending = f.credential(voucher(INITIAL)).await;
    assert!(matches!(
        f.service.voucher(&spending, 10, Uuid::new_v4()).await,
        Err(MppError::ChannelClosing)
    ));
    assert_eq!(f.repo.reservation_count(), 0);
    assert_eq!((f.record().await.spent, f.record().await.units), (36, 18));

    f.rpc.mine(pending_receipt(pending, true));
    let result = f.service.close(&credential).await.unwrap().receipt;
    assert_eq!(result.tx_hash, Some(pending.tx_hash));
    assert_eq!((result.spent, result.units), (36, 18));
    assert_eq!(result.accepted_cumulative, INITIAL);
    assert_eq!(f.rpc.send_calls(), 1);
    let row = f.record().await;
    assert_eq!(row.state, ChannelLifecycle::Finalized);
    assert_eq!(row.settled_on_chain, 36);
    assert!(row.pending_close.is_none());
}

#[tokio::test]
async fn pending_close_stays_fenced_before_expiry_margin_and_reopens_only_after_chain_read() {
    for expired in [false, true] {
        let f = Fixture::new();
        f.seed().await;
        deliver(&f, 36, 18).await;
        let now = u64::try_from(Utc::now().timestamp()).unwrap();
        let pending = PendingClose {
            tx_hash: [0x71; 32],
            cumulative: INITIAL,
            capture: 36,
            // Both deadlines have elapsed. The recent one remains inside the
            // thirty-second reconciliation margin and must still fence reservations.
            valid_before: if expired { now - 31 } else { now - 5 },
        };
        f.repo
            .mark_closing(&Vectors::load().fixed("channelId"), pending, Utc::now())
            .await
            .unwrap();
        f.rpc.set_gas_price(Err(RpcError::Unavailable));
        let credential = f.credential(close(INITIAL)).await;
        let error = f.service.close(&credential).await.err().unwrap();
        assert_retry(&error);
        let row = f.record().await;
        if expired {
            assert_eq!(row.state, ChannelLifecycle::Open);
            assert!(row.pending_close.is_none());
            assert_eq!(f.rpc.eth_calls(), 1);
            assert!(row.last_chain_read_at.is_some());
        } else {
            assert_eq!(row.state, ChannelLifecycle::Closing);
            assert_eq!(row.pending_close, Some(pending));
            assert_eq!(f.rpc.eth_calls(), 0);
        }
        assert_eq!((row.spent, row.reserved, row.units), (36, 0, 18));
        assert_eq!(f.rpc.send_calls(), 0);
    }
}

#[tokio::test]
async fn definitive_failed_pending_close_receipt_clears_fence_without_spending() {
    let f = Fixture::new();
    f.seed().await;
    deliver(&f, 36, 18).await;
    let pending = PendingClose {
        tx_hash: [0x72; 32],
        cumulative: INITIAL,
        capture: 36,
        valid_before: u64::try_from(Utc::now().timestamp()).unwrap() + 3600,
    };
    f.repo
        .mark_closing(&Vectors::load().fixed("channelId"), pending, Utc::now())
        .await
        .unwrap();
    f.rpc.mine(pending_receipt(pending, false));
    f.rpc.set_gas_price(Err(RpcError::Unavailable));
    let credential = f.credential(close(INITIAL)).await;
    assert_retry(&f.service.close(&credential).await.err().unwrap());
    let row = f.record().await;
    assert_eq!(row.state, ChannelLifecycle::Open);
    assert!(row.pending_close.is_none());
    assert_eq!((row.spent, row.reserved, row.units), (36, 0, 18));
    assert_eq!(f.rpc.send_calls(), 0);
}

struct GasBarrierRpc {
    inner: Arc<FakeTempo>,
    reached: Notify,
    resume: Notify,
}

#[async_trait]
impl TempoRpc for GasBarrierRpc {
    async fn send_raw_transaction_sync(&self, raw: &[u8]) -> Result<RpcReceipt, RpcError> {
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
        self.reached.notify_one();
        self.resume.notified().await;
        self.inner.gas_price().await
    }
}

#[tokio::test]
async fn close_rechecks_capture_and_reservations_after_atomic_mark_before_broadcast() {
    for charge_during_build in [false, true] {
        let f = Fixture::new();
        f.seed().await;
        let rpc = Arc::new(GasBarrierRpc {
            inner: f.rpc.clone(),
            reached: Notify::new(),
            resume: Notify::new(),
        });
        let service = Arc::new(service(config(), f.repo.clone(), rpc.clone()));
        let credential =
            crate::mpp::session_service::tests::verified(&service, close(INITIAL)).await;
        let closing = {
            let service = service.clone();
            tokio::spawn(async move { service.close(&credential).await })
        };
        tokio::time::timeout(std::time::Duration::from_secs(5), rpc.reached.notified())
            .await
            .unwrap();
        let request = Uuid::new_v4();
        assert!(matches!(
            f.repo
                .reserve(&Vectors::load().fixed("channelId"), request, 100)
                .await
                .unwrap(),
            ReserveOutcome::Reserved(_)
        ));
        if charge_during_build {
            assert!(matches!(
                f.repo.charge(request, 36, 18).await.unwrap(),
                ChargeOutcome::Charged { charged: 36, .. }
            ));
        }
        rpc.resume.notify_one();
        let result = tokio::time::timeout(std::time::Duration::from_secs(5), closing)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            result.err(),
            Some(MppError::CloseRejected(CloseReason::StateChanged))
        );
        assert_eq!(f.rpc.send_calls(), 0);
        let row = f.record().await;
        assert_eq!(row.state, ChannelLifecycle::Open);
        assert!(row.pending_close.is_none());
        if charge_during_build {
            assert_eq!((row.spent, row.reserved, row.units), (36, 0, 18));
            assert_eq!(f.repo.reservation_count(), 0);
        } else {
            assert_eq!((row.spent, row.reserved, row.units), (100, 100, 0));
            assert_eq!(f.repo.reservation_count(), 1);
        }
    }
}

#[tokio::test]
async fn stale_chain_failures_refuse_even_exact_replay_without_money_changes() {
    for case in 0..3 {
        let f = Fixture::new();
        f.seed().await;
        let before = f.record().await;
        assert!(before.last_chain_read_at.is_none());
        match case {
            0 => f
                .rpc
                .set_call_handler(Box::new(|_, _| Err(RpcError::Unavailable))),
            1 => f.rpc.set_call_handler(Box::new(|_, _| Ok(vec![0x42; 95]))),
            _ => f.rpc.set_call_handler(Box::new(|_, _| {
                Ok([
                    crate::mpp::abi::uint_word(5_000_001),
                    crate::mpp::abi::uint_word(5_000_000),
                    crate::mpp::abi::uint_word(0),
                ]
                .concat())
            })),
        }
        let credential = f.credential(voucher(INITIAL)).await;
        assert_eq!(
            f.service
                .voucher(&credential, 100, Uuid::new_v4())
                .await
                .err(),
            Some(MppError::Unavailable(UnavailableReason::Rpc)),
            "case {case}"
        );
        let mut after = f.record().await;
        assert!(after.last_chain_read_at.is_some());
        assert!(after.updated_at > before.updated_at);
        // A failed read retains its throttle claim; every money field stays exact.
        after.last_chain_read_at = before.last_chain_read_at;
        after.updated_at = before.updated_at;
        assert_eq!(after, before);
        assert_eq!(f.repo.reservation_count(), 0);
        assert_eq!(f.rpc.eth_calls(), 1);
        assert_eq!(f.rpc.send_calls(), 0);
    }
}

async fn verified_at_price(
    service: &SessionService,
    payload: Value,
    price: Price,
) -> super::SessionCredential {
    let challenge = service.challenge(ROUTE, &price, None).await.unwrap();
    let header = credential_header(&challenge, payload, None);
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
        panic!("session envelope parsed as charge");
    };
    service
        .verify_credential(&credential, ROUTE, &price)
        .await
        .unwrap()
}

#[tokio::test]
async fn actual_metering_uses_each_verified_model_price_through_reserve_and_charge() {
    for (token_price, expected_cost) in [(2, 36), (7, 126)] {
        let f = Fixture::new();
        f.seed().await;
        let mut config = config();
        let mut expensive = *config.model_prices.get("model").unwrap();
        expensive.token_price = Some(7);
        config.model_prices.insert("expensive".into(), expensive);
        let service = service(config, f.repo.clone(), f.rpc.clone());
        let credential = verified_at_price(
            &service,
            voucher(INITIAL),
            Price {
                amount: 0,
                token_price: Some(token_price),
            },
        )
        .await;
        let reserved = service
            .voucher(&credential, 1000, Uuid::new_v4())
            .await
            .unwrap();
        assert_eq!(
            (f.record().await.spent, f.record().await.reserved),
            (1000, 1000)
        );
        let receipt = service.charge(&reserved, 7, 11).await.unwrap();
        assert_eq!((receipt.spent, receipt.units), (expected_cost, 18));
        let row = f.record().await;
        assert_eq!((row.spent, row.reserved, row.units), (expected_cost, 0, 18));
        assert_eq!(f.repo.reservation_count(), 0);
    }
}

#[tokio::test]
async fn exact_shortfall_then_small_delta_snapshot_guides_next_usable_voucher() {
    let f = Fixture::new();
    f.seed().await;
    deliver(&f, INITIAL - 10, 50).await;
    let replay = f.credential(voucher(INITIAL)).await;
    let error = f
        .service
        .voucher(&replay, 20, Uuid::new_v4())
        .await
        .err()
        .unwrap();
    let MppError::VoucherTooLow(snapshot) = error else {
        panic!("expected exact shortfall, got {error:?}")
    };
    assert_eq!(snapshot.required_cumulative, INITIAL + 10);
    assert_eq!(snapshot.accepted_cumulative, INITIAL);
    assert_eq!(snapshot.spent, INITIAL - 10);
    assert_eq!(snapshot.highest_voucher.cumulative_amount, INITIAL);
    assert_eq!(f.repo.reservation_count(), 0);
    assert_eq!(
        (f.record().await.spent, f.record().await.reserved),
        (INITIAL - 10, 0)
    );

    let small = f.credential(voucher(snapshot.required_cumulative)).await;
    let error = f
        .service
        .voucher(&small, 20, Uuid::new_v4())
        .await
        .err()
        .unwrap();
    assert_eq!(
        error.response(),
        MppResponse::Challenge(ProblemCode::DeltaTooSmall)
    );
    assert_eq!(
        error.session_snapshot().unwrap().required_cumulative,
        INITIAL + 100
    );
    let MppError::VoucherDeltaTooSmall(snapshot) = error else {
        panic!("expected delta snapshot, got {error:?}")
    };
    assert_eq!(snapshot.accepted_cumulative, INITIAL);
    assert_eq!(snapshot.highest_voucher.cumulative_amount, INITIAL);
    assert_eq!(snapshot.spent, INITIAL - 10);
    assert_eq!(snapshot.required_cumulative, INITIAL + 100);
    assert_eq!(f.repo.reservation_count(), 0);
    let row = f.record().await;
    assert_eq!(
        (row.highest_cumulative, row.spent, row.reserved, row.units),
        (INITIAL, INITIAL - 10, 0, 50)
    );

    let suggested = f.credential(voucher(snapshot.required_cumulative)).await;
    let reserved = f
        .service
        .voucher(&suggested, 20, Uuid::new_v4())
        .await
        .unwrap();
    assert_eq!(reserved.reserved, 20);
    let row = f.record().await;
    assert_eq!(
        (row.highest_cumulative, row.spent, row.reserved),
        (INITIAL + 100, INITIAL + 10, 20)
    );
    let receipt = f.service.charge(&reserved, 3, 4).await.unwrap();
    assert_eq!(
        (receipt.accepted_cumulative, receipt.spent, receipt.units),
        (INITIAL + 100, INITIAL + 4, 57)
    );
    let row = f.record().await;
    assert_eq!(
        (row.highest_cumulative, row.spent, row.reserved, row.units),
        (INITIAL + 100, INITIAL + 4, 0, 57)
    );
    assert_eq!(f.repo.reservation_count(), 0);
}

#[tokio::test]
async fn delta_snapshot_uses_request_requirement_when_larger_than_delta_floor() {
    let f = Fixture::new();
    f.seed().await;
    deliver(&f, INITIAL - 10, 50).await;
    let credential = f.credential(voucher(INITIAL + 10)).await;
    let error = f
        .service
        .voucher(&credential, 200, Uuid::new_v4())
        .await
        .err()
        .unwrap();
    let MppError::VoucherDeltaTooSmall(snapshot) = error else {
        panic!("expected delta snapshot, got {error:?}")
    };
    assert_eq!(snapshot.required_cumulative, INITIAL + 190);
    assert_eq!(
        (snapshot.spent, snapshot.accepted_cumulative),
        (INITIAL - 10, INITIAL)
    );
    assert_eq!(f.repo.reservation_count(), 0);
    let row = f.record().await;
    assert_eq!(
        (row.highest_cumulative, row.spent, row.reserved),
        (INITIAL, INITIAL - 10, 0)
    );
}
