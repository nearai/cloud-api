// INTEGRATION: child of mpp_session_tests.rs, registered as `finalization`.
//! Cancellation after the ledger write starts must not cancel that write.

use super::*;
use chrono::{DateTime, Utc};
use services::common::RepositoryError;
use services::mpp::{
    ChannelRecord, ChargeOutcome, CloseTransition, PendingClose, ReserveOutcome, VoucherOutcome,
};
use tokio::sync::Notify;

#[derive(Default)]
struct Gate {
    entered: Notify,
    resume: Notify,
    calls: AtomicUsize,
}

impl Gate {
    async fn pause(&self) {
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.entered.notify_one();
        self.resume.notified().await;
    }
}

struct DelayedLedger {
    inner: Arc<InMemoryChannelRepository>,
    charge: Arc<Gate>,
    release: Arc<Gate>,
}

#[async_trait]
impl ChannelRepository for DelayedLedger {
    async fn open_or_refresh(&self, channel: NewChannel) -> Result<ChannelRecord, RepositoryError> {
        self.inner.open_or_refresh(channel).await
    }

    async fn get(&self, channel_id: &[u8; 32]) -> Result<Option<ChannelRecord>, RepositoryError> {
        self.inner.get(channel_id).await
    }

    async fn accept_voucher(
        &self,
        channel_id: &[u8; 32],
        cumulative: u128,
        signature: [u8; 65],
    ) -> Result<VoucherOutcome, RepositoryError> {
        self.inner
            .accept_voucher(channel_id, cumulative, signature)
            .await
    }

    async fn reserve(
        &self,
        channel_id: &[u8; 32],
        request_id: Uuid,
        amount: u128,
    ) -> Result<ReserveOutcome, RepositoryError> {
        self.inner.reserve(channel_id, request_id, amount).await
    }

    async fn charge(
        &self,
        request_id: Uuid,
        actual: u128,
        units: u64,
    ) -> Result<ChargeOutcome, RepositoryError> {
        self.charge.pause().await;
        self.inner.charge(request_id, actual, units).await
    }

    async fn release(&self, request_id: Uuid) -> Result<bool, RepositoryError> {
        self.release.pause().await;
        self.inner.release(request_id).await
    }

    async fn record_chain_state(
        &self,
        channel_id: &[u8; 32],
        state: ChannelState,
        read_at: DateTime<Utc>,
    ) -> Result<Option<ChannelRecord>, RepositoryError> {
        self.inner
            .record_chain_state(channel_id, state, read_at)
            .await
    }

    async fn mark_closing(
        &self,
        channel_id: &[u8; 32],
        pending: PendingClose,
    ) -> Result<CloseTransition, RepositoryError> {
        self.inner.mark_closing(channel_id, pending).await
    }

    async fn finalize(
        &self,
        channel_id: &[u8; 32],
        settled_on_chain: u128,
    ) -> Result<(), RepositoryError> {
        self.inner.finalize(channel_id, settled_on_chain).await
    }

    async fn clear_closing(&self, channel_id: &[u8; 32]) -> Result<(), RepositoryError> {
        self.inner.clear_closing(channel_id).await
    }

    async fn claim_settlement_lease(
        &self,
        channel_id: &[u8; 32],
        now: DateTime<Utc>,
        until: DateTime<Utc>,
    ) -> Result<Option<ChannelRecord>, RepositoryError> {
        self.inner
            .claim_settlement_lease(channel_id, now, until)
            .await
    }

    async fn record_settlement(
        &self,
        channel_id: &[u8; 32],
        cumulative: u128,
        at: DateTime<Utc>,
    ) -> Result<(), RepositoryError> {
        self.inner
            .record_settlement(channel_id, cumulative, at)
            .await
    }

    async fn release_settlement_lease(&self, channel_id: &[u8; 32]) -> Result<(), RepositoryError> {
        self.inner.release_settlement_lease(channel_id).await
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

fn app_with_delayed_ledger() -> (Arc<SessionApp>, Arc<Gate>, Arc<Gate>) {
    let ledger = Arc::new(InMemoryChannelRepository::new());
    let charge = Arc::new(Gate::default());
    let release = Arc::new(Gate::default());
    let repository = Arc::new(DelayedLedger {
        inner: ledger.clone(),
        charge: charge.clone(),
        release: release.clone(),
    });
    let app =
        session_app_with_repository(SERVICE_KEY, AMOUNT, Some(SESSION_PRICE), ledger, repository);
    (Arc::new(app), charge, release)
}

async fn await_finalized(app: &SessionApp) {
    tokio::time::timeout(Duration::from_secs(2), async {
        while app.ledger.reservation_count() != 0 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn cancellation_during_charge_does_not_cancel_or_double_finalize() {
    let (app, charge, release) = app_with_delayed_ledger();
    seed(&app).await;
    let credential = session_credential(&app, voucher(INITIAL)).await;
    let running = {
        let app = app.clone();
        tokio::spawn(async move { post_session(&app, Some(&credential)).await })
    };
    tokio::time::timeout(Duration::from_secs(2), charge.entered.notified())
        .await
        .unwrap();
    assert_eq!(app.ledger.reservation_count(), 1);
    running.abort();
    let _ = running.await;
    charge.resume.notify_one();
    await_finalized(&app).await;
    assert_eq!((row(&app).await.spent, row(&app).await.units), (24, 12));
    assert_eq!(charge.calls.load(Ordering::SeqCst), 1);
    assert_eq!(release.calls.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn cancellation_during_release_does_not_leave_a_reservation() {
    let (app, charge, release) = app_with_delayed_ledger();
    seed(&app).await;
    *app.state.mode.lock().unwrap() = SessionHandlerMode::Fail;
    let credential = session_credential(&app, voucher(INITIAL)).await;
    let running = {
        let app = app.clone();
        tokio::spawn(async move { post_session(&app, Some(&credential)).await })
    };
    tokio::time::timeout(Duration::from_secs(2), release.entered.notified())
        .await
        .unwrap();
    assert_eq!(app.ledger.reservation_count(), 1);
    running.abort();
    let _ = running.await;
    release.resume.notify_one();
    await_finalized(&app).await;
    assert_eq!((row(&app).await.spent, row(&app).await.units), (0, 0));
    assert_eq!(charge.calls.load(Ordering::SeqCst), 0);
    assert_eq!(release.calls.load(Ordering::SeqCst), 1);
}
