//! In-memory [`super::ports::ChannelRepository`]: the ledger's state machine for unit
//! tests and local harnesses. Per process and forgotten on restart: never for a deployment.
//!
//! This is the reference for the money rules: the Postgres repository must produce the same
//! records for the same calls. One mutex guards every table, so each method is one atomic
//! step, as the trait requires.
//!
//! Time is a fake clock. Every write moves it forward by one microsecond and stamps the row
//! (`updated_at`, a reservation's `created_at`) with it, so two writes never share a stamp;
//! [`InMemoryChannelRepository::set_now`] and [`InMemoryChannelRepository::age_channel`]
//! steer it in tests. Timestamps the trait passes in (`now`, `at`, `read_at`, `cutoff`) are
//! used as given.

use std::collections::{BTreeMap, HashMap};
use std::ops::Bound;
use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Mutex,
};

use async_trait::async_trait;
use chrono::{DateTime, Duration, Utc};
use uuid::Uuid;

use super::abi::ChannelState;
use super::ports::{
    chain_read_claim_with_observation, chain_read_may_claim, chain_read_observed_at,
    ChainReadClaim, ChannelLifecycle, ChannelRecord, ChannelRepository, ChargeOutcome,
    CloseTransition, NewChannel, PendingClose, ReserveOutcome, RetainedVoucher, SettlementLease,
    VoucherOutcome, VoucherRules, MAX_RETAINED_VOUCHERS,
};
use super::secp256k1::SIGNATURE_LEN;
use crate::common::RepositoryError;

#[derive(Debug, Clone)]
struct Row {
    record: ChannelRecord,
    lease_until: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone, Copy)]
struct Reservation {
    channel_id: [u8; 32],
    amount: u128,
    created_at: DateTime<Utc>,
}

#[derive(Debug)]
struct State {
    channels: BTreeMap<[u8; 32], Row>,
    reservations: HashMap<Uuid, Reservation>,
    clock: DateTime<Utc>,
}

impl State {
    /// A stamp no earlier write has used, as `updated_at = NOW()` gives the real table.
    fn tick(&mut self) -> DateTime<Utc> {
        self.clock += Duration::microseconds(1);
        self.clock
    }

    /// `spent -= amount`, `reserved -= amount` on the channel of a dropped reservation.
    fn give_back(&mut self, reservation: &Reservation) {
        let stamp = self.tick();
        if let Some(row) = self.channels.get_mut(&reservation.channel_id) {
            row.record.spent = row.record.spent.saturating_sub(reservation.amount);
            row.record.raise_spent_floor(spent_floor(&row.record));
            row.record.reserved = row.record.reserved.saturating_sub(reservation.amount);
            row.record.updated_at = stamp;
        }
    }
}

/// Channels and reservations, in memory.
#[derive(Debug)]
pub struct InMemoryChannelRepository {
    state: Mutex<State>,
    chain_read_claims: AtomicUsize,
    chain_state_writes: AtomicUsize,
}

impl Default for InMemoryChannelRepository {
    fn default() -> Self {
        Self::new()
    }
}

impl InMemoryChannelRepository {
    /// An empty ledger whose clock starts at the current time.
    pub fn new() -> Self {
        Self::with_clock(Utc::now())
    }

    /// An empty ledger whose clock starts at `now`.
    pub fn with_clock(now: DateTime<Utc>) -> Self {
        Self {
            chain_read_claims: AtomicUsize::new(0),
            chain_state_writes: AtomicUsize::new(0),
            state: Mutex::new(State {
                channels: BTreeMap::new(),
                reservations: HashMap::new(),
                clock: now,
            }),
        }
    }

    /// Count refresh claims, including rejected claims, for boundary assertions.
    pub fn chain_read_claim_count(&self) -> usize {
        self.chain_read_claims.load(Ordering::Relaxed)
    }

    /// Count changes to chain-state data, excluding read-timestamp-only refreshes.
    pub fn chain_state_write_count(&self) -> usize {
        self.chain_state_writes.load(Ordering::Relaxed)
    }

