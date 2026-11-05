// INTEGRATION: child of mpp_session_tests.rs, registered as `finalization`.
//! Cancellation after the ledger write starts must not cancel that write.

use super::*;
use chrono::{DateTime, Utc};
use services::common::RepositoryError;
use services::mpp::{
    ChannelRecord, ChargeOutcome, CloseTransition, PendingClose, ReserveOutcome, SettlementLease,
    VoucherOutcome, VoucherRules,
};
use tokio::sync::Notify;

#[derive(Default)]
struct Gate {
    entered: Notify,
    resume: Notify,
    calls: AtomicUsize,
    failures: AtomicUsize,
    skip_pause: AtomicBool,
    missing_reservation: AtomicBool,
}

impl Gate {
    async fn pause(&self) -> Result<(), RepositoryError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.entered.notify_one();
        if self
            .failures
            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |n| n.checked_sub(1))
            .is_ok()
        {
            return Err(RepositoryError::DatabaseError(anyhow::anyhow!(
                "injected charge failure"
            )));
        }
        if !self.skip_pause.load(Ordering::SeqCst) {
            self.resume.notified().await;
        }
        Ok(())
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
        rules: VoucherRules,
    ) -> Result<VoucherOutcome, RepositoryError> {
        self.inner
            .accept_voucher(channel_id, cumulative, signature, rules)
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
        self.charge.pause().await?;
        if self.charge.missing_reservation.load(Ordering::SeqCst) {
            return Ok(ChargeOutcome::NoReservation);
        }
        self.inner.charge(request_id, actual, units).await
    }

    async fn release(&self, request_id: Uuid) -> Result<bool, RepositoryError> {
        self.release.pause().await?;
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
        now: DateTime<Utc>,
    ) -> Result<CloseTransition, RepositoryError> {
        self.inner.mark_closing(channel_id, pending, now).await
    }

    async fn finalize(
        &self,
        channel_id: &[u8; 32],
        settled_on_chain: u128,
    ) -> Result<(), RepositoryError> {
        self.inner.finalize(channel_id, settled_on_chain).await
    }

    async fn clear_closing(
        &self,
        channel_id: &[u8; 32],
        expected_tx_hash: &[u8; 32],
    ) -> Result<bool, RepositoryError> {
        self.inner.clear_closing(channel_id, expected_tx_hash).await
    }

    async fn finalize_pending_close(
        &self,
        channel_id: &[u8; 32],
        expected_tx_hash: &[u8; 32],
        settled_on_chain: u128,
    ) -> Result<bool, RepositoryError> {
        self.inner
            .finalize_pending_close(channel_id, expected_tx_hash, settled_on_chain)
            .await
    }

    async fn claim_chain_read(
        &self,
        channel_id: &[u8; 32],
        now: DateTime<Utc>,
        min_interval: chrono::Duration,
    ) -> Result<bool, RepositoryError> {
        self.inner
            .claim_chain_read(channel_id, now, min_interval)
            .await
    }

    async fn claim_settlement_lease(
        &self,
        channel_id: &[u8; 32],
        now: DateTime<Utc>,
        until: DateTime<Utc>,
    ) -> Result<Option<(ChannelRecord, SettlementLease)>, RepositoryError> {
        self.inner
            .claim_settlement_lease(channel_id, now, until)
            .await
    }

    async fn clear_chain_read_claim(
        &self,
        channel_id: &[u8; 32],
        claimed_at: DateTime<Utc>,
    ) -> Result<bool, RepositoryError> {
        self.inner
            .clear_chain_read_claim(channel_id, claimed_at)
            .await
    }

    async fn record_settlement(
        &self,
        channel_id: &[u8; 32],
        lease: SettlementLease,
        cumulative: u128,
        at: DateTime<Utc>,
    ) -> Result<bool, RepositoryError> {
        self.inner
            .record_settlement(channel_id, lease, cumulative, at)
            .await
    }

    async fn release_settlement_lease(
        &self,
        channel_id: &[u8; 32],
        lease: SettlementLease,
    ) -> Result<bool, RepositoryError> {
        self.inner.release_settlement_lease(channel_id, lease).await
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

pub(super) async fn await_finalized(app: &SessionApp) {
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

#[tokio::test]
async fn charge_retries_two_store_failures_and_records_once() {
    let (app, charge, release) = app_with_delayed_ledger();
    seed(&app).await;
    charge.failures.store(2, Ordering::SeqCst);
    charge.skip_pause.store(true, Ordering::SeqCst);
    let credential = session_credential(&app, voucher(INITIAL)).await;
    let response = post_session(&app, Some(&credential)).await;
    assert_eq!(response.status(), StatusCode::OK);
    assert!(response.headers().contains_key("payment-receipt"));
    assert_eq!(charge.calls.load(Ordering::SeqCst), 3);
    assert_eq!(release.calls.load(Ordering::SeqCst), 0);
    assert_eq!(
        (
            row(&app).await.spent,
            row(&app).await.units,
            row(&app).await.reserved
        ),
        (24, 12, 0)
    );
}

#[tokio::test]
async fn permanent_charge_failure_preserves_success_without_receipt() {
    let (app, charge, release) = app_with_delayed_ledger();
    seed(&app).await;
    charge.failures.store(usize::MAX, Ordering::SeqCst);
    let credential = session_credential(&app, voucher(INITIAL)).await;
    let response = post_session(&app, Some(&credential)).await;
    assert_eq!(response.status(), StatusCode::OK);
    assert!(!response.headers().contains_key("payment-receipt"));
    assert_eq!(charge.calls.load(Ordering::SeqCst), 3);
    assert_eq!(release.calls.load(Ordering::SeqCst), 0);
    assert_eq!(app.ledger.reservation_count(), 1);
    assert_eq!(
        axum::body::to_bytes(response.into_body(), 1024)
            .await
            .unwrap()
            .as_ref(),
        b"{\"status\":\"ok\"}"
    );
}

#[tokio::test]
async fn missing_reservation_after_success_preserves_completion_without_receipt() {
    let (app, charge, _) = app_with_delayed_ledger();
    seed(&app).await;
    charge.skip_pause.store(true, Ordering::SeqCst);
    charge.missing_reservation.store(true, Ordering::SeqCst);
    let credential = session_credential(&app, voucher(INITIAL)).await;
    let response = post_session(&app, Some(&credential)).await;
    assert_eq!(response.status(), StatusCode::OK);
    assert!(!response.headers().contains_key("payment-receipt"));
    assert!(response.headers().get(header::WWW_AUTHENTICATE).is_none());
    assert_eq!(
        axum::body::to_bytes(response.into_body(), 1024)
            .await
            .unwrap()
            .as_ref(),
        b"{\"status\":\"ok\"}"
    );
}
