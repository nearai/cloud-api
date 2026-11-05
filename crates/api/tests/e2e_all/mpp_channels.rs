//! E2E tests of the Postgres channel ledger (`MppChannelRepository`) used by the MPP
//! `session` intent: the same money rules as the in-memory reference, against the real
//! tables (`mpp_channels`, `mpp_channel_reservations`, `V0100__mpp_channels.sql` in
//! nearai/cloud-platform), including the races that only a database can show.
//!
//! Every test uses channels with random ids, so tests share the database without touching
//! each other's rows.

const RULES: services::mpp::VoucherRules = services::mpp::VoucherRules {
    min_delta: 0,
    settled_floor_from_chain: true,
};

use crate::common::db_setup::create_test_pool;
use chrono::{DateTime, Duration, Utc};
use services::mpp::SettlementLease;

fn test_now() -> DateTime<Utc> {
    DateTime::from_timestamp_micros(Utc::now().timestamp_micros()).unwrap()
}
use database::repositories::MppChannelRepository;
use database::DbPool;
use services::common::RepositoryError;
use services::mpp::session::{ChannelDescriptor, ChannelState};
use services::mpp::{
    ChannelLifecycle, ChannelRecord, ChannelRepository, ChargeOutcome, CloseTransition, NewChannel,
    PendingClose, ReserveOutcome, VoucherOutcome, MAX_RETAINED_VOUCHERS,
};
use std::sync::Arc;
use uuid::Uuid;

const DEPOSIT: u128 = 1_000_000;

fn random_id() -> [u8; 32] {
    let mut id = [0u8; 32];
    id[..16].copy_from_slice(Uuid::new_v4().as_bytes());
    id[16..].copy_from_slice(Uuid::new_v4().as_bytes());
    id
}

fn sig(n: u8) -> [u8; 65] {
    [n; 65]
}