    /// Set the fake clock; later writes are stamped just after it. For tests.
    pub fn set_now(&self, now: DateTime<Utc>) {
        if let Ok(mut state) = self.state.lock() {
            state.clock = now;
        }
    }

    /// The fake clock as of the last write. For tests.
    pub fn now(&self) -> DateTime<Utc> {
        self.state
            .lock()
            .map(|state| state.clock)
            .unwrap_or(DateTime::<Utc>::UNIX_EPOCH)
    }

    /// Move the fake clock forward by `by`. For tests.
    pub fn advance(&self, by: Duration) {
        if let Ok(mut state) = self.state.lock() {
            state.clock += by;
        }
    }

    /// Number of held reservations. For assertions.
    pub fn reservation_count(&self) -> usize {
        self.state
            .lock()
            .map(|state| state.reservations.len())
            .unwrap_or(0)
    }

    /// Number of stored channels, finalized ones included. For assertions.
    pub fn channel_count(&self) -> usize {
        self.state
            .lock()
            .map(|state| state.channels.len())
            .unwrap_or(0)
    }

    /// Pretend the reservation of `request_id` was made `by` earlier. For stale-release
    /// tests.
    pub fn age_reservation(&self, request_id: Uuid, by: Duration) {
        if let Ok(mut state) = self.state.lock() {
            if let Some(reservation) = state.reservations.get_mut(&request_id) {
                reservation.created_at -= by;
            }
        }
    }

    /// Pretend the channel was last written `by` earlier. For purge tests.
    pub fn age_channel(&self, channel_id: &[u8; 32], by: Duration) {
        if let Ok(mut state) = self.state.lock() {
            if let Some(row) = state.channels.get_mut(channel_id) {
                row.record.updated_at -= by;
            }
        }
    }

    /// Age the last refresh claim, for cache-expiry tests.
    pub fn age_chain_read(&self, channel_id: &[u8; 32], by: Duration) {
        if let Ok(mut state) = self.state.lock() {
            if let Some(row) = state.channels.get_mut(channel_id) {
                row.record.last_chain_read_at = row.record.last_chain_read_at.map(|at| at - by);
            }
        }
    }

    /// Whether a settlement lease is held on the channel (whatever its expiry). For
    /// assertions.
    pub fn lease_until(&self, channel_id: &[u8; 32]) -> Option<DateTime<Utc>> {
        self.state
            .lock()
            .ok()
            .and_then(|state| state.channels.get(channel_id).and_then(|r| r.lease_until))
    }
}

fn poisoned() -> RepositoryError {
    RepositoryError::DatabaseError(anyhow::anyhow!("in-memory channel store poisoned"))
}

/// A submitted close can capture this amount even while its reservations drain.
fn spent_floor(record: &ChannelRecord) -> u128 {
    match record.state {
        ChannelLifecycle::Open => 0,
        ChannelLifecycle::Closing => record.pending_close.map_or(0, |pending| pending.capture),
        ChannelLifecycle::Finalized => record.settled_on_chain,
    }
}

/// Keep `old` as a retained voucher: highest first, at most [`MAX_RETAINED_VOUCHERS`].
fn retain_voucher(record: &mut ChannelRecord, old: RetainedVoucher) {
    record.retained_vouchers.push(old);
    record
        .retained_vouchers
        .sort_by(|a, b| b.cumulative.cmp(&a.cumulative));
    let settleable = record
        .retained_vouchers
        .iter()
        .copied()
        .filter(|voucher| {
            voucher.cumulative <= record.delivered() && voucher.cumulative > record.settled_on_chain
        })
        .max_by_key(|voucher| voucher.cumulative);
    record.retained_vouchers.truncate(MAX_RETAINED_VOUCHERS);
    if let Some(settleable) = settleable {
        if !record.retained_vouchers.contains(&settleable) {
            record.retained_vouchers.pop();
            record.retained_vouchers.push(settleable);
        }
    }
}

