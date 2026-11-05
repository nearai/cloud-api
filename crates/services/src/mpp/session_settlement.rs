//! Periodic on-chain settlement of session vouchers.

use std::sync::Arc;
use std::time::{Duration, Instant};

use chrono::{DateTime, Utc};
use tokio::task::JoinHandle;

use crate::mpp::abi::settle_calldata;
use crate::mpp::ports::{
    ChannelLifecycle, ChannelRecord, ChannelRepository, TempoRpc, TempoSigner,
};
use crate::mpp::session_chain::{broadcast, build_payee_tx, PayeeChain};

/// Scheduling thresholds and bounded ledger page size.
#[derive(Clone)]
pub struct SettlementConfig {
    /// Maximum time between settlements when a settleable voucher exists.
    pub interval: Duration,
    /// Spend added since the last settlement that makes another settlement due.
    pub amount: u128,
    /// Duration of the exclusive settlement lease; defaults to 60 seconds.
    pub lease: Duration,
    /// Maximum records per ledger page; defaults to 100.
    pub page: usize,
}

impl Default for SettlementConfig {
    fn default() -> Self {
        Self {
            interval: Duration::from_secs(3600),
            amount: 5_000_000,
            lease: Duration::from_secs(60),
            page: 100,
        }
    }
}

/// Counts from one pass. No channel identifiers or amounts are exposed.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct SettlementSummary {
    /// Open channels examined.
    pub considered: u64,
    /// Channels meeting either schedule threshold.
    pub due: u64,
    /// Successful broadcasts recorded in the ledger.
    pub settled: u64,
    /// Due channels without a voucher bounded by delivered spend.
    pub no_settleable_voucher: u64,
    /// Due channels whose settlement lease was unavailable.
    pub lease_busy: u64,
    /// Failed operations, including ledger cleanup failures.
    pub failed: u64,
}

fn is_due(record: &ChannelRecord, cfg: &SettlementConfig, now: DateTime<Utc>) -> bool {
    record.spent.saturating_sub(record.last_settlement_spent) >= cfg.amount
        || record.last_settlement_at.is_none_or(|at| {
            now.signed_duration_since(at)
                .to_std()
                .is_ok_and(|age| age >= cfg.interval)
        })
}

async fn release(repo: &dyn ChannelRepository, id: &[u8; 32], summary: &mut SettlementSummary) {
    if repo.release_settlement_lease(id).await.is_err() {
        summary.failed += 1;
        tracing::warn!(
            outcome = "settlement_release_failed",
            "MPP settlement lease release failed"
        );
    }
}

