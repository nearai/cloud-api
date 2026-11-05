use super::{store_error, SessionService, CHAIN_READ_FRESHNESS_SECONDS};
use crate::mpp::error::{MppError, UnavailableReason};
use crate::mpp::ports::{chain_read_is_fresh, chain_read_last_observed_at, ChannelRecord};
use chrono::Utc;
use tokio::sync::oneshot;

const MAX_STALE_READ_SECS: i64 = 300;

impl SessionService {
    pub(super) async fn refresh(&self, record: ChannelRecord) -> Result<ChannelRecord, MppError> {
        Self::accepting(&record)?;
        let interval = chrono::Duration::seconds(CHAIN_READ_FRESHNESS_SECONDS);
        if chain_read_is_fresh(record.last_chain_read_at, Utc::now(), interval) {
            return Ok(record);
        }
        // The worker owns acquisition through cleanup, including a claim that
        // commits after the request has disappeared. Dropping the caller signals
        // cancellation of the RPC; it cannot cancel claim cleanup or persistence.
        let (caller, cancelled) = oneshot::channel::<()>();
        let service = self.clone();
        let task = tokio::spawn(async move { service.refresh_claim(record, cancelled).await });
        let result = task
            .await
            .map_err(|_| MppError::Unavailable(UnavailableReason::Rpc))?;
        drop(caller);
        result
    }

    async fn refresh_claim(
        &self,
        record: ChannelRecord,
        mut cancelled: oneshot::Receiver<()>,
    ) -> Result<ChannelRecord, MppError> {
        let now = Utc::now();
        let interval = chrono::Duration::seconds(CHAIN_READ_FRESHNESS_SECONDS);
        let claim = self
            .repo
            .claim_chain_read_lease(&record.channel_id, now, interval)
            .await
            .map_err(store_error)?;
        let Some(claim) = claim else {
            let current = self.get(&record.channel_id).await?;
            Self::accepting(&current)?;
            let now = Utc::now();
            let pending = current
                .last_chain_read_at
                .is_some_and(|at| at.timestamp_micros() < 0);
            let usable_observation = pending
                && chain_read_last_observed_at(current.last_chain_read_at).is_some_and(|at| {
                    at <= now
                        && now.signed_duration_since(at)
                            <= chrono::Duration::seconds(MAX_STALE_READ_SECS)
                });
            return if chain_read_is_fresh(current.last_chain_read_at, now, interval)
                || usable_observation
            {
                Ok(current)
            } else {
                Err(MppError::Unavailable(UnavailableReason::Rpc))
            };
        };
        let read = tokio::select! {
            biased;
            _ = &mut cancelled => Err(MppError::Unavailable(UnavailableReason::Rpc)),
            result = self.read_chain(&record.channel_id) => result,
        };
        let result = match read {
            Ok(chain) => self.record_chain(&record.channel_id, chain).await,
            Err(error) => Err(error),
        };
        if result.is_err() {
            self.repo
                .clear_chain_read_claim(&record.channel_id, claim.at())
                .await
                .map_err(store_error)?;
        }
        result
    }
}