/// Make `(cumulative, signature)` the highest voucher, retaining the one it replaces.
fn promote_voucher(record: &mut ChannelRecord, cumulative: u128, signature: [u8; SIGNATURE_LEN]) {
    let old = RetainedVoucher {
        cumulative: record.highest_cumulative,
        signature: record.highest_signature,
    };
    retain_voucher(record, old);
    record.highest_cumulative = cumulative;
    record.highest_signature = signature;
}

#[async_trait]
impl ChannelRepository for InMemoryChannelRepository {
    async fn open_or_refresh(&self, channel: NewChannel) -> Result<ChannelRecord, RepositoryError> {
        let mut state = self.state.lock().map_err(|_| poisoned())?;
        let stamp = state.tick();
        if let Some(row) = state.channels.get_mut(&channel.channel_id) {
            let record = &mut row.record;
            record.deposit = record.deposit.max(channel.deposit);
            record.settled_on_chain = record.settled_on_chain.max(channel.settled_on_chain);
            record.updated_at = stamp;
            return Ok(record.clone());
        }
        let record = ChannelRecord {
            channel_id: channel.channel_id,
            chain_id: channel.chain_id,
            escrow: channel.escrow,
            descriptor: channel.descriptor,
            deposit: channel.deposit,
            settled_on_chain: channel.settled_on_chain,
            highest_cumulative: channel.highest_cumulative,
            highest_signature: channel.highest_signature,
            retained_vouchers: Vec::new(),
            spent: 0,
            reserved: 0,
            units: 0,
            state: ChannelLifecycle::Open,
            close_requested_at: 0,
            pending_close: None,
            last_chain_read_at: None,
            last_settlement_at: None,
            last_settlement_spent: 0,
            updated_at: stamp,
        };
        state.channels.insert(
            channel.channel_id,
            Row {
                record: record.clone(),
                lease_until: None,
            },
        );
        Ok(record)
    }

    async fn get(&self, channel_id: &[u8; 32]) -> Result<Option<ChannelRecord>, RepositoryError> {
        let state = self.state.lock().map_err(|_| poisoned())?;
        Ok(state.channels.get(channel_id).map(|row| row.record.clone()))
    }

    async fn accept_voucher(
        &self,
        channel_id: &[u8; 32],
        cumulative: u128,
        signature: [u8; SIGNATURE_LEN],
        rules: VoucherRules,
    ) -> Result<VoucherOutcome, RepositoryError> {
        let mut state = self.state.lock().map_err(|_| poisoned())?;
        let stamp = state.tick();
        let Some(row) = state.channels.get_mut(channel_id) else {
            return Ok(VoucherOutcome::Missing);
        };
        if !row.record.accepts_vouchers() {
            return Ok(VoucherOutcome::NotAccepting);
        }
        if cumulative <= row.record.highest_cumulative {
            return Ok(VoucherOutcome::NotHigher);
        }
        if cumulative - row.record.highest_cumulative < rules.min_delta {
            return Ok(VoucherOutcome::DeltaTooSmall);
        }
        if rules.settled_floor_from_chain && cumulative < row.record.settled_on_chain {
            return Ok(VoucherOutcome::BelowSettled);
        }
        if cumulative > row.record.deposit {
            return Ok(VoucherOutcome::AboveDeposit);
        }
        promote_voucher(&mut row.record, cumulative, signature);
        row.record.updated_at = stamp;
        Ok(VoucherOutcome::Accepted(Box::new(row.record.clone())))
    }

    async fn reserve(
        &self,
        channel_id: &[u8; 32],
        request_id: Uuid,
        amount: u128,
    ) -> Result<ReserveOutcome, RepositoryError> {
        if amount == 0 {
            return Err(RepositoryError::ValidationFailed(
                "reservation amount is zero".to_string(),
            ));
        }
        let mut state = self.state.lock().map_err(|_| poisoned())?;
        if state.reservations.contains_key(&request_id) {
            return Err(RepositoryError::AlreadyExists);
        }
        let stamp = state.tick();
        let Some(row) = state.channels.get_mut(channel_id) else {
            return Ok(ReserveOutcome::Missing);
        };
        if !row.record.accepts_vouchers() {
            return Ok(ReserveOutcome::NotAccepting);
        }
        let available = row.record.available();
        if available < amount {
            return Ok(ReserveOutcome::Insufficient { available });
        }
        row.record.spent = row.record.spent.saturating_add(amount);
        row.record.reserved = row.record.reserved.saturating_add(amount);
        row.record.updated_at = stamp;
        let record = row.record.clone();
        state.reservations.insert(
            request_id,
            Reservation {
                channel_id: *channel_id,
                amount,
                created_at: stamp,
            },
        );
        Ok(ReserveOutcome::Reserved(Box::new(record)))
    }

