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
use std::sync::Mutex;

use async_trait::async_trait;
use chrono::{DateTime, Duration, Utc};
use uuid::Uuid;

use super::abi::ChannelState;
use super::ports::{
    ChannelLifecycle, ChannelRecord, ChannelRepository, ChargeOutcome, CloseTransition, NewChannel,
    PendingClose, ReserveOutcome, RetainedVoucher, VoucherOutcome, MAX_RETAINED_VOUCHERS,
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
            row.record.reserved = row.record.reserved.saturating_sub(reservation.amount);
            row.record.updated_at = stamp;
        }
    }
}

/// Channels and reservations, in memory.
#[derive(Debug)]
pub struct InMemoryChannelRepository {
    state: Mutex<State>,
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
            state: Mutex::new(State {
                channels: BTreeMap::new(),
                reservations: HashMap::new(),
                clock: now,
            }),
        }
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

/// Keep `old` as a retained voucher: highest first, at most [`MAX_RETAINED_VOUCHERS`].
fn retain_voucher(record: &mut ChannelRecord, old: RetainedVoucher) {
    record.retained_vouchers.push(old);
    record
        .retained_vouchers
        .sort_by(|a, b| b.cumulative.cmp(&a.cumulative));
    record.retained_vouchers.truncate(MAX_RETAINED_VOUCHERS);
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
            if channel.highest_cumulative > record.highest_cumulative {
                promote_voucher(
                    record,
                    channel.highest_cumulative,
                    channel.highest_signature,
                );
            }
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
        if chain.deposit != 0 {
            record.deposit = record.deposit.max(chain.deposit);
        }
        record.settled_on_chain = record.settled_on_chain.max(chain.settled);
        if chain.close_requested_at != 0 {
            record.close_requested_at = chain.close_requested_at;
        }
        record.last_chain_read_at = Some(read_at);
        record.updated_at = stamp;
        Ok(Some(record.clone()))
    }

    async fn mark_closing(
        &self,
        channel_id: &[u8; 32],
        pending: PendingClose,
    ) -> Result<CloseTransition, RepositoryError> {
        let mut state = self.state.lock().map_err(|_| poisoned())?;
        let stamp = state.tick();
        let Some(row) = state.channels.get_mut(channel_id) else {
            return Ok(CloseTransition::Missing);
        };
        if row.record.state != ChannelLifecycle::Open {
            return Ok(CloseTransition::NotOpen);
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
            row.record.pending_close = None;
            row.record.updated_at = stamp;
        }
        Ok(())
    }

    async fn clear_closing(&self, channel_id: &[u8; 32]) -> Result<(), RepositoryError> {
        let mut state = self.state.lock().map_err(|_| poisoned())?;
        let stamp = state.tick();
        if let Some(row) = state.channels.get_mut(channel_id) {
            if row.record.state == ChannelLifecycle::Closing {
                row.record.state = ChannelLifecycle::Open;
                row.record.pending_close = None;
                row.record.updated_at = stamp;
            }
        }
        Ok(())
    }

    async fn claim_settlement_lease(
        &self,
        channel_id: &[u8; 32],
        now: DateTime<Utc>,
        until: DateTime<Utc>,
    ) -> Result<Option<ChannelRecord>, RepositoryError> {
        let mut state = self.state.lock().map_err(|_| poisoned())?;
        let stamp = state.tick();
        let Some(row) = state.channels.get_mut(channel_id) else {
            return Ok(None);
        };
        if row.lease_until.is_some_and(|held| held >= now) {
            return Ok(None);
        }
        row.lease_until = Some(until);
        row.record.updated_at = stamp;
        Ok(Some(row.record.clone()))
    }

    async fn record_settlement(
        &self,
        channel_id: &[u8; 32],
        cumulative: u128,
        at: DateTime<Utc>,
    ) -> Result<(), RepositoryError> {
        let mut state = self.state.lock().map_err(|_| poisoned())?;
        let stamp = state.tick();
        if let Some(row) = state.channels.get_mut(channel_id) {
            let record = &mut row.record;
            record.settled_on_chain = record.settled_on_chain.max(cumulative);
            record.last_settlement_at = Some(at);
            record.last_settlement_spent = record.spent;
            record
                .retained_vouchers
                .retain(|voucher| voucher.cumulative > cumulative);
            record.updated_at = stamp;
            row.lease_until = None;
        }
        Ok(())
    }

    async fn release_settlement_lease(&self, channel_id: &[u8; 32]) -> Result<(), RepositoryError> {
        let mut state = self.state.lock().map_err(|_| poisoned())?;
        let stamp = state.tick();
        if let Some(row) = state.channels.get_mut(channel_id) {
            row.lease_until = None;
            row.record.updated_at = stamp;
        }
        Ok(())
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mpp::abi::ChannelDescriptor;

    fn id(n: u8) -> [u8; 32] {
        [n; 32]
    }

    fn sig(n: u8) -> [u8; SIGNATURE_LEN] {
        [n; SIGNATURE_LEN]
    }

    fn new_channel(n: u8, deposit: u128, cumulative: u128) -> NewChannel {
        NewChannel {
            channel_id: id(n),
            chain_id: 4217,
            escrow: [0xee; 20],
            descriptor: ChannelDescriptor {
                payer: [1; 20],
                payee: [2; 20],
                operator: [3; 20],
                token: [4; 20],
                salt: [5; 32],
                authorized_signer: [0; 20],
                expiring_nonce_hash: [6; 32],
            },
            deposit,
            settled_on_chain: 0,
            highest_cumulative: cumulative,
            highest_signature: sig(cumulative as u8),
        }
    }

    async fn open(repo: &InMemoryChannelRepository, n: u8, cumulative: u128) -> ChannelRecord {
        repo.open_or_refresh(new_channel(n, 1_000_000, cumulative))
            .await
            .unwrap()
    }

    async fn reserved(
        repo: &InMemoryChannelRepository,
        n: u8,
        amount: u128,
    ) -> (Uuid, ChannelRecord) {
        let request = Uuid::new_v4();
        match repo.reserve(&id(n), request, amount).await.unwrap() {
            ReserveOutcome::Reserved(record) => (request, *record),
            other => panic!("expected a reservation, got {other:?}"),
        }
    }

    fn charge_ok(outcome: ChargeOutcome) -> (ChannelRecord, u128) {
        match outcome {
            ChargeOutcome::Charged { record, charged } => (*record, charged),
            ChargeOutcome::NoReservation => panic!("expected a charge"),
        }
    }

    fn pending() -> PendingClose {
        PendingClose {
            tx_hash: [9; 32],
            cumulative: 700,
            capture: 650,
            valid_before: 1_900_000_000,
        }
    }

    fn assert_invariants(record: &ChannelRecord) {
        assert!(record.reserved <= record.spent, "reserved <= spent");
        assert!(
            record.spent <= record.highest_cumulative,
            "spent <= highest"
        );
        assert!(record
            .retained_vouchers
            .iter()
            .all(|voucher| voucher.cumulative < record.highest_cumulative));
        assert!(record.retained_vouchers.len() <= MAX_RETAINED_VOUCHERS);
    }

    // ---- open_or_refresh -------------------------------------------------------------

    #[tokio::test]
    async fn open_inserts_an_open_channel_with_nothing_spent() {
        let repo = InMemoryChannelRepository::new();
        let record = open(&repo, 1, 100).await;
        assert_eq!(record.state, ChannelLifecycle::Open);
        assert_eq!(
            (record.spent, record.reserved, record.units, record.deposit),
            (0, 0, 0, 1_000_000)
        );
        assert_eq!(record.highest_cumulative, 100);
        assert!(record.retained_vouchers.is_empty());
        assert_eq!(record.close_requested_at, 0);
        assert!(record.pending_close.is_none());
        assert!(record.last_chain_read_at.is_none());
        assert_eq!(repo.get(&id(1)).await.unwrap(), Some(record));
        assert_eq!(repo.get(&id(2)).await.unwrap(), None);
    }

    #[tokio::test]
    async fn refresh_takes_the_greater_amounts_and_leaves_state_alone() {
        let repo = InMemoryChannelRepository::new();
        open(&repo, 1, 100).await;
        let (request, _) = reserved(&repo, 1, 30).await;
        repo.mark_closing(&id(1), pending()).await.unwrap();

        // Lower deposit, higher settled, higher voucher.
        let mut refresh = new_channel(1, 500, 250);
        refresh.settled_on_chain = 40;
        let record = repo.open_or_refresh(refresh).await.unwrap();
        assert_eq!(record.deposit, 1_000_000, "deposit never decreases");
        assert_eq!(record.settled_on_chain, 40);
        assert_eq!(record.highest_cumulative, 250);
        assert_eq!(record.highest_signature, sig(250));
        assert_eq!(
            record.retained_vouchers,
            vec![RetainedVoucher {
                cumulative: 100,
                signature: sig(100)
            }]
        );
        // State, spend and reservations are untouched.
        assert_eq!(record.state, ChannelLifecycle::Closing);
        assert_eq!((record.spent, record.reserved), (30, 30));
        assert_eq!(repo.reservation_count(), 1);
        assert!(repo.release(request).await.unwrap());
    }

    #[tokio::test]
    async fn refresh_with_a_lower_or_equal_voucher_keeps_the_stored_highest() {
        let repo = InMemoryChannelRepository::new();
        open(&repo, 1, 100).await;
        for lower in [100, 50] {
            let record = repo
                .open_or_refresh(new_channel(1, 2_000_000, lower))
                .await
                .unwrap();
            assert_eq!(record.highest_cumulative, 100);
            assert_eq!(record.highest_signature, sig(100));
            assert!(record.retained_vouchers.is_empty());
            assert_eq!(record.deposit, 2_000_000, "a larger deposit is taken");
        }
    }

    // ---- accept_voucher --------------------------------------------------------------

    #[tokio::test]
    async fn accept_voucher_outcomes() {
        let repo = InMemoryChannelRepository::new();
        assert_eq!(
            repo.accept_voucher(&id(1), 10, sig(1)).await.unwrap(),
            VoucherOutcome::Missing
        );
        open(&repo, 1, 100).await;
        // Not higher: equal and lower change nothing.
        for cumulative in [100, 99, 0] {
            assert_eq!(
                repo.accept_voucher(&id(1), cumulative, sig(9))
                    .await
                    .unwrap(),
                VoucherOutcome::NotHigher
            );
        }
        let stored = repo.get(&id(1)).await.unwrap().unwrap();
        assert_eq!(stored.highest_signature, sig(100));
        assert!(stored.retained_vouchers.is_empty());

        let VoucherOutcome::Accepted(record) =
            repo.accept_voucher(&id(1), 101, sig(7)).await.unwrap()
        else {
            panic!("expected Accepted");
        };
        assert_eq!(record.highest_cumulative, 101);
        assert_eq!(record.highest_signature, sig(7));
        assert_eq!(record.retained_vouchers.len(), 1);
        assert_eq!(record.retained_vouchers[0].cumulative, 100);
        assert_eq!(repo.get(&id(1)).await.unwrap().unwrap(), *record);
    }

    #[tokio::test]
    async fn accept_voucher_refuses_a_channel_that_is_not_accepting() {
        let repo = InMemoryChannelRepository::new();
        // Closing.
        open(&repo, 1, 100).await;
        repo.mark_closing(&id(1), pending()).await.unwrap();
        assert_eq!(
            repo.accept_voucher(&id(1), 200, sig(2)).await.unwrap(),
            VoucherOutcome::NotAccepting
        );
        // Finalized.
        open(&repo, 2, 100).await;
        repo.finalize(&id(2), 0).await.unwrap();
        assert_eq!(
            repo.accept_voucher(&id(2), 200, sig(2)).await.unwrap(),
            VoucherOutcome::NotAccepting
        );
        // Open, but the payer requested a close on chain.
        open(&repo, 3, 100).await;
        repo.record_chain_state(
            &id(3),
            ChannelState {
                settled: 0,
                deposit: 1_000_000,
                close_requested_at: 1_800_000_000,
            },
            Utc::now(),
        )
        .await
        .unwrap();
        assert_eq!(
            repo.accept_voucher(&id(3), 200, sig(2)).await.unwrap(),
            VoucherOutcome::NotAccepting
        );
        // NotAccepting wins over NotHigher: the state is checked first.
        assert_eq!(
            repo.accept_voucher(&id(3), 1, sig(2)).await.unwrap(),
            VoucherOutcome::NotAccepting
        );
        for n in 1..=3 {
            assert_eq!(
                repo.get(&id(n)).await.unwrap().unwrap().highest_cumulative,
                100
            );
        }
    }

    #[tokio::test]
    async fn retained_vouchers_are_capped_and_ordered_highest_first() {
        let repo = InMemoryChannelRepository::new();
        open(&repo, 1, 1).await;
        let top = MAX_RETAINED_VOUCHERS as u128 + 6;
        for cumulative in 2..=top {
            let outcome = repo
                .accept_voucher(&id(1), cumulative, sig(cumulative as u8))
                .await
                .unwrap();
            let VoucherOutcome::Accepted(record) = outcome else {
                panic!("expected Accepted");
            };
            assert_invariants(&record);
            assert!(record
                .retained_vouchers
                .windows(2)
                .all(|pair| pair[0].cumulative > pair[1].cumulative));
        }
        let record = repo.get(&id(1)).await.unwrap().unwrap();
        assert_eq!(record.highest_cumulative, top);
        assert_eq!(record.retained_vouchers.len(), MAX_RETAINED_VOUCHERS);
        // The newest earlier vouchers survive; the lowest were dropped.
        assert_eq!(record.retained_vouchers[0].cumulative, top - 1);
        assert_eq!(
            record.retained_vouchers[MAX_RETAINED_VOUCHERS - 1].cumulative,
            top - MAX_RETAINED_VOUCHERS as u128
        );
        // Each retained voucher keeps its own signature.
        assert!(record
            .retained_vouchers
            .iter()
            .all(|voucher| voucher.signature == sig(voucher.cumulative as u8)));
    }

    // ---- reserve / charge / release --------------------------------------------------

    #[tokio::test]
    async fn reserve_holds_the_amount_and_stores_the_reservation() {
        let repo = InMemoryChannelRepository::new();
        open(&repo, 1, 100).await;
        let (_, record) = reserved(&repo, 1, 40).await;
        assert_eq!((record.spent, record.reserved), (40, 40));
        assert_eq!(record.available(), 60);
        assert_eq!(record.delivered(), 0);
        assert_eq!(repo.reservation_count(), 1);
        assert_invariants(&record);
    }

    #[tokio::test]
    async fn the_second_reservation_gets_insufficient_with_the_exact_available() {
        let repo = InMemoryChannelRepository::new();
        open(&repo, 1, 100).await;
        reserved(&repo, 1, 70).await;
        let second = Uuid::new_v4();
        assert_eq!(
            repo.reserve(&id(1), second, 31).await.unwrap(),
            ReserveOutcome::Insufficient { available: 30 }
        );
        // Nothing was held for the refused request.
        assert_eq!(repo.reservation_count(), 1);
        let record = repo.get(&id(1)).await.unwrap().unwrap();
        assert_eq!((record.spent, record.reserved), (70, 70));
        // Exactly the remainder is still reservable.
        assert!(matches!(
            repo.reserve(&id(1), second, 30).await.unwrap(),
            ReserveOutcome::Reserved(_)
        ));
        assert_eq!(
            repo.reserve(&id(1), Uuid::new_v4(), 1).await.unwrap(),
            ReserveOutcome::Insufficient { available: 0 }
        );
    }

    #[tokio::test]
    async fn reserve_refuses_missing_and_non_accepting_channels() {
        let repo = InMemoryChannelRepository::new();
        assert_eq!(
            repo.reserve(&id(1), Uuid::new_v4(), 1).await.unwrap(),
            ReserveOutcome::Missing
        );
        open(&repo, 1, 100).await;
        repo.mark_closing(&id(1), pending()).await.unwrap();
        assert_eq!(
            repo.reserve(&id(1), Uuid::new_v4(), 1).await.unwrap(),
            ReserveOutcome::NotAccepting
        );
        open(&repo, 2, 100).await;
        repo.finalize(&id(2), 0).await.unwrap();
        assert_eq!(
            repo.reserve(&id(2), Uuid::new_v4(), 1).await.unwrap(),
            ReserveOutcome::NotAccepting
        );
        open(&repo, 3, 100).await;
        let chain = ChannelState {
            settled: 0,
            deposit: 1_000_000,
            close_requested_at: 5,
        };
        repo.record_chain_state(&id(3), chain, Utc::now())
            .await
            .unwrap();
        assert_eq!(
            repo.reserve(&id(3), Uuid::new_v4(), 1).await.unwrap(),
            ReserveOutcome::NotAccepting
        );
        assert_eq!(repo.reservation_count(), 0);
    }

    #[tokio::test]
    async fn reserve_rejects_zero_and_a_duplicate_request_id() {
        let repo = InMemoryChannelRepository::new();
        open(&repo, 1, 100).await;
        assert!(matches!(
            repo.reserve(&id(1), Uuid::new_v4(), 0).await,
            Err(RepositoryError::ValidationFailed(_))
        ));
        let (request, _) = reserved(&repo, 1, 10).await;
        assert!(matches!(
            repo.reserve(&id(1), request, 10).await,
            Err(RepositoryError::AlreadyExists)
        ));
        let record = repo.get(&id(1)).await.unwrap().unwrap();
        assert_eq!((record.spent, record.reserved), (10, 10));
        assert_eq!(repo.reservation_count(), 1);
    }

    #[tokio::test]
    async fn charge_turns_the_reservation_into_the_actual_cost() {
        let repo = InMemoryChannelRepository::new();
        open(&repo, 1, 100).await;
        let (request, _) = reserved(&repo, 1, 40).await;
        let (record, charged) = charge_ok(repo.charge(request, 25, 7).await.unwrap());
        assert_eq!(charged, 25);
        assert_eq!((record.spent, record.reserved, record.units), (25, 0, 7));
        assert_eq!(record.delivered(), 25);
        assert_eq!(record.available(), 75);
        assert_eq!(repo.reservation_count(), 0);
        assert_invariants(&record);
        // A second charge of the same request finds nothing.
        assert_eq!(
            repo.charge(request, 25, 7).await.unwrap(),
            ChargeOutcome::NoReservation
        );
        assert_eq!(repo.get(&id(1)).await.unwrap().unwrap(), record);
    }

    #[tokio::test]
    async fn charge_can_cost_more_than_the_reservation_up_to_the_voucher() {
        let repo = InMemoryChannelRepository::new();
        open(&repo, 1, 100).await;
        let (request, _) = reserved(&repo, 1, 40).await;
        let (record, charged) = charge_ok(repo.charge(request, 55, 1).await.unwrap());
        assert_eq!(charged, 55);
        assert_eq!((record.spent, record.reserved), (55, 0));
    }

    #[tokio::test]
    async fn charge_is_capped_at_what_the_voucher_still_covers() {
        let repo = InMemoryChannelRepository::new();
        open(&repo, 1, 100).await;
        let (first, _) = reserved(&repo, 1, 30).await;
        let (second, _) = reserved(&repo, 1, 50).await;
        // Voucher 100, spent 80. Charging the first at 500 may take at most
        // 100 - (80 - 30) = 50.
        let (record, charged) = charge_ok(repo.charge(first, 500, 3).await.unwrap());
        assert_eq!(charged, 50);
        assert_eq!((record.spent, record.reserved), (100, 50));
        assert_eq!(record.available(), 0);
        assert_invariants(&record);
        // The other reservation is still whole and charges within the same cap: 100 -
        // (100 - 50) = 50.
        let (record, charged) = charge_ok(repo.charge(second, 50, 2).await.unwrap());
        assert_eq!(charged, 50);
        assert_eq!((record.spent, record.reserved, record.units), (100, 0, 5));
        assert_invariants(&record);
    }

    #[tokio::test]
    async fn charge_units_saturate_and_a_zero_charge_is_allowed() {
        let repo = InMemoryChannelRepository::new();
        open(&repo, 1, 100).await;
        let (first, _) = reserved(&repo, 1, 10).await;
        let (second, _) = reserved(&repo, 1, 10).await;
        let (record, charged_now) = charge_ok(repo.charge(first, 0, u64::MAX).await.unwrap());
        assert_eq!(charged_now, 0);
        assert_eq!((record.spent, record.reserved), (10, 10));
        let (record, _) = charge_ok(repo.charge(second, 1, 5).await.unwrap());
        assert_eq!(record.units, u64::MAX);
        assert_eq!((record.spent, record.reserved), (1, 0));
    }

    #[tokio::test]
    async fn charge_still_lands_on_a_closing_channel() {
        // A request reserved before the close was started delivers and is paid.
        let repo = InMemoryChannelRepository::new();
        open(&repo, 1, 100).await;
        let (request, _) = reserved(&repo, 1, 40).await;
        repo.mark_closing(&id(1), pending()).await.unwrap();
        let (record, charged_now) = charge_ok(repo.charge(request, 40, 4).await.unwrap());
        assert_eq!(charged_now, 40);
        assert_eq!(record.state, ChannelLifecycle::Closing);
        assert_eq!(record.delivered(), 40);
    }

    #[tokio::test]
    async fn release_restores_the_balance_exactly() {
        let repo = InMemoryChannelRepository::new();
        let before = open(&repo, 1, 100).await;
        let (kept, _) = reserved(&repo, 1, 20).await;
        let after_first = repo.get(&id(1)).await.unwrap().unwrap();
        let (dropped, _) = reserved(&repo, 1, 35).await;
        assert!(repo.release(dropped).await.unwrap());
        let record = repo.get(&id(1)).await.unwrap().unwrap();
        assert_eq!(
            (record.spent, record.reserved, record.highest_cumulative),
            (after_first.spent, after_first.reserved, 100)
        );
        assert_eq!(repo.reservation_count(), 1);
        assert!(repo.release(kept).await.unwrap());
        let record = repo.get(&id(1)).await.unwrap().unwrap();
        assert_eq!(
            (record.spent, record.reserved, record.units),
            (before.spent, before.reserved, before.units)
        );
        assert_eq!(record.available(), 100);
        assert_eq!(repo.reservation_count(), 0);
    }

    #[tokio::test]
    async fn release_of_an_unknown_or_used_request_is_false() {
        let repo = InMemoryChannelRepository::new();
        open(&repo, 1, 100).await;
        assert!(!repo.release(Uuid::new_v4()).await.unwrap());
        let (request, _) = reserved(&repo, 1, 10).await;
        charge_ok(repo.charge(request, 10, 1).await.unwrap());
        assert!(!repo.release(request).await.unwrap());
        let record = repo.get(&id(1)).await.unwrap().unwrap();
        assert_eq!((record.spent, record.reserved), (10, 0));
        // A released request id can reserve again.
        let (again, _) = reserved(&repo, 1, 5).await;
        assert!(repo.release(again).await.unwrap());
        assert!(matches!(
            repo.reserve(&id(1), again, 5).await.unwrap(),
            ReserveOutcome::Reserved(_)
        ));
    }

    // ---- chain state -----------------------------------------------------------------

    #[tokio::test]
    async fn chain_state_takes_the_greater_values_and_records_the_read() {
        let repo = InMemoryChannelRepository::new();
        assert!(repo
            .record_chain_state(
                &id(1),
                ChannelState {
                    settled: 1,
                    deposit: 1,
                    close_requested_at: 0
                },
                Utc::now()
            )
            .await
            .unwrap()
            .is_none());
        open(&repo, 1, 100).await;
        let read_at = Utc::now();
        let record = repo
            .record_chain_state(
                &id(1),
                ChannelState {
                    settled: 60,
                    deposit: 3_000_000,
                    close_requested_at: 0,
                },
                read_at,
            )
            .await
            .unwrap()
            .unwrap();
        assert_eq!(record.deposit, 3_000_000);
        assert_eq!(record.settled_on_chain, 60);
        assert_eq!(record.close_requested_at, 0);
        assert_eq!(record.last_chain_read_at, Some(read_at));

        // A lower read lowers nothing.
        let later = read_at + Duration::seconds(5);
        let record = repo
            .record_chain_state(
                &id(1),
                ChannelState {
                    settled: 10,
                    deposit: 2_000_000,
                    close_requested_at: 0,
                },
                later,
            )
            .await
            .unwrap()
            .unwrap();
        assert_eq!((record.deposit, record.settled_on_chain), (3_000_000, 60));
        assert_eq!(record.last_chain_read_at, Some(later));
    }

    #[tokio::test]
    async fn chain_state_with_a_zero_deposit_leaves_the_deposit() {
        let repo = InMemoryChannelRepository::new();
        open(&repo, 1, 100).await;
        let record = repo
            .record_chain_state(
                &id(1),
                ChannelState {
                    settled: 90,
                    deposit: 0,
                    close_requested_at: 0,
                },
                Utc::now(),
            )
            .await
            .unwrap()
            .unwrap();
        assert_eq!(record.deposit, 1_000_000);
        assert_eq!(record.settled_on_chain, 90);
    }

    #[tokio::test]
    async fn a_close_request_is_stored_and_a_zero_never_erases_it() {
        let repo = InMemoryChannelRepository::new();
        open(&repo, 1, 100).await;
        let chain = |close_requested_at| ChannelState {
            settled: 0,
            deposit: 1_000_000,
            close_requested_at,
        };
        let record = repo
            .record_chain_state(&id(1), chain(1_800_000_000), Utc::now())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(record.close_requested_at, 1_800_000_000);
        assert!(!record.accepts_vouchers());
        let record = repo
            .record_chain_state(&id(1), chain(0), Utc::now())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(record.close_requested_at, 1_800_000_000);
    }

    // ---- closing / finalize ----------------------------------------------------------

    #[tokio::test]
    async fn closing_finalize_and_clear_transitions() {
        let repo = InMemoryChannelRepository::new();
        assert_eq!(
            repo.mark_closing(&id(1), pending()).await.unwrap(),
            CloseTransition::Missing
        );
        open(&repo, 1, 100).await;
        let CloseTransition::Marked(record) = repo.mark_closing(&id(1), pending()).await.unwrap()
        else {
            panic!("expected Marked");
        };
        assert_eq!(record.state, ChannelLifecycle::Closing);
        assert_eq!(record.pending_close, Some(pending()));
        // Only an open channel can start closing.
        assert_eq!(
            repo.mark_closing(&id(1), pending()).await.unwrap(),
            CloseTransition::NotOpen
        );

        // A close that did not land goes back to open.
        repo.clear_closing(&id(1)).await.unwrap();
        let record = repo.get(&id(1)).await.unwrap().unwrap();
        assert_eq!(record.state, ChannelLifecycle::Open);
        assert!(record.pending_close.is_none());
        assert!(record.accepts_vouchers());

        // Clearing an open channel changes nothing.
        let before = repo.get(&id(1)).await.unwrap().unwrap();
        repo.clear_closing(&id(1)).await.unwrap();
        assert_eq!(repo.get(&id(1)).await.unwrap().unwrap(), before);

        // Finalize from closing, with the greater settled amount, clears the pending close.
        repo.mark_closing(&id(1), pending()).await.unwrap();
        repo.finalize(&id(1), 650).await.unwrap();
        let record = repo.get(&id(1)).await.unwrap().unwrap();
        assert_eq!(record.state, ChannelLifecycle::Finalized);
        assert_eq!(record.settled_on_chain, 650);
        assert!(record.pending_close.is_none());
        // Terminal: clear_closing and mark_closing leave it.
        repo.clear_closing(&id(1)).await.unwrap();
        assert_eq!(
            repo.mark_closing(&id(1), pending()).await.unwrap(),
            CloseTransition::NotOpen
        );
        // A lower settled amount does not lower it.
        repo.finalize(&id(1), 10).await.unwrap();
        assert_eq!(
            repo.get(&id(1)).await.unwrap().unwrap().settled_on_chain,
            650
        );
    }

    #[tokio::test]
    async fn any_state_may_finalize_and_an_unknown_channel_is_a_no_op() {
        let repo = InMemoryChannelRepository::new();
        repo.finalize(&id(9), 5).await.unwrap();
        assert_eq!(repo.channel_count(), 0);
        open(&repo, 1, 100).await;
        repo.finalize(&id(1), 0).await.unwrap();
        assert_eq!(
            repo.get(&id(1)).await.unwrap().unwrap().state,
            ChannelLifecycle::Finalized
        );
    }

    // ---- settlement ------------------------------------------------------------------

    #[tokio::test]
    async fn the_settlement_lease_is_exclusive_until_it_lapses() {
        let repo = InMemoryChannelRepository::new();
        let now = Utc::now();
        assert!(repo
            .claim_settlement_lease(&id(1), now, now + Duration::seconds(30))
            .await
            .unwrap()
            .is_none());
        open(&repo, 1, 100).await;
        let until = now + Duration::seconds(30);
        assert!(repo
            .claim_settlement_lease(&id(1), now, until)
            .await
            .unwrap()
            .is_some());
        assert_eq!(repo.lease_until(&id(1)), Some(until));
        // Held: refused while now is before, and exactly at, the expiry.
        for at in [now, now + Duration::seconds(29), until] {
            assert!(repo
                .claim_settlement_lease(&id(1), at, at + Duration::seconds(30))
                .await
                .unwrap()
                .is_none());
        }
        assert_eq!(repo.lease_until(&id(1)), Some(until));
        // Past the expiry another instance takes over.
        let later = until + Duration::seconds(1);
        assert!(repo
            .claim_settlement_lease(&id(1), later, later + Duration::seconds(30))
            .await
            .unwrap()
            .is_some());
    }

    #[tokio::test]
    async fn releasing_the_lease_frees_it() {
        let repo = InMemoryChannelRepository::new();
        let now = Utc::now();
        open(&repo, 1, 100).await;
        let until = now + Duration::seconds(30);
        repo.claim_settlement_lease(&id(1), now, until)
            .await
            .unwrap();
        repo.release_settlement_lease(&id(1)).await.unwrap();
        assert_eq!(repo.lease_until(&id(1)), None);
        assert!(repo
            .claim_settlement_lease(&id(1), now, until)
            .await
            .unwrap()
            .is_some());
        // Unknown channel: nothing to release.
        repo.release_settlement_lease(&id(9)).await.unwrap();
    }

    #[tokio::test]
    async fn record_settlement_drops_covered_vouchers_and_the_lease() {
        let repo = InMemoryChannelRepository::new();
        open(&repo, 1, 100).await;
        for cumulative in [200, 300, 400] {
            repo.accept_voucher(&id(1), cumulative, sig(cumulative as u8))
                .await
                .unwrap();
        }
        let (request, _) = reserved(&repo, 1, 150).await;
        charge_ok(repo.charge(request, 150, 15).await.unwrap());
        let now = Utc::now();
        repo.claim_settlement_lease(&id(1), now, now + Duration::seconds(30))
            .await
            .unwrap();
        let at = now + Duration::seconds(1);
        repo.record_settlement(&id(1), 200, at).await.unwrap();

        let record = repo.get(&id(1)).await.unwrap().unwrap();
        assert_eq!(record.settled_on_chain, 200);
        assert_eq!(record.last_settlement_at, Some(at));
        assert_eq!(record.last_settlement_spent, 150);
        assert_eq!(
            record
                .retained_vouchers
                .iter()
                .map(|voucher| voucher.cumulative)
                .collect::<Vec<_>>(),
            vec![300],
            "the vouchers at or below the settled amount are gone"
        );
        assert_eq!(repo.lease_until(&id(1)), None);

        // A lower amount lowers nothing, but still stamps the settlement.
        repo.record_settlement(&id(1), 50, at + Duration::seconds(1))
            .await
            .unwrap();
        let record = repo.get(&id(1)).await.unwrap().unwrap();
        assert_eq!(record.settled_on_chain, 200);
        assert_eq!(record.last_settlement_at, Some(at + Duration::seconds(1)));
        repo.record_settlement(&id(9), 1, at).await.unwrap();
    }

    #[tokio::test]
    async fn settleable_voucher_never_exceeds_what_was_delivered() {
        let repo = InMemoryChannelRepository::new();
        open(&repo, 1, 100).await;
        for cumulative in [250, 400, 900] {
            repo.accept_voucher(&id(1), cumulative, sig(cumulative as u8))
                .await
                .unwrap();
        }
        // Nothing delivered yet: nothing settleable, whatever the vouchers say.
        let record = repo.get(&id(1)).await.unwrap().unwrap();
        assert_eq!(record.settleable_voucher(), None);

        let (request, record) = reserved(&repo, 1, 500).await;
        assert_eq!(
            record.settleable_voucher(),
            None,
            "reserved is not delivered"
        );
        let (record, _) = charge_ok(repo.charge(request, 300, 30).await.unwrap());
        assert_eq!(record.delivered(), 300);
        let voucher = record.settleable_voucher().expect("a voucher below 300");
        assert_eq!(voucher.cumulative, 250);
        assert!(voucher.cumulative <= record.delivered());

        // In-flight reservations keep the highest voucher out of reach.
        let (_inflight, record) = reserved(&repo, 1, 200).await;
        assert_eq!(record.delivered(), 300);
        assert!(record.settleable_voucher().unwrap().cumulative <= record.delivered());
        assert_invariants(&record);

        // After settling the 250 voucher nothing further fits under 300.
        repo.record_settlement(&id(1), 250, Utc::now())
            .await
            .unwrap();
        let record = repo.get(&id(1)).await.unwrap().unwrap();
        assert_eq!(record.settleable_voucher(), None);
    }

    // ---- list_active -----------------------------------------------------------------

    #[tokio::test]
    async fn list_active_pages_by_channel_id_and_skips_finalized() {
        let repo = InMemoryChannelRepository::new();
        // Inserted out of order.
        for n in [5, 2, 4, 1, 3, 6] {
            open(&repo, n, 100).await;
        }
        repo.finalize(&id(4), 0).await.unwrap();
        repo.mark_closing(&id(5), pending()).await.unwrap();

        let page = repo.list_active(None, 2).await.unwrap();
        assert_eq!(
            page.iter().map(|r| r.channel_id).collect::<Vec<_>>(),
            vec![id(1), id(2)]
        );
        let page = repo.list_active(Some(id(2)), 2).await.unwrap();
        assert_eq!(
            page.iter().map(|r| r.channel_id).collect::<Vec<_>>(),
            vec![id(3), id(5)],
            "finalized 4 is skipped; closing 5 is still active"
        );
        let page = repo.list_active(Some(id(5)), 10).await.unwrap();
        assert_eq!(
            page.iter().map(|r| r.channel_id).collect::<Vec<_>>(),
            vec![id(6)]
        );
        assert!(repo.list_active(Some(id(6)), 10).await.unwrap().is_empty());
        assert!(repo.list_active(None, 0).await.unwrap().is_empty());
        // `after` is exclusive and need not exist.
        let mut between = id(2);
        between[31] = 0xff;
        assert_eq!(
            repo.list_active(Some(between), 1).await.unwrap()[0].channel_id,
            id(3)
        );
    }

    // ---- stale reservations, purge ---------------------------------------------------

    #[tokio::test]
    async fn stale_reservations_are_released_like_release() {
        let repo = InMemoryChannelRepository::new();
        open(&repo, 1, 100).await;
        open(&repo, 2, 100).await;
        let (stale_a, _) = reserved(&repo, 1, 30).await;
        let (stale_b, _) = reserved(&repo, 1, 20).await;
        let (fresh, _) = reserved(&repo, 1, 10).await;
        let (stale_c, _) = reserved(&repo, 2, 40).await;
        for request in [stale_a, stale_b, stale_c] {
            repo.age_reservation(request, Duration::minutes(30));
        }
        let cutoff = repo.now() - Duration::minutes(10);
        assert_eq!(repo.release_stale_reservations(cutoff).await.unwrap(), 3);

        let one = repo.get(&id(1)).await.unwrap().unwrap();
        assert_eq!((one.spent, one.reserved), (10, 10));
        let two = repo.get(&id(2)).await.unwrap().unwrap();
        assert_eq!((two.spent, two.reserved), (0, 0));
        assert_eq!(repo.reservation_count(), 1);
        // The fresh one still charges; the reaped ones do not.
        assert_eq!(
            repo.charge(stale_a, 30, 1).await.unwrap(),
            ChargeOutcome::NoReservation
        );
        assert!(!repo.release(stale_b).await.unwrap());
        charge_ok(repo.charge(fresh, 10, 1).await.unwrap());
        assert_eq!(repo.release_stale_reservations(cutoff).await.unwrap(), 0);
    }

    #[tokio::test]
    async fn purge_removes_old_finalized_channels_and_their_reservations() {
        let repo = InMemoryChannelRepository::new();
        for n in 1..=4 {
            open(&repo, n, 100).await;
        }
        let (orphaned, _) = reserved(&repo, 1, 10).await;
        let (survivor, _) = reserved(&repo, 4, 10).await;
        repo.finalize(&id(1), 0).await.unwrap();
        repo.finalize(&id(2), 0).await.unwrap();
        repo.finalize(&id(3), 0).await.unwrap();
        // 1 and 2 finalized long ago; 3 just now; 4 is old but still open.
        repo.age_channel(&id(1), Duration::days(10));
        repo.age_channel(&id(2), Duration::days(10));
        repo.age_channel(&id(4), Duration::days(10));
        let cutoff = repo.now() - Duration::days(7);

        assert_eq!(repo.purge_finalized(cutoff).await.unwrap(), 2);
        assert!(repo.get(&id(1)).await.unwrap().is_none());
        assert!(repo.get(&id(2)).await.unwrap().is_none());
        assert!(repo.get(&id(3)).await.unwrap().is_some());
        assert!(repo.get(&id(4)).await.unwrap().is_some(), "open is kept");
        assert_eq!(
            repo.charge(orphaned, 1, 1).await.unwrap(),
            ChargeOutcome::NoReservation
        );
        assert_eq!(repo.reservation_count(), 1);
        assert!(repo.release(survivor).await.unwrap());
        assert_eq!(repo.purge_finalized(cutoff).await.unwrap(), 0);
    }

    // ---- clock, concurrency ----------------------------------------------------------

    #[tokio::test]
    async fn every_write_moves_updated_at_forward() {
        let repo = InMemoryChannelRepository::new();
        let mut last = open(&repo, 1, 100).await.updated_at;
        let mut check = |record: ChannelRecord| {
            assert!(record.updated_at > last);
            last = record.updated_at;
        };
        let (request, record) = reserved(&repo, 1, 10).await;
        check(record);
        let (record, _) = charge_ok(repo.charge(request, 10, 1).await.unwrap());
        check(record);
        let VoucherOutcome::Accepted(record) =
            repo.accept_voucher(&id(1), 200, sig(2)).await.unwrap()
        else {
            panic!("expected Accepted");
        };
        check(*record);
        let record = repo
            .record_chain_state(
                &id(1),
                ChannelState {
                    settled: 0,
                    deposit: 1,
                    close_requested_at: 0,
                },
                Utc::now(),
            )
            .await
            .unwrap()
            .unwrap();
        check(record);

        // set_now steers later stamps.
        let target = Utc::now() + Duration::days(1);
        repo.set_now(target);
        let record = repo.open_or_refresh(new_channel(1, 1, 1)).await.unwrap();
        assert!(record.updated_at > target);
    }

    #[tokio::test]
    async fn concurrent_reservations_never_overspend_the_voucher() {
        let repo = std::sync::Arc::new(InMemoryChannelRepository::new());
        open(&repo, 1, 100).await;
        let mut tasks = Vec::new();
        for _ in 0..16 {
            let repo = repo.clone();
            tasks.push(tokio::spawn(async move {
                repo.reserve(&id(1), Uuid::new_v4(), 30).await.unwrap()
            }));
        }
        let mut held = 0;
        for task in tasks {
            if matches!(task.await.unwrap(), ReserveOutcome::Reserved(_)) {
                held += 1;
            }
        }
        assert_eq!(held, 3, "100 covers three reservations of 30");
        let record = repo.get(&id(1)).await.unwrap().unwrap();
        assert_eq!((record.spent, record.reserved), (90, 90));
        assert_invariants(&record);
    }
}