fn new_channel(id: [u8; 32], cumulative: u128) -> NewChannel {
    NewChannel {
        channel_id: id,
        chain_id: 42431,
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
        deposit: DEPOSIT,
        settled_on_chain: 0,
        highest_cumulative: cumulative,
        highest_signature: sig(cumulative as u8),
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

struct Fixture {
    pool: DbPool,
    repo: Arc<MppChannelRepository>,
}

async fn fixture() -> Fixture {
    let pool = create_test_pool().await;
    let repo = Arc::new(MppChannelRepository::new(pool.clone()));
    Fixture { pool, repo }
}

impl Fixture {
    async fn open(&self, cumulative: u128) -> ([u8; 32], ChannelRecord) {
        let id = random_id();
        let record = self
            .repo
            .open_or_refresh(new_channel(id, cumulative))
            .await
            .unwrap();
        (id, record)
    }

    async fn reserve(&self, id: &[u8; 32], amount: u128) -> (Uuid, ChannelRecord) {
        let request = Uuid::new_v4();
        match self.repo.reserve(id, request, amount).await.unwrap() {
            ReserveOutcome::Reserved(record) => (request, *record),
            other => panic!("expected a reservation, got {other:?}"),
        }
    }

    async fn stored(&self, id: &[u8; 32]) -> ChannelRecord {
        self.repo.get(id).await.unwrap().expect("channel row")
    }

    async fn reservations(&self, id: &[u8; 32]) -> i64 {
        let client = self.pool.get().await.unwrap();
        client
            .query_one(
                "SELECT COUNT(*) FROM mpp_channel_reservations WHERE channel_id = $1",
                &[&id.as_slice()],
            )
            .await
            .unwrap()
            .get(0)
    }

    async fn settle(
        &self,
        id: &[u8; 32],
        cumulative: u128,
        at: DateTime<Utc>,
    ) -> Result<bool, RepositoryError> {
        let client = self.pool.get().await.unwrap();
        let until = client
            .query_opt(
                "SELECT settlement_lease_until FROM mpp_channels WHERE channel_id = $1",
                &[&id.as_slice()],
            )
            .await
            .unwrap()
            .and_then(|row| row.get::<_, Option<DateTime<Utc>>>(0));
        let lease = match until {
            Some(until) => SettlementLease::new(until),
            None => {
                let now = test_now();
                let Some((_, lease)) = self
                    .repo
                    .claim_settlement_lease(id, now, now + Duration::seconds(30))
                    .await?
                else {
                    return Ok(false);
                };
                lease
            }
        };
        self.repo.record_settlement(id, lease, cumulative, at).await
    }

    /// Run one statement against the channel row, for setting up aged rows.
    async fn sql(&self, statement: &str, id: &[u8; 32]) {
        let client = self.pool.get().await.unwrap();
        client.execute(statement, &[&id.as_slice()]).await.unwrap();
    }
}

fn charged(outcome: ChargeOutcome) -> (ChannelRecord, u128) {
    match outcome {
        ChargeOutcome::Charged { record, charged } => (*record, charged),
        ChargeOutcome::NoReservation => panic!("expected a charge"),
    }
}

fn assert_invariants(record: &ChannelRecord) {
    assert!(record.reserved <= record.spent);
    assert!(record.spent <= record.highest_cumulative);
    assert!(record
        .retained_vouchers
        .iter()
        .all(|voucher| voucher.cumulative < record.highest_cumulative));
    assert!(record.retained_vouchers.len() <= MAX_RETAINED_VOUCHERS);
}

#[tokio::test]
async fn open_inserts_an_open_channel_and_refresh_merges_without_touching_state() {
    let f = fixture().await;
    let (id, record) = f.open(100).await;
    assert_eq!(record.state, ChannelLifecycle::Open);
    assert_eq!(
        (record.spent, record.reserved, record.units, record.deposit),
        (0, 0, 0, DEPOSIT)
    );
    assert_eq!(record.highest_cumulative, 100);
    assert_eq!(record.highest_signature, sig(100));
    assert_eq!(record.descriptor, new_channel(id, 100).descriptor);
    assert_eq!((record.chain_id, record.escrow), (42431, [0xee; 20]));
    assert_eq!(f.stored(&id).await, record);
    assert!(f.repo.get(&random_id()).await.unwrap().is_none());

    let (_request, _) = f.reserve(&id, 30).await;
    f.repo
        .mark_closing(&id, pending(), Utc::now())
        .await
        .unwrap();
    let mut refresh = new_channel(id, 250);
    refresh.deposit = 500;
    refresh.settled_on_chain = 40;
    let refreshed = f.repo.open_or_refresh(refresh).await.unwrap();
    assert_eq!(refreshed.deposit, DEPOSIT, "the deposit never decreases");
    assert_eq!(refreshed.settled_on_chain, 40);
    assert_eq!(refreshed.highest_cumulative, 100);
    assert!(refreshed.retained_vouchers.is_empty());
    assert_eq!(refreshed.state, ChannelLifecycle::Closing);
    assert_eq!((refreshed.spent, refreshed.reserved), (30, 30));
    assert_eq!(f.reservations(&id).await, 1);

    // A lower voucher changes nothing; a larger deposit is taken.
    let mut lower = new_channel(id, 50);
    lower.deposit = 2 * DEPOSIT;
    let again = f.repo.open_or_refresh(lower).await.unwrap();
    assert_eq!(
        (again.highest_cumulative, again.deposit),
        (100, 2 * DEPOSIT)
    );
    assert!(again.retained_vouchers.is_empty());
}

#[tokio::test]
async fn concurrent_opens_of_one_channel_leave_one_row() {
    let f = fixture().await;
    let id = random_id();
    let mut tasks = Vec::new();
    for n in 1..=8u8 {
        let repo = f.repo.clone();
        tasks.push(tokio::spawn(async move {
            repo.open_or_refresh(new_channel(id, u128::from(n) * 10))
                .await
                .unwrap();
            repo.accept_voucher(&id, u128::from(n) * 10, sig(n), RULES)
                .await
                .unwrap()
        }));
    }
    for task in tasks {
        task.await.unwrap();
    }
    let record = f.stored(&id).await;
    assert_eq!(record.highest_cumulative, 80, "the highest voucher won");
    assert_invariants(&record);
}

#[tokio::test]
async fn vouchers_are_accepted_only_above_the_highest_and_retained_in_order() {
    let f = fixture().await;
    assert_eq!(
        f.repo
            .accept_voucher(&random_id(), 5, sig(5), RULES)
            .await
            .unwrap(),
        VoucherOutcome::Missing
    );
    let (id, _) = f.open(1).await;
    for not_higher in [1, 0] {
        assert_eq!(
            f.repo
                .accept_voucher(&id, not_higher, sig(9), RULES)
                .await
                .unwrap(),
            VoucherOutcome::NotHigher
        );
    }
    let top = MAX_RETAINED_VOUCHERS as u128 + 6;
    for cumulative in 2..=top {
        let VoucherOutcome::Accepted(record) = f
            .repo
            .accept_voucher(
                &id,
                cumulative,
                sig(cumulative as u8),
                services::mpp::VoucherRules {
                    min_delta: 0,
                    settled_floor_from_chain: true,
                },
            )
            .await
            .unwrap()
        else {
            panic!("expected Accepted");
        };
        assert_invariants(&record);
    }
    let record = f.stored(&id).await;
    assert_eq!(record.highest_cumulative, top);
    assert_eq!(record.retained_vouchers.len(), MAX_RETAINED_VOUCHERS);
    assert_eq!(record.retained_vouchers[0].cumulative, top - 1);
    assert_eq!(
        record.retained_vouchers[MAX_RETAINED_VOUCHERS - 1].cumulative,
        top - MAX_RETAINED_VOUCHERS as u128
    );
    assert!(record
        .retained_vouchers
        .windows(2)
        .all(|pair| pair[0].cumulative > pair[1].cumulative));
    assert!(record
        .retained_vouchers
        .iter()
        .all(|voucher| voucher.signature == sig(voucher.cumulative as u8)));

    // The stored JSON has the documented shape.
    let client = f.pool.get().await.unwrap();
    let row = client
        .query_one(
            "SELECT jsonb_array_length(retained_vouchers), \
             retained_vouchers->0->>'cumulative', \
             length(retained_vouchers->0->>'signature') \
             FROM mpp_channels WHERE channel_id = $1",
            &[&id.as_slice()],
        )
        .await
        .unwrap();
    assert_eq!(row.get::<_, i32>(0), MAX_RETAINED_VOUCHERS as i32);
    assert_eq!(row.get::<_, String>(1), (top - 1).to_string());
    assert_eq!(row.get::<_, i32>(2), 2 + 2 * 65);
}

#[tokio::test]
async fn a_channel_that_is_not_accepting_refuses_vouchers_and_reservations() {
    let f = fixture().await;
    let (closing, _) = f.open(100).await;
    f.repo
        .mark_closing(&closing, pending(), Utc::now())
        .await
        .unwrap();
    let (finalized, _) = f.open(100).await;
    f.repo.finalize(&finalized, 0).await.unwrap();
    let (requested, _) = f.open(100).await;
    f.repo
        .record_chain_state(
            &requested,
            ChannelState {
                settled: 0,
                deposit: DEPOSIT,
                close_requested_at: 1_800_000_000,
            },
            Utc::now(),
        )
        .await
        .unwrap();
    for id in [closing, finalized, requested] {
        assert_eq!(
            f.repo
                .accept_voucher(&id, 200, sig(2), RULES)
                .await
                .unwrap(),
            VoucherOutcome::NotAccepting
        );
        assert_eq!(
            f.repo.reserve(&id, Uuid::new_v4(), 1).await.unwrap(),
            ReserveOutcome::NotAccepting
        );
        assert_eq!(f.stored(&id).await.highest_cumulative, 100);
        assert_eq!(f.reservations(&id).await, 0);
    }
    assert_eq!(
        f.repo
            .reserve(&random_id(), Uuid::new_v4(), 1)
            .await
            .unwrap(),
        ReserveOutcome::Missing
    );
}

#[tokio::test]
async fn reserve_charge_and_release_arithmetic() {
    let f = fixture().await;
    let (id, _) = f.open(100).await;

    // Reserve holds the amount; the second reservation sees the exact remainder.
    let (first, record) = f.reserve(&id, 70).await;
    assert_eq!((record.spent, record.reserved), (70, 70));
    let second = Uuid::new_v4();
    assert_eq!(
        f.repo.reserve(&id, second, 31).await.unwrap(),
        ReserveOutcome::Insufficient { available: 30 }
    );
    assert_eq!(f.reservations(&id).await, 1);
    assert!(matches!(
        f.repo.reserve(&id, second, 30).await.unwrap(),
        ReserveOutcome::Reserved(_)
    ));
    assert_eq!(
        f.repo.reserve(&id, Uuid::new_v4(), 1).await.unwrap(),
        ReserveOutcome::Insufficient { available: 0 }
    );

    // Release restores the balance exactly.
    assert!(f.repo.release(second).await.unwrap());
    assert!(!f.repo.release(second).await.unwrap());
    let record = f.stored(&id).await;
    assert_eq!((record.spent, record.reserved), (70, 70));

    // A charge capped at the voucher: 100 - (70 - 70) = 100 is the cap, so 500 pays 100.
    let (record, paid) = charged(f.repo.charge(first, 500, 3).await.unwrap());
    assert_eq!(paid, 100);
    assert_eq!((record.spent, record.reserved, record.units), (100, 0, 3));
    assert_eq!(record.delivered(), 100);
    assert_invariants(&record);
    assert_eq!(f.reservations(&id).await, 0);
    assert_eq!(
        f.repo.charge(first, 1, 1).await.unwrap(),
        ChargeOutcome::NoReservation
    );
    assert_eq!(f.stored(&id).await, record);
}

#[tokio::test]
async fn charge_is_capped_by_what_other_reservations_leave_of_the_voucher() {
    let f = fixture().await;
    let (id, _) = f.open(100).await;
    let (a, _) = f.reserve(&id, 30).await;
    let (b, _) = f.reserve(&id, 50).await;
    // spent 80: charging `a` may take at most 100 - (80 - 30) = 50.
    let (record, paid) = charged(f.repo.charge(a, 500, 3).await.unwrap());
    assert_eq!(paid, 50);
    assert_eq!((record.spent, record.reserved), (100, 50));
    let (record, paid) = charged(f.repo.charge(b, 50, 2).await.unwrap());
    assert_eq!(paid, 50);
    assert_eq!((record.spent, record.reserved, record.units), (100, 0, 5));
    assert_invariants(&record);

    // A charge below the reservation gives the difference back.
    let (id, _) = f.open(100).await;
    let (c, _) = f.reserve(&id, 40).await;
    let (record, paid) = charged(f.repo.charge(c, 25, 1).await.unwrap());
    assert_eq!((paid, record.spent, record.available()), (25, 25, 75));

    // A charge above the reservation takes the voucher's remainder.
    let (d, _) = f.reserve(&id, 10).await;
    let (record, paid) = charged(f.repo.charge(d, 60, 1).await.unwrap());
    assert_eq!((paid, record.spent), (60, 85));
}

#[tokio::test]
async fn reserve_rejects_zero_and_a_reused_request_id() {
    let f = fixture().await;
    let (id, _) = f.open(100).await;
    assert!(matches!(
        f.repo.reserve(&id, Uuid::new_v4(), 0).await,
        Err(RepositoryError::ValidationFailed(_))
    ));
    let (request, _) = f.reserve(&id, 10).await;
    assert!(matches!(
        f.repo.reserve(&id, request, 10).await,
        Err(RepositoryError::AlreadyExists)
    ));
    // The refused attempt rolled back whole: nothing extra was held.
    let record = f.stored(&id).await;
    assert_eq!((record.spent, record.reserved), (10, 10));
    assert_eq!(f.reservations(&id).await, 1);
    // A reservation is released only through its own channel's lock, never by another id.
    assert!(!f.repo.release(Uuid::new_v4()).await.unwrap());
}

#[tokio::test]
async fn two_concurrent_reserves_cannot_both_take_the_same_balance() {
    let f = fixture().await;
    for _ in 0..10 {
        let (id, _) = f.open(100).await;
        let (repo_a, repo_b) = (f.repo.clone(), f.repo.clone());
        let a = tokio::spawn(async move { repo_a.reserve(&id, Uuid::new_v4(), 60).await });
        let b = tokio::spawn(async move { repo_b.reserve(&id, Uuid::new_v4(), 60).await });
        let outcomes = [a.await.unwrap().unwrap(), b.await.unwrap().unwrap()];
        let reserved = outcomes
            .iter()
            .filter(|outcome| matches!(outcome, ReserveOutcome::Reserved(_)))
            .count();
        let insufficient = outcomes
            .iter()
            .filter(|outcome| matches!(outcome, ReserveOutcome::Insufficient { available: 40 }))
            .count();
        assert_eq!((reserved, insufficient), (1, 1), "{outcomes:?}");
        let record = f.stored(&id).await;
        assert_eq!((record.spent, record.reserved), (60, 60));
        assert_eq!(f.reservations(&id).await, 1);
    }
}

#[tokio::test]
async fn many_concurrent_reserves_never_overspend_the_voucher() {
    let f = fixture().await;
    let (id, _) = f.open(100).await;
    let mut tasks = Vec::new();
    for _ in 0..16 {
        let repo = f.repo.clone();
        tasks.push(tokio::spawn(async move {
            repo.reserve(&id, Uuid::new_v4(), 30).await.unwrap()
        }));
    }
    let mut held = 0;
    for task in tasks {
        if matches!(task.await.unwrap(), ReserveOutcome::Reserved(_)) {
            held += 1;
        }
    }
    assert_eq!(held, 3, "100 covers three reservations of 30");
    let record = f.stored(&id).await;
    assert_eq!((record.spent, record.reserved), (90, 90));
    assert_eq!(f.reservations(&id).await, 3);
    assert_invariants(&record);
}

#[tokio::test]
async fn a_reservation_is_settled_by_exactly_one_of_charge_and_release() {
    let f = fixture().await;
    for _ in 0..10 {
        let (id, _) = f.open(100).await;
        let (request, _) = f.reserve(&id, 40).await;
        let (repo_a, repo_b) = (f.repo.clone(), f.repo.clone());
        let charge = tokio::spawn(async move { repo_a.charge(request, 40, 1).await.unwrap() });
        let release = tokio::spawn(async move { repo_b.release(request).await.unwrap() });
        let charge = charge.await.unwrap();
        let released = release.await.unwrap();
        let record = f.stored(&id).await;
        match (charge, released) {
            (ChargeOutcome::Charged { .. }, false) => {
                assert_eq!((record.spent, record.reserved), (40, 0));
            }
            (ChargeOutcome::NoReservation, true) => {
                assert_eq!((record.spent, record.reserved), (0, 0));
            }
            other => panic!("both or neither settled the reservation: {other:?}"),
        }
        assert_eq!(f.reservations(&id).await, 0);
    }
}

#[tokio::test]
async fn settleable_voucher_never_exceeds_what_was_delivered() {
    let f = fixture().await;
    let (id, _) = f.open(100).await;
    for cumulative in [250, 400, 900] {
        f.repo
            .accept_voucher(
                &id,
                cumulative,
                sig(cumulative as u8),
                services::mpp::VoucherRules {
                    min_delta: 0,
                    settled_floor_from_chain: true,
                },
            )
            .await
            .unwrap();
    }
    assert_eq!(f.stored(&id).await.settleable_voucher(), None);
    let (request, record) = f.reserve(&id, 500).await;
    assert_eq!(record.settleable_voucher(), None);
    let (record, _) = charged(f.repo.charge(request, 300, 30).await.unwrap());
    let voucher = record.settleable_voucher().expect("a voucher below 300");
    assert_eq!(voucher.cumulative, 250);
    assert!(voucher.cumulative <= record.delivered());
}

#[tokio::test]
async fn chain_state_takes_the_greater_values_and_a_close_request_sticks() {
    let f = fixture().await;
    assert!(f
        .repo
        .record_chain_state(
            &random_id(),
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
    let (id, _) = f.open(100).await;
    let read_at = test_now();
    let record = f
        .repo
        .record_chain_state(
            &id,
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
    assert_eq!((record.deposit, record.settled_on_chain), (3_000_000, 60));
    assert_eq!(record.close_requested_at, 0);
    assert_eq!(record.last_chain_read_at, Some(read_at));

    // Lower values, a zero deposit and a zero close request lower or erase nothing.
    let record = f
        .repo
        .record_chain_state(
            &id,
            ChannelState {
                settled: 10,
                deposit: 0,
                close_requested_at: 1_800_000_000,
            },
            read_at,
        )
        .await
        .unwrap()
        .unwrap();
    assert_eq!((record.deposit, record.settled_on_chain), (3_000_000, 60));
    assert_eq!(record.close_requested_at, 1_800_000_000);
    assert!(!record.accepts_vouchers());
    let record = f
        .repo
        .record_chain_state(
            &id,
            ChannelState {
                settled: 10,
                deposit: 2_000_000,
                close_requested_at: 0,
            },
            read_at,
        )
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        (record.deposit, record.close_requested_at),
        (3_000_000, 1_800_000_000)
    );
}

#[tokio::test]
async fn closing_finalize_and_clear_transitions() {
    let f = fixture().await;
    assert_eq!(
        f.repo
            .mark_closing(&random_id(), pending(), Utc::now())
            .await
            .unwrap(),
        CloseTransition::Missing
    );
    let (id, _) = f.open(100).await;
    let CloseTransition::Marked(record) = f
        .repo
        .mark_closing(&id, pending(), Utc::now())
        .await
        .unwrap()
    else {
        panic!("expected Marked");
    };
    assert_eq!(record.state, ChannelLifecycle::Closing);
    assert_eq!(record.pending_close, Some(pending()));
    assert_eq!(f.stored(&id).await.pending_close, Some(pending()));
    assert_eq!(
        f.repo
            .mark_closing(&id, pending(), Utc::now())
            .await
            .unwrap(),
        CloseTransition::NotOpen
    );

    f.repo.clear_closing(&id, &pending().tx_hash).await.unwrap();
    let record = f.stored(&id).await;
    assert_eq!(record.state, ChannelLifecycle::Open);
    assert!(record.pending_close.is_none());
    // Clearing an open channel changes nothing.
    f.repo.clear_closing(&id, &pending().tx_hash).await.unwrap();
    assert_eq!(f.stored(&id).await, record);

    f.repo
        .mark_closing(&id, pending(), Utc::now())
        .await
        .unwrap();
    f.repo.finalize(&id, 650).await.unwrap();
    let record = f.stored(&id).await;
    assert_eq!(record.state, ChannelLifecycle::Finalized);
    assert_eq!(record.settled_on_chain, 650);
    assert!(record.pending_close.is_none());
    f.repo.clear_closing(&id, &pending().tx_hash).await.unwrap();
    assert_eq!(
        f.repo
            .mark_closing(&id, pending(), Utc::now())
            .await
            .unwrap(),
        CloseTransition::NotOpen
    );
    f.repo.finalize(&id, 10).await.unwrap();
    assert_eq!(f.stored(&id).await.settled_on_chain, 650);
    // An unknown channel is a no-op.
    f.repo.finalize(&random_id(), 1).await.unwrap();
}

#[tokio::test]
async fn the_settlement_lease_goes_to_exactly_one_of_two_contenders() {
    let f = fixture().await;
    for _ in 0..10 {
        let (id, _) = f.open(100).await;
        let now = test_now();
        let until = now + Duration::seconds(30);
        let (repo_a, repo_b) = (f.repo.clone(), f.repo.clone());
        let a = tokio::spawn(async move { repo_a.claim_settlement_lease(&id, now, until).await });
        let b = tokio::spawn(async move { repo_b.claim_settlement_lease(&id, now, until).await });
        let winners = [a.await.unwrap().unwrap(), b.await.unwrap().unwrap()]
            .into_iter()
            .filter(Option::is_some)
            .count();
        assert_eq!(winners, 1);
    }
}

#[tokio::test]
async fn many_contenders_for_one_lease_have_one_winner_and_it_lapses_and_releases() {
    let f = fixture().await;
    let (id, _) = f.open(100).await;
    let now = test_now();
    let until = now + Duration::seconds(30);
    let mut tasks = Vec::new();
    for _ in 0..16 {
        let repo = f.repo.clone();
        tasks.push(tokio::spawn(async move {
            repo.claim_settlement_lease(&id, now, until).await.unwrap()
        }));
    }
    let mut winners = 0;
    for task in tasks {
        if task.await.unwrap().is_some() {
            winners += 1;
        }
    }
    assert_eq!(winners, 1);

    // Held until the expiry, exactly at it included.
    for at in [now, until] {
        assert!(f
            .repo
            .claim_settlement_lease(&id, at, at + Duration::seconds(30))
            .await
            .unwrap()
            .is_none());
    }
    // Past the expiry another instance takes over; a release frees it at once.
    let later = until + Duration::seconds(1);
    let lease_end = later + Duration::seconds(30);
    assert!(f
        .repo
        .claim_settlement_lease(&id, later, lease_end)
        .await
        .unwrap()
        .is_some());
    f.repo
        .release_settlement_lease(&id, SettlementLease::new(lease_end))
        .await
        .unwrap();
    assert!(f
        .repo
        .claim_settlement_lease(&id, later, lease_end)
        .await
        .unwrap()
        .is_some());
    // An unknown channel has no lease to take.
    assert!(f
        .repo
        .claim_settlement_lease(&random_id(), now, until)
        .await
        .unwrap()
        .is_none());
    f.repo
        .release_settlement_lease(&random_id(), SettlementLease::new(lease_end))
        .await
        .unwrap();
}

#[tokio::test]
async fn record_settlement_drops_covered_vouchers_and_releases_the_lease() {
    let f = fixture().await;
    let (id, _) = f.open(100).await;
    for cumulative in [200, 300, 400] {
        f.repo
            .accept_voucher(
                &id,
                cumulative,
                sig(cumulative as u8),
                services::mpp::VoucherRules {
                    min_delta: 0,
                    settled_floor_from_chain: true,
                },
            )
            .await
            .unwrap();
    }
    let (request, _) = f.reserve(&id, 150).await;
    charged(f.repo.charge(request, 150, 15).await.unwrap());
    let now = test_now();
    f.repo
        .claim_settlement_lease(&id, now, now + Duration::seconds(30))
        .await
        .unwrap();
    let at = test_now();
    f.settle(&id, 200, at).await.unwrap();

    let record = f.stored(&id).await;
    assert_eq!(record.settled_on_chain, 200);
    assert_eq!(record.last_settlement_at, Some(at));
    assert_eq!(record.last_settlement_spent, 150);
    assert_eq!(
        record
            .retained_vouchers
            .iter()
            .map(|voucher| voucher.cumulative)
            .collect::<Vec<_>>(),
        vec![300]
    );
    // The lease was released: another instance can take it straight away.
    assert!(f
        .repo
        .claim_settlement_lease(&id, now, now + Duration::seconds(30))
        .await
        .unwrap()
        .is_some());

    // A lower amount lowers nothing.
    f.settle(&id, 50, at).await.unwrap();
    assert_eq!(f.stored(&id).await.settled_on_chain, 200);
    f.settle(&random_id(), 1, at).await.unwrap();
}

#[tokio::test]
async fn list_active_pages_by_channel_id_and_skips_finalized() {
    let f = fixture().await;
    // Ids that share a random prefix are contiguous in key order, so no other test's row
    // can fall between them.
    let mut prefix = [0u8; 32];
    prefix[..16].copy_from_slice(Uuid::new_v4().as_bytes());
    prefix[16..30].copy_from_slice(&Uuid::new_v4().as_bytes()[..14]);
    let id = |n: u8| {
        let mut id = prefix;
        id[30] = 1;
        id[31] = n;
        id
    };
    let mut before = prefix;
    before[30] = 0;
    for n in [5, 2, 4, 1, 3, 6] {
        f.repo
            .open_or_refresh(new_channel(id(n), 100))
            .await
            .unwrap();
    }
    f.repo.finalize(&id(4), 0).await.unwrap();
    f.repo
        .mark_closing(&id(5), pending(), Utc::now())
        .await
        .unwrap();

    let ids = |page: &[ChannelRecord]| page.iter().map(|r| r.channel_id).collect::<Vec<_>>();
    let page = f.repo.list_active(Some(before), 2).await.unwrap();
    assert_eq!(ids(&page), vec![id(1), id(2)]);
    let page = f.repo.list_active(Some(id(2)), 2).await.unwrap();
    assert_eq!(
        ids(&page),
        vec![id(3), id(5)],
        "finalized 4 is skipped; closing 5 is still active"
    );
    let page = f.repo.list_active(Some(id(5)), 1).await.unwrap();
    assert_eq!(ids(&page), vec![id(6)]);
    // The first page of an unbounded listing is ordered and bounded.
    let page = f.repo.list_active(None, 3).await.unwrap();
    assert_eq!(page.len(), 3);
    assert!(page.windows(2).all(|w| w[0].channel_id < w[1].channel_id));
    assert!(page
        .iter()
        .all(|record| record.state != ChannelLifecycle::Finalized));
    assert!(f.repo.list_active(None, 0).await.unwrap().is_empty());
}

#[tokio::test]
async fn stale_reservations_are_released_like_release() {
    let f = fixture().await;
    let (one, _) = f.open(100).await;
    let (two, _) = f.open(100).await;
    let (stale_a, _) = f.reserve(&one, 30).await;
    let (stale_b, _) = f.reserve(&one, 20).await;
    let (fresh, _) = f.reserve(&one, 10).await;
    let (stale_c, _) = f.reserve(&two, 40).await;
    for request in [stale_a, stale_b, stale_c] {
        let client = f.pool.get().await.unwrap();
        client
            .execute(
                "UPDATE mpp_channel_reservations SET created_at = NOW() - INTERVAL '2 hours' \
                 WHERE request_id = $1",
                &[&request],
            )
            .await
            .unwrap();
    }
    let cutoff = test_now() - Duration::hours(1);
    assert!(f.repo.release_stale_reservations(cutoff).await.unwrap() >= 3);

    let a = f.stored(&one).await;
    assert_eq!((a.spent, a.reserved), (10, 10));
    let b = f.stored(&two).await;
    assert_eq!((b.spent, b.reserved), (0, 0));
    assert_eq!(f.reservations(&one).await, 1);
    assert_eq!(f.reservations(&two).await, 0);
    assert_eq!(
        f.repo.charge(stale_a, 30, 1).await.unwrap(),
        ChargeOutcome::NoReservation
    );
    assert!(!f.repo.release(stale_b).await.unwrap());
    charged(f.repo.charge(fresh, 10, 1).await.unwrap());
}

#[tokio::test]
async fn purge_removes_old_finalized_channels_and_their_reservations() {
    let f = fixture().await;
    let (old_a, _) = f.open(100).await;
    let (old_b, _) = f.open(100).await;
    let (fresh, _) = f.open(100).await;
    let (open_old, _) = f.open(100).await;
    let (orphaned, _) = f.reserve(&old_a, 10).await;
    let (survivor, _) = f.reserve(&open_old, 10).await;
    for id in [old_a, old_b, fresh] {
        f.repo.finalize(&id, 0).await.unwrap();
    }
    for id in [old_a, old_b, open_old] {
        f.sql(
            "UPDATE mpp_channels SET updated_at = NOW() - INTERVAL '40 days' \
             WHERE channel_id = $1",
            &id,
        )
        .await;
    }
    let cutoff = test_now() - Duration::days(30);
    assert!(f.repo.purge_finalized(cutoff).await.unwrap() >= 2);
    assert!(f.repo.get(&old_a).await.unwrap().is_none());
    assert!(f.repo.get(&old_b).await.unwrap().is_none());
    assert!(f.repo.get(&fresh).await.unwrap().is_some());
    assert!(
        f.repo.get(&open_old).await.unwrap().is_some(),
        "an old open channel is kept"
    );
    assert_eq!(
        f.reservations(&old_a).await,
        0,
        "the reservation went with it"
    );
    assert_eq!(
        f.repo.charge(orphaned, 1, 1).await.unwrap(),
        ChargeOutcome::NoReservation
    );
    assert!(f.repo.release(survivor).await.unwrap());
}

#[tokio::test]
async fn amounts_up_to_the_numeric_range_round_trip_exactly() {
    let f = fixture().await;
    let id = random_id();
    let big = 10u128.pow(38) + 12_345;
    let mut channel = new_channel(id, big);
    channel.deposit = u128::MAX / 2;
    let record = f.repo.open_or_refresh(channel).await.unwrap();
    assert_eq!(
        (record.highest_cumulative, record.deposit),
        (big, u128::MAX / 2)
    );
    let (request, _) = f.reserve(&id, big - 1).await;
    assert_eq!(
        f.repo.reserve(&id, Uuid::new_v4(), 2).await.unwrap(),
        ReserveOutcome::Insufficient { available: 1 }
    );
    let (record, paid) = charged(f.repo.charge(request, big, 1).await.unwrap());
    assert_eq!((paid, record.spent), (big, big));
}

mod hardening;