    async fn charge(
        &self,
        request_id: Uuid,
        actual: u128,
        units: u64,
    ) -> Result<ChargeOutcome, RepositoryError> {
        let mut state = self.state.lock().map_err(|_| poisoned())?;
        let stamp = state.tick();
        let Some(reservation) = state.reservations.remove(&request_id) else {
            return Ok(ChargeOutcome::NoReservation);
        };
        let Some(row) = state.channels.get_mut(&reservation.channel_id) else {
            return Ok(ChargeOutcome::NoReservation);
        };
        let record = &mut row.record;
        let before = record.spent.saturating_sub(reservation.amount);
        let cap = record.highest_cumulative.saturating_sub(before);
        let charged = actual.min(cap);
        record.spent = before.saturating_add(charged);
        record.raise_spent_floor(spent_floor(record));
        record.reserved = record.reserved.saturating_sub(reservation.amount);
        record.units = record.units.saturating_add(units);
        record.updated_at = stamp;
        Ok(ChargeOutcome::Charged {
            record: Box::new(record.clone()),
            charged,
        })
    }

    async fn release(&self, request_id: Uuid) -> Result<bool, RepositoryError> {
        let mut state = self.state.lock().map_err(|_| poisoned())?;
        let Some(reservation) = state.reservations.remove(&request_id) else {
            return Ok(false);
        };
        state.give_back(&reservation);
        Ok(true)
    }

    async fn record_chain_state(
        &self,
        channel_id: &[u8; 32],
        chain: ChannelState,
        read_at: DateTime<Utc>,
    ) -> Result<Option<ChannelRecord>, RepositoryError> {
        let mut state = self.state.lock().map_err(|_| poisoned())?;
        let stamp = state.tick();
        let Some(row) = state.channels.get_mut(channel_id) else {
            return Ok(None);
        };
        let record = &mut row.record;
        let before = (
            record.deposit,
            record.settled_on_chain,
            record.close_requested_at,
        );
        if chain.deposit != 0 {
            record.deposit = record.deposit.max(chain.deposit);
        }
        record.settled_on_chain = record.settled_on_chain.max(chain.settled);
        let observed_at = chain_read_observed_at(record.last_chain_read_at, read_at);
        if observed_at == read_at {
            record.close_requested_at = chain.close_requested_at;
        }
        record.last_chain_read_at = Some(observed_at);
        if before
            != (
                record.deposit,
                record.settled_on_chain,
                record.close_requested_at,
            )
        {
            self.chain_state_writes.fetch_add(1, Ordering::Relaxed);
            record.updated_at = stamp;
        }
        Ok(Some(record.clone()))
    }

    async fn mark_closing(
        &self,
        channel_id: &[u8; 32],
        pending: PendingClose,
        now: DateTime<Utc>,
    ) -> Result<CloseTransition, RepositoryError> {
        let mut state = self.state.lock().map_err(|_| poisoned())?;
        let stamp = state.tick();
        let Some(row) = state.channels.get_mut(channel_id) else {
            return Ok(CloseTransition::Missing);
        };
        if row.record.state != ChannelLifecycle::Open {
            return Ok(CloseTransition::NotOpen);
        }
        if row.lease_until.is_some_and(|until| until > now) {
            return Ok(CloseTransition::SettlementInFlight);
        }
        row.record.state = ChannelLifecycle::Closing;
        row.record.pending_close = Some(pending);
        row.record.updated_at = stamp;
        Ok(CloseTransition::Marked(Box::new(row.record.clone())))
    }

