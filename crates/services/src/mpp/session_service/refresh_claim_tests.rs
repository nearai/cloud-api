use crate::common::RepositoryError;
use crate::mpp::abi::ChannelState;
use crate::mpp::channel_memory::InMemoryChannelRepository;
use crate::mpp::error::{MppError, UnavailableReason};
use crate::mpp::ports::*;
use crate::mpp::session_service::tests::{config, Fixture};
use crate::mpp::session_service::SessionService;
use crate::mpp::session_vectors::Vectors;
use crate::mpp::signer::LocalSigner;
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Notify;
use uuid::Uuid;

struct ClaimRepo {
    inner: Arc<InMemoryChannelRepository>,
    pause: bool,
    get_calls: AtomicUsize,
    entered: Notify,
    finish: Notify,
    written: Notify,
}

impl ClaimRepo {
    fn new(inner: Arc<InMemoryChannelRepository>, pause: bool) -> Arc<Self> {
        Arc::new(Self {
            inner,
            pause,
            get_calls: AtomicUsize::new(0),
            entered: Notify::new(),
            finish: Notify::new(),
            written: Notify::new(),
        })
    }
}

fn service(f: &Fixture, repo: Arc<ClaimRepo>) -> SessionService {
    SessionService::new(
        config(),
        repo,
        f.rpc.clone(),
        Arc::new(LocalSigner::new(Vectors::load().secret_key("payeeKey"))),
    )
    .unwrap()
}

#[tokio::test]
async fn cleanup_does_not_depend_on_a_fallible_post_claim_get() {
    let f = Fixture::new();
    f.seed().await;
    let repo = ClaimRepo::new(f.repo.clone(), false);
    f.rpc
        .set_call_handler(Box::new(|_, _| Err(RpcError::Unavailable)));
    let result = service(&f, repo.clone()).refresh(f.record().await).await;
    assert!(f.record().await.last_chain_read_at.is_none());
    assert_eq!(
        result.err(),
        Some(MppError::Unavailable(UnavailableReason::Rpc))
    );
    assert_eq!(repo.get_calls.load(Ordering::SeqCst), 0);
    assert_eq!(f.rpc.eth_calls(), 1);
}

#[tokio::test]
async fn cancellation_before_claim_commit_still_cleans_up_the_committed_claim() {
    let f = Fixture::new();
    f.seed().await;
    let repo = ClaimRepo::new(f.repo.clone(), true);
    let service = service(&f, repo.clone());
    let record = f.record().await;
    let attempt = tokio::spawn(async move { service.refresh(record).await });
    tokio::time::timeout(Duration::from_secs(5), repo.entered.notified())
        .await
        .unwrap();
    attempt.abort();
    assert!(attempt.await.unwrap_err().is_cancelled());
    repo.finish.notify_one();
    tokio::time::timeout(Duration::from_secs(5), repo.written.notified())
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_secs(5), async {
        while f.record().await.last_chain_read_at.is_some() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("the detached owner clears a claim committed after cancellation");
    assert_eq!(repo.get_calls.load(Ordering::SeqCst), 0);
    assert_eq!(f.rpc.eth_calls(), 0);
}

#[async_trait]
impl ChannelRepository for ClaimRepo {
    async fn open_or_refresh(&self, channel: NewChannel) -> Result<ChannelRecord, RepositoryError> {
        self.inner.open_or_refresh(channel).await
    }
    async fn get(&self, _: &[u8; 32]) -> Result<Option<ChannelRecord>, RepositoryError> {
        self.get_calls.fetch_add(1, Ordering::SeqCst);
        Err(RepositoryError::QueryTimeout)
    }
    async fn accept_voucher(
        &self,
        id: &[u8; 32],
        amount: u128,
        signature: [u8; 65],
        rules: VoucherRules,
    ) -> Result<VoucherOutcome, RepositoryError> {
        self.inner
            .accept_voucher(id, amount, signature, rules)
            .await
    }
    async fn reserve(
        &self,
        id: &[u8; 32],
        request: Uuid,
        amount: u128,
    ) -> Result<ReserveOutcome, RepositoryError> {
        self.inner.reserve(id, request, amount).await
    }
    async fn charge(
        &self,
        request: Uuid,
        actual: u128,
        units: u64,
    ) -> Result<ChargeOutcome, RepositoryError> {
        self.inner.charge(request, actual, units).await
    }
    async fn release(&self, request: Uuid) -> Result<bool, RepositoryError> {
        self.inner.release(request).await
    }
    async fn record_chain_state(
        &self,
        id: &[u8; 32],
        state: ChannelState,
        at: DateTime<Utc>,
    ) -> Result<Option<ChannelRecord>, RepositoryError> {
        self.inner.record_chain_state(id, state, at).await
    }
    async fn mark_closing(
        &self,
        id: &[u8; 32],
        pending: PendingClose,
        now: DateTime<Utc>,
    ) -> Result<CloseTransition, RepositoryError> {
        self.inner.mark_closing(id, pending, now).await
    }
    async fn finalize(&self, id: &[u8; 32], settled: u128) -> Result<(), RepositoryError> {
        self.inner.finalize(id, settled).await
    }
    async fn clear_closing(&self, id: &[u8; 32], hash: &[u8; 32]) -> Result<bool, RepositoryError> {
        self.inner.clear_closing(id, hash).await
    }
    async fn finalize_pending_close(
        &self,
        id: &[u8; 32],
        hash: &[u8; 32],
        settled: u128,
    ) -> Result<bool, RepositoryError> {
        self.inner.finalize_pending_close(id, hash, settled).await
    }
    async fn claim_chain_read_lease(
        &self,
        id: &[u8; 32],
        now: DateTime<Utc>,
        interval: chrono::Duration,
    ) -> Result<Option<ChainReadClaim>, RepositoryError> {
        if self.pause {
            self.entered.notify_one();
            self.finish.notified().await;
        }
        let result = self.inner.claim_chain_read_lease(id, now, interval).await;
        self.written.notify_one();
        result
    }
    async fn clear_chain_read_claim(
        &self,
        id: &[u8; 32],
        at: DateTime<Utc>,
    ) -> Result<bool, RepositoryError> {
        self.inner.clear_chain_read_claim(id, at).await
    }
    async fn claim_settlement_lease(
        &self,
        id: &[u8; 32],
        now: DateTime<Utc>,
        until: DateTime<Utc>,
    ) -> Result<Option<(ChannelRecord, SettlementLease)>, RepositoryError> {
        self.inner.claim_settlement_lease(id, now, until).await
    }
    async fn record_settlement(
        &self,
        id: &[u8; 32],
        lease: SettlementLease,
        cumulative: u128,
        at: DateTime<Utc>,
    ) -> Result<bool, RepositoryError> {
        self.inner
            .record_settlement(id, lease, cumulative, at)
            .await
    }
    async fn release_settlement_lease(
        &self,
        id: &[u8; 32],
        lease: SettlementLease,
    ) -> Result<bool, RepositoryError> {
        self.inner.release_settlement_lease(id, lease).await
    }
    async fn list_active(
        &self,
        after: Option<[u8; 32]>,
        limit: usize,
    ) -> Result<Vec<ChannelRecord>, RepositoryError> {
        self.inner.list_active(after, limit).await
    }
    async fn release_stale_reservations(
        &self,
        cutoff: DateTime<Utc>,
    ) -> Result<u64, RepositoryError> {
        self.inner.release_stale_reservations(cutoff).await
    }
    async fn purge_finalized(&self, cutoff: DateTime<Utc>) -> Result<u64, RepositoryError> {
        self.inner.purge_finalized(cutoff).await
    }
}