/// Settle due open channels in channel-id order. The lease result is authoritative:
/// lifecycle, schedule and voucher are checked again after acquisition. A failed
/// attempt releases its lease; only a successful receipt advances the ledger.
pub async fn run_settlement_pass(
    repo: &dyn ChannelRepository,
    rpc: &dyn TempoRpc,
    signer: &dyn TempoSigner,
    chain: PayeeChain,
    cfg: &SettlementConfig,
    now: DateTime<Utc>,
) -> SettlementSummary {
    let started = Instant::now();
    let mut summary = SettlementSummary::default();
    let Ok(lease) = chrono::Duration::from_std(cfg.lease) else {
        summary.failed = 1;
        return summary;
    };
    if cfg.page == 0 || cfg.lease.is_zero() {
        summary.failed = 1;
        return summary;
    }
    let mut after = None;
    loop {
        let records = match repo.list_active(after, cfg.page).await {
            Ok(records) => records,
            Err(_) => {
                summary.failed += 1;
                break;
            }
        };
        let Some(last) = records.last() else {
            break;
        };
        let next = last.channel_id;
        if after.is_some_and(|previous| next <= previous) {
            summary.failed += 1;
            break;
        }
        after = Some(next);
        for record in records {
            if record.state != ChannelLifecycle::Open {
                continue;
            }
            summary.considered += 1;
            if record.chain_id != chain.chain_id
                || record.escrow != chain.escrow
                || record.descriptor.payee != signer.address()
            {
                summary.failed += 1;
                continue;
            }
            // A long page scan must not reuse the pass start as a transaction expiry
            // or claim a lease whose deadline has already passed.
            let Some(now) = i64::try_from(started.elapsed().as_secs())
                .ok()
                .and_then(|seconds| now.checked_add_signed(chrono::Duration::seconds(seconds)))
            else {
                summary.failed += 1;
                continue;
            };
            let (Some(until), Ok(now_unix)) = (
                now.checked_add_signed(lease),
                u64::try_from(now.timestamp()),
            ) else {
                summary.failed += 1;
                continue;
            };
            if !is_due(&record, cfg, now) {
                continue;
            }
            summary.due += 1;
            if record.settleable_voucher().is_none() {
                summary.no_settleable_voucher += 1;
                continue;
            }
            let id = record.channel_id;
            let current = match repo.claim_settlement_lease(&id, now, until).await {
                Ok(Some(current)) => current,
                Ok(None) => {
                    summary.lease_busy += 1;
                    continue;
                }
                Err(_) => {
                    summary.failed += 1;
                    continue;
                }
            };
            if current.state != ChannelLifecycle::Open || !is_due(&current, cfg, now) {
                release(repo, &id, &mut summary).await;
                continue;
            }
            let Some(voucher) = current.settleable_voucher() else {
                summary.no_settleable_voucher += 1;
                release(repo, &id, &mut summary).await;
                continue;
            };
            let attempt = async {
                let calldata =
                    settle_calldata(&current.descriptor, voucher.cumulative, &voucher.signature)
                        .map_err(|_| "settlement_calldata_failed")?;
                let tx = build_payee_tx(chain, signer, rpc, calldata, now_unix)
                    .await
                    .map_err(|error| error.code())?;
                broadcast(rpc, &tx).await.map_err(|error| error.code())?;
                repo.record_settlement(&id, voucher.cumulative, now)
                    .await
                    .map_err(|_| "settlement_record_failed")
            };
            // The port has no lease owner token or renewal. Bound work below the
            // lease lifetime so a stalled RPC cannot keep acting after reassignment.
            let result = tokio::time::timeout(cfg.lease / 2, attempt)
                .await
                .unwrap_or(Err("settlement_timeout"));
            match result {
                Ok(()) => summary.settled += 1,
                Err(outcome) => {
                    summary.failed += 1;
                    tracing::warn!(outcome, "MPP settlement failed");
                    release(repo, &id, &mut summary).await;
                }
            }
        }
    }
    summary
}

