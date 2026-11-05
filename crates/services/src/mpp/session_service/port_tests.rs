use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use async_trait::async_trait;
use chrono::Utc;
use tokio::sync::Notify;
use uuid::Uuid;

use super::tests::{close, config, service, voucher, Fixture};
use crate::mpp::error::{CloseReason, MppError, VoucherReason};
use crate::mpp::ports::{
    ChannelLifecycle, ChannelRepository, PendingClose, RpcError, RpcReceipt, TempoRpc, VoucherRules,
};
use crate::mpp::session_vectors::Vectors;
use crate::mpp::testing::FakeTempo;

const INITIAL: u128 = 1_250_000;

enum Pause {
    Receipt,
    Read,
}

struct BarrierRpc {
    inner: Arc<FakeTempo>,
    pause: Pause,
    reached: Notify,
    resume: Notify,
    reads: AtomicUsize,
}

impl BarrierRpc {
    fn shared(inner: Arc<FakeTempo>, pause: Pause) -> Arc<Self> {
        Arc::new(Self {
            inner,
            pause,
            reached: Notify::new(),
            resume: Notify::new(),
            reads: AtomicUsize::new(0),
        })
    }
    async fn wait(&self) {
        tokio::time::timeout(std::time::Duration::from_secs(5), self.reached.notified())
            .await
            .unwrap();
    }
}

#[async_trait]
impl TempoRpc for BarrierRpc {
    async fn send_raw_transaction_sync(&self, raw: &[u8]) -> Result<RpcReceipt, RpcError> {
        self.inner.send_raw_transaction_sync(raw).await
    }
    async fn get_transaction_receipt(
        &self,
        hash: [u8; 32],
    ) -> Result<Option<RpcReceipt>, RpcError> {
        if matches!(self.pause, Pause::Receipt) {
            self.reached.notify_one();
            self.resume.notified().await;
        }
        self.inner.get_transaction_receipt(hash).await
    }
    async fn get_block_timestamp(&self, number: u64) -> Result<Option<u64>, RpcError> {
        self.inner.get_block_timestamp(number).await
    }
    async fn call(&self, to: [u8; 20], data: &[u8]) -> Result<Vec<u8>, RpcError> {
        self.reads.fetch_add(1, Ordering::SeqCst);
        if matches!(self.pause, Pause::Read) {
            self.reached.notify_one();
            self.resume.notified().await;
        }
        self.inner.call(to, data).await
    }
    async fn gas_price(&self) -> Result<u128, RpcError> {
        self.inner.gas_price().await
    }
}

#[tokio::test]
async fn stale_close_receipt_cannot_finalize_a_replacement_pending_close() {
    let f = Fixture::new();
    f.seed().await;
    let id = Vectors::load().fixed("channelId");
    let a = PendingClose {
        tx_hash: [0xa1; 32],
        cumulative: INITIAL,
        capture: 0,
        valid_before: u64::try_from(Utc::now().timestamp()).unwrap() + 60,
    };
    f.repo.mark_closing(&id, a, Utc::now()).await.unwrap();
    f.rpc.mine(RpcReceipt {
        tx_hash: a.tx_hash,
        block_number: 1,
        success: true,
        from: Vectors::load().fixed("payee"),
        logs: vec![],
    });
    let rpc = BarrierRpc::shared(f.rpc.clone(), Pause::Receipt);
    let other = service(config(), f.repo.clone(), rpc.clone());
    let credential = f.credential(close(INITIAL)).await;
    let attempt = tokio::spawn(async move { other.close(&credential).await });
    rpc.wait().await;
    assert!(f.repo.clear_closing(&id, &a.tx_hash).await.unwrap());
    let b = PendingClose {
        tx_hash: [0xb2; 32],
        ..a
    };
    f.repo.mark_closing(&id, b, Utc::now()).await.unwrap();
    let before = f.record().await;
    rpc.resume.notify_one();
    assert_eq!(attempt.await.unwrap().err(), Some(MppError::ChannelClosing));
    assert_eq!(f.record().await, before);
    assert_eq!(f.record().await.pending_close, Some(b));
    assert_eq!(f.rpc.send_calls(), 0);
    assert_eq!(
        (
            before.spent,
            before.reserved,
            before.settled_on_chain,
            before.units
        ),
        (0, 0, 0, 0)
    );
}

