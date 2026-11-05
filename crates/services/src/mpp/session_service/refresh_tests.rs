use super::tests::{config, service, state, voucher, Fixture};
use crate::mpp::error::{MppError, UnavailableReason};
use crate::mpp::ports::{ChannelRepository, RpcError, RpcReceipt, TempoRpc};
use crate::mpp::session_vectors::Vectors;
use crate::mpp::testing::FakeTempo;
use async_trait::async_trait;
use chrono::Utc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Notify;
use uuid::Uuid;

#[path = "refresh_claim_tests.rs"]
mod claim_tests;

#[tokio::test]
async fn pending_refresh_uses_two_minute_observation_but_rejects_ten_minutes() {
    for (minutes, allowed) in [(2, true), (10, false)] {
        let f = Fixture::new();
        f.seed().await;
        let id = Vectors::load().fixed("channelId");
        let now = Utc::now();
        f.repo
            .record_chain_state(&id, state(), now - chrono::Duration::minutes(minutes))
            .await
            .unwrap();
        assert!(f
            .repo
            .claim_chain_read(&id, now, chrono::Duration::seconds(60))
            .await
            .unwrap());
        let credential = f.credential(voucher(1_250_000)).await;
        let result = f.service.voucher(&credential, 10, Uuid::new_v4()).await;
        if allowed {
            assert!(
                result.is_ok(),
                "a recent observation remains usable while pending"
            );
        } else {
            assert_eq!(
                result.err(),
                Some(MppError::Unavailable(UnavailableReason::Rpc))
            );
            assert_eq!(f.repo.reservation_count(), 0);
        }
        assert_eq!(f.rpc.eth_calls(), 0);
    }
}

#[tokio::test]
async fn failed_refresh_does_not_fall_back_to_two_minute_observation() {
    let f = Fixture::new();
    f.seed().await;
    let id = Vectors::load().fixed("channelId");
    f.repo
        .record_chain_state(&id, state(), Utc::now() - chrono::Duration::minutes(2))
        .await
        .unwrap();
    f.rpc
        .set_call_handler(Box::new(|_, _| Err(RpcError::Unavailable)));
    let credential = f.credential(voucher(1_250_000)).await;
    assert_eq!(
        f.service
            .voucher(&credential, 10, Uuid::new_v4())
            .await
            .err(),
        Some(MppError::Unavailable(UnavailableReason::Rpc))
    );
    assert_eq!(f.rpc.eth_calls(), 1);
    assert_eq!(f.repo.reservation_count(), 0);
}

struct FirstReadHangs {
    inner: Arc<FakeTempo>,
    reads: AtomicUsize,
    entered: Notify,
}

#[async_trait]
impl TempoRpc for FirstReadHangs {
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
    async fn call(&self, to: [u8; 20], input: &[u8]) -> Result<Vec<u8>, RpcError> {
        if self.reads.fetch_add(1, Ordering::SeqCst) == 0 {
            self.entered.notify_one();
            std::future::pending::<()>().await;
        }
        self.inner.call(to, input).await
    }
    async fn gas_price(&self) -> Result<u128, RpcError> {
        self.inner.gas_price().await
    }
}

#[tokio::test]
async fn cancelled_voucher_refresh_releases_claim_after_bounded_hanging_read() {
    let f = Fixture::new();
    f.seed().await;
    let rpc = Arc::new(FirstReadHangs {
        inner: f.rpc.clone(),
        reads: AtomicUsize::new(0),
        entered: Notify::new(),
    });
    let service = Arc::new(service(config(), f.repo.clone(), rpc.clone()));
    let credential = f.credential(voucher(1_250_000)).await;
    let attempt = {
        let service = service.clone();
        let credential = credential.clone();
        tokio::spawn(async move { service.voucher(&credential, 10, Uuid::new_v4()).await })
    };
    rpc.entered.notified().await;
    attempt.abort();
    assert!(attempt.await.unwrap_err().is_cancelled());
    tokio::time::timeout(Duration::from_secs(5), async {
        while f.record().await.last_chain_read_at.is_some() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("cancelling the caller clears its pending claim without waiting for lease expiry");
    let result = service.voucher(&credential, 10, Uuid::new_v4()).await;
    assert!(
        result.is_ok(),
        "a cancelled request must not retain the refresh claim"
    );
    assert_eq!(rpc.reads.load(Ordering::SeqCst), 2);
}