/// Spawn settlement on the current Tokio runtime. Runs immediately, then every quarter
/// of the configured settlement interval, with a minimum cadence of 60 seconds.
pub fn spawn_settlement_task(
    repo: Arc<dyn ChannelRepository>,
    rpc: Arc<dyn TempoRpc>,
    signer: Arc<dyn TempoSigner>,
    chain: PayeeChain,
    cfg: SettlementConfig,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval((cfg.interval / 4).max(Duration::from_secs(60)));
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            ticker.tick().await;
            let summary = run_settlement_pass(
                repo.as_ref(),
                rpc.as_ref(),
                signer.as_ref(),
                chain,
                &cfg,
                Utc::now(),
            )
            .await;
            tracing::info!(
                considered = summary.considered,
                due = summary.due,
                settled = summary.settled,
                no_settleable_voucher = summary.no_settleable_voucher,
                lease_busy = summary.lease_busy,
                failed = summary.failed,
                "MPP settlement pass completed"
            );
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mpp::channel_memory::InMemoryChannelRepository;
    use crate::mpp::ports::{NewChannel, PendingClose, ReserveOutcome, RpcError, RpcReceipt};
    use crate::mpp::session_vectors::Vectors;
    use crate::mpp::signer::LocalSigner;
    use crate::mpp::testing::{FakeTempo, SendMode};
    use uuid::Uuid;

    struct Harness {
        repo: InMemoryChannelRepository,
        rpc: FakeTempo,
        signer: LocalSigner,
        chain: PayeeChain,
        now: DateTime<Utc>,
        id: [u8; 32],
    }

    impl Harness {
        async fn new() -> Self {
            let v = Vectors::load();
            let now = DateTime::from_timestamp(1_800_000_000, 0).unwrap();
            let repo = InMemoryChannelRepository::with_clock(now);
            let id = v.fixed("channelId");
            repo.open_or_refresh(NewChannel {
                channel_id: id,
                chain_id: v.chain_id(),
                escrow: v.escrow(),
                descriptor: v.descriptor(),
                deposit: 1000,
                settled_on_chain: 0,
                highest_cumulative: 60,
                highest_signature: v.fixed("voucher.signature"),
            })
            .await
            .unwrap();
            for cumulative in [100, 200] {
                repo.accept_voucher(&id, cumulative, v.fixed("voucher.signature"))
                    .await
                    .unwrap();
            }
            Self {
                repo,
                rpc: FakeTempo::default(),
                signer: LocalSigner::new(v.secret_key("payeeKey")),
                chain: PayeeChain {
                    chain_id: v.chain_id(),
                    escrow: v.escrow(),
                    fee_token: v.fixed("token"),
                },
                now,
                id,
            }
        }

        async fn reserve(&self, amount: u128) -> Uuid {
            let request = Uuid::new_v4();
            assert!(matches!(
                self.repo.reserve(&self.id, request, amount).await.unwrap(),
                ReserveOutcome::Reserved(_)
            ));
            request
        }

        async fn spend(&self, amount: u128) {
            let request = self.reserve(amount).await;
            self.repo.charge(request, amount, 1).await.unwrap();
        }

        async fn record(&self) -> ChannelRecord {
            self.repo.get(&self.id).await.unwrap().unwrap()
        }

        async fn mine(&self, cumulative: u128, success: bool) {
            let record = self.record().await;
            let data =
                settle_calldata(&record.descriptor, cumulative, &record.highest_signature).unwrap();
            let tx = build_payee_tx(
                self.chain,
                &self.signer,
                &self.rpc,
                data,
                self.now.timestamp() as u64,
            )
            .await
            .unwrap();
            self.rpc.mine(RpcReceipt {
                tx_hash: tx.tx_hash,
                block_number: 1,
                success,
                from: self.signer.address(),
                logs: vec![],
            });
        }

        async fn run(&self, cfg: &SettlementConfig) -> SettlementSummary {
            run_settlement_pass(
                &self.repo,
                &self.rpc,
                &self.signer,
                self.chain,
                cfg,
                self.now,
            )
            .await
        }
    }

    // Completes an in-flight request between the page read and lease acquisition.
    struct ClaimRace<'a> {
        inner: &'a InMemoryChannelRepository,
        request: Uuid,
    }

    #[async_trait::async_trait]
    impl ChannelRepository for ClaimRace<'_> {
        async fn open_or_refresh(
            &self,
            channel: NewChannel,
        ) -> Result<ChannelRecord, crate::common::RepositoryError> {
            self.inner.open_or_refresh(channel).await
        }
        async fn get(
            &self,
            channel_id: &[u8; 32],
        ) -> Result<Option<ChannelRecord>, crate::common::RepositoryError> {
            self.inner.get(channel_id).await
        }
        async fn accept_voucher(
            &self,
            channel_id: &[u8; 32],
            cumulative: u128,
            signature: [u8; 65],
        ) -> Result<crate::mpp::ports::VoucherOutcome, crate::common::RepositoryError> {
            self.inner
                .accept_voucher(channel_id, cumulative, signature)
                .await
        }
        async fn reserve(
            &self,
            channel_id: &[u8; 32],
            request_id: Uuid,
            amount: u128,
        ) -> Result<ReserveOutcome, crate::common::RepositoryError> {
            self.inner.reserve(channel_id, request_id, amount).await
        }
        async fn charge(
            &self,
            request_id: Uuid,
            actual: u128,
            units: u64,
        ) -> Result<crate::mpp::ports::ChargeOutcome, crate::common::RepositoryError> {
            self.inner.charge(request_id, actual, units).await
        }
        async fn release(&self, request_id: Uuid) -> Result<bool, crate::common::RepositoryError> {
            self.inner.release(request_id).await
        }
        async fn record_chain_state(
            &self,
            channel_id: &[u8; 32],
            state: crate::mpp::abi::ChannelState,
            read_at: DateTime<Utc>,
        ) -> Result<Option<ChannelRecord>, crate::common::RepositoryError> {
            self.inner
                .record_chain_state(channel_id, state, read_at)
                .await
        }
        async fn mark_closing(
            &self,
            channel_id: &[u8; 32],
            pending: PendingClose,
        ) -> Result<crate::mpp::ports::CloseTransition, crate::common::RepositoryError> {
            self.inner.mark_closing(channel_id, pending).await
        }
        async fn finalize(
            &self,
            channel_id: &[u8; 32],
            settled_on_chain: u128,
        ) -> Result<(), crate::common::RepositoryError> {
            self.inner.finalize(channel_id, settled_on_chain).await
        }
        async fn clear_closing(
            &self,
            channel_id: &[u8; 32],
        ) -> Result<(), crate::common::RepositoryError> {
            self.inner.clear_closing(channel_id).await
        }
        async fn record_settlement(
            &self,
            channel_id: &[u8; 32],
            cumulative: u128,
            at: DateTime<Utc>,
        ) -> Result<(), crate::common::RepositoryError> {
            self.inner
                .record_settlement(channel_id, cumulative, at)
                .await
        }
        async fn release_settlement_lease(
            &self,
            channel_id: &[u8; 32],
        ) -> Result<(), crate::common::RepositoryError> {
            self.inner.release_settlement_lease(channel_id).await
        }
        async fn list_active(
            &self,
            after: Option<[u8; 32]>,
            limit: usize,
        ) -> Result<Vec<ChannelRecord>, crate::common::RepositoryError> {
            self.inner.list_active(after, limit).await
        }
        async fn release_stale_reservations(
            &self,
            cutoff: DateTime<Utc>,
        ) -> Result<u64, crate::common::RepositoryError> {
            self.inner.release_stale_reservations(cutoff).await
        }
        async fn purge_finalized(
            &self,
            cutoff: DateTime<Utc>,
        ) -> Result<u64, crate::common::RepositoryError> {
            self.inner.purge_finalized(cutoff).await
        }
        async fn claim_settlement_lease(
            &self,
            channel_id: &[u8; 32],
            now: DateTime<Utc>,
            until: DateTime<Utc>,
        ) -> Result<Option<ChannelRecord>, crate::common::RepositoryError> {
            self.inner.charge(self.request, 60, 1).await?;
            self.inner
                .claim_settlement_lease(channel_id, now, until)
                .await
        }
    }

    #[tokio::test]
    async fn voucher_is_selected_again_from_the_leased_record() {
        let h = Harness::new().await;
        h.spend(80).await;
        let request = h.reserve(90).await;
        assert_eq!(
            h.record().await.settleable_voucher().unwrap().cumulative,
            60
        );
        h.mine(100, true).await;
        let repo = ClaimRace {
            inner: &h.repo,
            request,
        };
        let summary = run_settlement_pass(
            &repo,
            &h.rpc,
            &h.signer,
            h.chain,
            &SettlementConfig::default(),
            h.now,
        )
        .await;
        assert_eq!((summary.settled, summary.failed), (1, 0));
        let record = h.record().await;
        assert_eq!((record.delivered(), record.settled_on_chain), (140, 100));
        assert_eq!(h.rpc.send_calls(), 1);
    }

    #[tokio::test]
    async fn due_by_amount_at_the_exact_threshold() {
        let h = Harness::new().await;
        h.repo.record_settlement(&h.id, 0, h.now).await.unwrap();
        h.spend(100).await;
        h.mine(100, true).await;
        let summary = h
            .run(&SettlementConfig {
                amount: 100,
                ..Default::default()
            })
            .await;
        assert_eq!(
            (
                summary.considered,
                summary.due,
                summary.settled,
                summary.failed
            ),
            (1, 1, 1, 0)
        );
    }

    #[tokio::test]
    async fn due_by_interval_at_the_exact_boundary() {
        let h = Harness::new().await;
        h.spend(100).await;
        h.repo
            .record_settlement(&h.id, 0, h.now - chrono::Duration::hours(1))
            .await
            .unwrap();
        h.mine(100, true).await;
        let summary = h.run(&SettlementConfig::default()).await;
        assert_eq!((summary.due, summary.settled), (1, 1));
    }

    #[tokio::test]
    async fn no_previous_settlement_is_immediately_due() {
        let h = Harness::new().await;
        h.spend(60).await;
        h.mine(60, true).await;
        assert_eq!(h.run(&SettlementConfig::default()).await.settled, 1);
    }

    #[tokio::test]
    async fn recent_below_threshold_channel_is_not_due() {
        let h = Harness::new().await;
        h.repo
            .record_settlement(&h.id, 0, h.now - chrono::Duration::seconds(3599))
            .await
            .unwrap();
        h.spend(100).await;
        let summary = h
            .run(&SettlementConfig {
                amount: 101,
                ..Default::default()
            })
            .await;
        assert_eq!(
            (summary.considered, summary.due, summary.settled),
            (1, 0, 0)
        );
        assert_eq!(h.rpc.send_calls(), 0);
        assert_eq!(h.repo.lease_until(&h.id), None);
    }

    #[tokio::test]
    async fn closing_and_finalized_channels_are_skipped() {
        let h = Harness::new().await;
        h.spend(100).await;
        h.repo
            .mark_closing(
                &h.id,
                PendingClose {
                    tx_hash: [1; 32],
                    cumulative: 100,
                    capture: 100,
                    valid_before: 1_800_000_025,
                },
            )
            .await
            .unwrap();
        assert_eq!(
            h.run(&SettlementConfig::default()).await,
            SettlementSummary::default()
        );
        assert_eq!(h.record().await.state, ChannelLifecycle::Closing);
        h.repo.finalize(&h.id, 100).await.unwrap();
        assert_eq!(
            h.run(&SettlementConfig::default()).await,
            SettlementSummary::default()
        );
        assert_eq!(h.rpc.send_calls(), 0);
    }

    #[tokio::test]
    async fn settlement_never_captures_reserved_spend() {
        let h = Harness::new().await;
        h.spend(80).await;
        h.reserve(90).await;
        // The retained 100 voucher is below spent=170 but above delivered=80.
        // Only the exact transaction settling 60 has a successful fake receipt.
        h.mine(60, true).await;
        let summary = h.run(&SettlementConfig::default()).await;
        assert_eq!((summary.settled, summary.failed), (1, 0));
        let record = h.record().await;
        assert_eq!(
            (record.settled_on_chain, record.spent, record.reserved),
            (60, 170, 90)
        );
        assert_eq!(
            record
                .retained_vouchers
                .iter()
                .map(|v| v.cumulative)
                .collect::<Vec<_>>(),
            vec![100]
        );
        assert_eq!(h.rpc.send_calls(), 1);
    }

    #[tokio::test]
    async fn no_voucher_at_or_below_delivered_means_no_transaction() {
        let h = Harness::new().await;
        h.reserve(100).await;
        let summary = h.run(&SettlementConfig::default()).await;
        assert_eq!(
            (summary.due, summary.no_settleable_voucher, summary.settled),
            (1, 1, 0)
        );
        assert_eq!(h.rpc.send_calls(), 0);
        assert_eq!(h.repo.lease_until(&h.id), None);
    }

    #[tokio::test]
    async fn an_existing_lease_is_left_untouched() {
        let h = Harness::new().await;
        h.spend(100).await;
        let until = h.now + chrono::Duration::seconds(60);
        h.repo
            .claim_settlement_lease(&h.id, h.now, until)
            .await
            .unwrap()
            .unwrap();
        let summary = h.run(&SettlementConfig::default()).await;
        assert_eq!((summary.lease_busy, summary.settled), (1, 0));
        assert_eq!(h.repo.lease_until(&h.id), Some(until));
        assert_eq!(h.rpc.send_calls(), 0);
    }

    #[tokio::test]
    async fn gas_broadcast_and_revert_failures_release_the_lease() {
        for failure in 0..3 {
            let h = Harness::new().await;
            h.spend(100).await;
            match failure {
                0 => h.rpc.set_gas_price(Err(RpcError::Unavailable)),
                1 => h.rpc.set_send_mode(SendMode::Unavailable),
                _ => h.mine(100, false).await,
            }
            let summary = h.run(&SettlementConfig::default()).await;
            assert_eq!((summary.failed, summary.settled), (1, 0));
            assert_eq!(h.repo.lease_until(&h.id), None);
            let record = h.record().await;
            assert_eq!(record.settled_on_chain, 0);
            assert_eq!(record.last_settlement_at, None);
            assert_eq!(record.retained_vouchers.len(), 2);
            h.rpc.set_gas_price(Ok(600_000_001));
            h.rpc.set_send_mode(SendMode::Normal);
            h.mine(100, true).await;
            assert_eq!(h.run(&SettlementConfig::default()).await.settled, 1);
        }
    }

    #[tokio::test]
    async fn success_records_schedule_and_drops_captured_vouchers() {
        let h = Harness::new().await;
        h.spend(100).await;
        h.mine(100, true).await;
        assert_eq!(h.run(&SettlementConfig::default()).await.settled, 1);
        let record = h.record().await;
        assert_eq!(record.settled_on_chain, 100);
        assert_eq!(record.last_settlement_at, Some(h.now));
        assert_eq!(record.last_settlement_spent, 100);
        assert!(record.retained_vouchers.is_empty());
        assert_eq!(record.highest_cumulative, 200);
        assert_eq!(h.repo.lease_until(&h.id), None);
        assert_eq!(h.run(&SettlementConfig::default()).await.due, 0);
        assert_eq!(h.rpc.send_calls(), 1);
    }

    #[tokio::test]
    async fn a_hanging_broadcast_releases_the_lease_before_its_deadline() {
        let h = Harness::new().await;
        h.spend(100).await;
        h.rpc.set_send_mode(SendMode::Hang);
        let summary = h
            .run(&SettlementConfig {
                lease: Duration::from_millis(200),
                ..Default::default()
            })
            .await;
        assert_eq!((summary.failed, summary.settled), (1, 0));
        assert_eq!(h.repo.lease_until(&h.id), None);
        assert_eq!(h.rpc.send_calls(), 1);
    }

    #[tokio::test]
    async fn a_different_chain_is_not_claimed_or_broadcast() {
        let h = Harness::new().await;
        h.spend(100).await;
        let chain = PayeeChain {
            chain_id: h.chain.chain_id + 1,
            ..h.chain
        };
        let summary = run_settlement_pass(
            &h.repo,
            &h.rpc,
            &h.signer,
            chain,
            &SettlementConfig::default(),
            h.now,
        )
        .await;
        assert_eq!((summary.failed, summary.settled), (1, 0));
        assert_eq!(h.rpc.send_calls(), 0);
        assert_eq!(h.repo.lease_until(&h.id), None);
    }
}