#[tokio::test]
async fn separate_service_instances_share_one_chain_refresh_claim() {
    let f = Fixture::new();
    f.seed().await;
    let rpc = BarrierRpc::shared(f.rpc.clone(), Pause::Read);
    let a = service(config(), f.repo.clone(), rpc.clone());
    let b = service(config(), f.repo.clone(), rpc.clone());
    let credential = f.credential(voucher(INITIAL)).await;
    let first_credential = credential.clone();
    let attempt =
        tokio::spawn(async move { a.voucher(&first_credential, 10, Uuid::new_v4()).await });
    rpc.wait().await;
    let second = tokio::time::timeout(
        std::time::Duration::from_secs(5),
        b.voucher(&credential, 10, Uuid::new_v4()),
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(rpc.reads.load(Ordering::SeqCst), 1);
    rpc.resume.notify_one();
    let first = attempt.await.unwrap().unwrap();
    assert_eq!(f.rpc.eth_calls(), 1);
    assert_eq!(rpc.reads.load(Ordering::SeqCst), 1);
    let row = f.record().await;
    assert_eq!(
        (row.spent, row.reserved, row.units, row.settled_on_chain),
        (20, 20, 0, 0)
    );
    f.service.release(&first).await.unwrap();
    f.service.release(&second).await.unwrap();
    let row = f.record().await;
    assert_eq!((row.spent, row.reserved, row.units), (0, 0, 0));
}

#[tokio::test]
async fn service_close_returns_busy_while_settlement_owns_the_channel() {
    let f = Fixture::new();
    f.seed().await;
    let id = Vectors::load().fixed("channelId");
    let now = Utc::now();
    let (_, lease) = f
        .repo
        .claim_settlement_lease(&id, now, now + chrono::Duration::seconds(60))
        .await
        .unwrap()
        .unwrap();
    let credential = f.credential(close(INITIAL)).await;
    assert_eq!(
        f.service.close(&credential).await.err(),
        Some(MppError::CloseRejected(CloseReason::Busy))
    );
    assert_eq!(f.rpc.send_calls(), 0);
    assert_eq!(f.repo.lease_until(&id), Some(lease.until()));
    let row = f.record().await;
    assert_eq!(row.state, ChannelLifecycle::Open);
    assert!(row.pending_close.is_none());
    assert_eq!(
        (row.spent, row.reserved, row.units, row.settled_on_chain),
        (0, 0, 0, 0)
    );
}

fn signature(amount: u128) -> [u8; 65] {
    let v = Vectors::load();
    let digest = crate::mpp::eip712::voucher_digest(
        &v.escrow(),
        v.chain_id(),
        &v.fixed("channelId"),
        amount,
    )
    .unwrap();
    crate::mpp::secp256k1::sign_digest(&v.secret_key("payerKey"), &digest).unwrap()
}

#[tokio::test]
async fn service_maps_atomic_delta_refusal_after_another_instance_advances() {
    let f = Fixture::new();
    f.seed().await;
    let stale = f.record().await;
    let id = stale.channel_id;
    f.repo
        .accept_voucher(
            &id,
            INITIAL + 150,
            signature(INITIAL + 150),
            VoucherRules {
                min_delta: 100,
                settled_floor_from_chain: true,
            },
        )
        .await
        .unwrap();
    let before = f.record().await;
    assert_eq!(
        f.service
            .accept(stale, INITIAL + 200, signature(INITIAL + 200), 100)
            .await
            .err(),
        Some(MppError::VoucherRejected(VoucherReason::DeltaTooSmall))
    );
    assert_eq!(f.record().await, before);
    assert_eq!(
        (before.highest_cumulative, before.spent, before.reserved),
        (INITIAL + 150, 0, 0)
    );
}