    async fn finalize(
        &self,
        channel_id: &[u8; 32],
        settled_on_chain: u128,
    ) -> Result<(), RepositoryError> {
        let mut state = self.state.lock().map_err(|_| poisoned())?;
        let stamp = state.tick();
        if let Some(row) = state.channels.get_mut(channel_id) {
            row.record.state = ChannelLifecycle::Finalized;
            row.record.settled_on_chain = row.record.settled_on_chain.max(settled_on_chain);
            row.record.raise_spent_floor(row.record.settled_on_chain);
            row.record.pending_close = None;
            row.record.updated_at = stamp;
        }
        Ok(())
    }

    async fn clear_closing(
        &self,
        channel_id: &[u8; 32],
        expected_tx_hash: &[u8; 32],
    ) -> Result<bool, RepositoryError> {
        let mut state = self.state.lock().map_err(|_| poisoned())?;
        let stamp = state.tick();
        let Some(row) = state.channels.get_mut(channel_id) else {
            return Ok(false);
        };
        if row.record.state != ChannelLifecycle::Closing
            || row
                .record
                .pending_close
                .is_none_or(|pending| pending.tx_hash != *expected_tx_hash)
        {
            return Ok(false);
        }
        row.record.state = ChannelLifecycle::Open;
        row.record.pending_close = None;
        row.record.updated_at = stamp;
        Ok(true)
    }

    async fn finalize_pending_close(
        &self,
        channel_id: &[u8; 32],
        expected_tx_hash: &[u8; 32],
        settled_on_chain: u128,
    ) -> Result<bool, RepositoryError> {
        let mut state = self.state.lock().map_err(|_| poisoned())?;
        let stamp = state.tick();
        let Some(row) = state.channels.get_mut(channel_id) else {
            return Ok(false);
        };
        if row.record.state != ChannelLifecycle::Closing
            || row
                .record
                .pending_close
                .is_none_or(|pending| pending.tx_hash != *expected_tx_hash)
        {
            return Ok(false);
        }
        row.record.state = ChannelLifecycle::Finalized;
        row.record.settled_on_chain = row.record.settled_on_chain.max(settled_on_chain);
        row.record.raise_spent_floor(row.record.settled_on_chain);
        row.record.pending_close = None;
        row.record.updated_at = stamp;
        Ok(true)
    }

    async fn claim_chain_read_lease(
        &self,
        channel_id: &[u8; 32],
        now: DateTime<Utc>,
        min_interval: Duration,
    ) -> Result<Option<ChainReadClaim>, RepositoryError> {
        self.chain_read_claims.fetch_add(1, Ordering::Relaxed);
        let mut state = self.state.lock().map_err(|_| poisoned())?;
        let Some(row) = state.channels.get_mut(channel_id) else {
            return Ok(None);
        };
        if !chain_read_may_claim(row.record.last_chain_read_at, now, min_interval) {
            return Ok(None);
        }
        let claimed_at =
            chain_read_claim_with_observation(now, min_interval, row.record.last_chain_read_at)
                .ok_or_else(|| {
                    RepositoryError::ValidationFailed("chain read interval".to_string())
                })?;
        row.record.last_chain_read_at = Some(claimed_at);
        Ok(Some(ChainReadClaim::new(claimed_at)))
    }

    async fn clear_chain_read_claim(
        &self,
        channel_id: &[u8; 32],
        claimed_at: DateTime<Utc>,
    ) -> Result<bool, RepositoryError> {
        let mut state = self.state.lock().map_err(|_| poisoned())?;
        let Some(row) = state.channels.get_mut(channel_id) else {
            return Ok(false);
        };
        if row.record.last_chain_read_at != Some(claimed_at) {
            return Ok(false);
        }
        row.record.last_chain_read_at = None;
        Ok(true)
    }

    async fn claim_settlement_lease(
        &self,
        channel_id: &[u8; 32],
        now: DateTime<Utc>,
        until: DateTime<Utc>,
    ) -> Result<Option<(ChannelRecord, SettlementLease)>, RepositoryError> {
        let mut state = self.state.lock().map_err(|_| poisoned())?;
        let stamp = state.tick();
        let Some(row) = state.channels.get_mut(channel_id) else {
            return Ok(None);
        };
        // Match PostgreSQL's timestamp encoding, including sub-microsecond callers.
        let until = DateTime::from_timestamp_micros(until.timestamp_micros())
            .ok_or_else(|| RepositoryError::ValidationFailed("lease deadline".to_string()))?;
        if until <= now
            || row.record.state != ChannelLifecycle::Open
            || row.lease_until.is_some_and(|held| held >= now)
        {
            return Ok(None);
        }
        row.lease_until = Some(until);
        row.record.updated_at = stamp;
        Ok(Some((row.record.clone(), SettlementLease::new(until))))
    }

    async fn record_settlement(
        &self,
        channel_id: &[u8; 32],
        lease: SettlementLease,
        cumulative: u128,
        at: DateTime<Utc>,
    ) -> Result<bool, RepositoryError> {
        let mut state = self.state.lock().map_err(|_| poisoned())?;
        let stamp = state.tick();
        let Some(row) = state.channels.get_mut(channel_id) else {
            return Ok(false);
        };
        if row.lease_until != Some(lease.until()) || row.record.state != ChannelLifecycle::Open {
            return Ok(false);
        }
        let record = &mut row.record;
        record.settled_on_chain = record.settled_on_chain.max(cumulative);
        record.last_settlement_at = Some(at);
        record.last_settlement_spent = record.spent;
        record
            .retained_vouchers
            .retain(|voucher| voucher.cumulative > cumulative);
        record.updated_at = stamp;
        row.lease_until = None;
        Ok(true)
    }

    async fn release_settlement_lease(
        &self,
        channel_id: &[u8; 32],
        lease: SettlementLease,
    ) -> Result<bool, RepositoryError> {
        let mut state = self.state.lock().map_err(|_| poisoned())?;
        let stamp = state.tick();
        let Some(row) = state.channels.get_mut(channel_id) else {
            return Ok(false);
        };
        if row.lease_until != Some(lease.until()) {
            return Ok(false);
        }
        row.lease_until = None;
        row.record.updated_at = stamp;
        Ok(true)
    }

    async fn list_active(
        &self,
        after: Option<[u8; 32]>,
        limit: usize,
    ) -> Result<Vec<ChannelRecord>, RepositoryError> {
        let state = self.state.lock().map_err(|_| poisoned())?;
        let lower = match after {
            Some(id) => Bound::Excluded(id),
            None => Bound::Unbounded,
        };
        Ok(state
            .channels
            .range((lower, Bound::Unbounded))
            .map(|(_, row)| &row.record)
            .filter(|record| record.state != ChannelLifecycle::Finalized)
            .take(limit)
            .cloned()
            .collect())
    }

    async fn release_stale_reservations(
        &self,
        cutoff: DateTime<Utc>,
    ) -> Result<u64, RepositoryError> {
        let mut state = self.state.lock().map_err(|_| poisoned())?;
        let stale: Vec<Uuid> = state
            .reservations
            .iter()
            .filter(|(_, reservation)| reservation.created_at < cutoff)
            .map(|(request_id, _)| *request_id)
            .collect();
        let mut released = 0u64;
        for request_id in stale {
            if let Some(reservation) = state.reservations.remove(&request_id) {
                state.give_back(&reservation);
                released += 1;
            }
        }
        Ok(released)
    }

    async fn purge_finalized(&self, cutoff: DateTime<Utc>) -> Result<u64, RepositoryError> {
        let mut state = self.state.lock().map_err(|_| poisoned())?;
        let purge: Vec<[u8; 32]> = state
            .channels
            .iter()
            .filter(|(_, row)| {
                row.record.state == ChannelLifecycle::Finalized && row.record.updated_at < cutoff
            })
            .map(|(id, _)| *id)
            .collect();
        for id in &purge {
            state.channels.remove(id);
        }
        state
            .reservations
            .retain(|_, reservation| !purge.contains(&reservation.channel_id));
        Ok(purge.len() as u64)
    }
}
