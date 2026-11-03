//! `verify_payment` (the chain work, no claim), `claim`, `finish` and the serving guard.

use std::time::Duration as StdDuration;

use super::*;
use crate::common::RepositoryError;
use crate::mpp::testing::{
    framed_transaction_payload, payment_transaction, tempo_transaction, transaction_payload,
    transfer_with_memo_input, FlakyClaimRepository, PaymentTx,
};

/// What the chain must pay for `challenge`.
fn paying<'a>(challenge: &'a Challenge, seed: u64) -> PaymentTx<'a> {
    PaymentTx {
        chain_id: CHAIN,
        currency: CURRENCY,
        recipient: RECIPIENT,
        amount: AMOUNT,
        realm: REALM,
        challenge_id: &challenge.id,
        seed,
    }
}

/// A `transaction` credential for a challenge, with its receipt mined (unless `mine` is off).
struct TxCredential {
    header: String,
    hash: [u8; 32],
}

impl Fixture {
    fn tx_credential(&self, seed: u64, mine: bool) -> TxCredential {
        let challenge = self.challenge(&body());
        let raw = payment_transaction(&paying(&challenge, seed));
        let hash = tempo::transaction_hash(&raw);
        if mine {
            self.chain.mine(bound_payment_receipt(
                hash, PAYER, CURRENCY, RECIPIENT, AMOUNT, REALM, &challenge,
            ));
        }
        let header = credential_header(&challenge, transaction_payload(&raw), None);
        TxCredential { header, hash }
    }

    async fn verify_payment(&self, header: &str) -> Result<PaidCredential, MppError> {
        self.service
            .verify_payment(&self.verify(header, &body()).unwrap())
            .await
    }
}

// ----- verify_payment: chain work, no claim -----

#[tokio::test]
async fn a_paid_hash_credential_is_verified_without_claiming_and_claimed_serving() {
    let f = fixture();
    let (_, header) = f.paid_hash_credential(&body(), TX);
    let verified = f.verify(&header, &body()).unwrap();

    let paid = f.service.verify_payment(&verified).await.unwrap();
    assert_eq!(paid.payment_key(), key(TX));
    assert_eq!(
        f.repo.state_of(&key(TX)),
        None,
        "verification claims nothing"
    );
    // It can be repeated: a request that a later layer refuses leaves the credential good.
    let again = f.service.verify_payment(&verified).await.unwrap();
    assert_eq!(again, paid);
    assert_eq!(f.repo.state_of(&key(TX)), None);

    let settled = f.service.claim(&paid).await.unwrap();
    assert_eq!(settled.payment_key(), key(TX));
    assert_eq!(f.repo.state_of(&key(TX)), Some(ClaimState::Serving));
    let receipt = settled.receipt_header(Utc::now()).unwrap();
    let decoded: serde_json::Value =
        serde_json::from_slice(&b64url_decode(&receipt).unwrap()).unwrap();
    assert_eq!(decoded["method"], "tempo");
    assert_eq!(decoded["status"], "success");
    // The receipt names the real transaction; only the ledger key is keyed.
    assert_eq!(decoded["reference"], format!("0x{}", hex::encode(TX)));
    assert!(!key(TX).contains(&hex::encode(TX)));
}

#[tokio::test]
async fn a_paid_transaction_credential_is_broadcast_and_verified() {
    let f = fixture();
    let tx = f.tx_credential(7, true);
    let paid = f.verify_payment(&tx.header).await.unwrap();
    assert_eq!(paid.payment_key(), key(tx.hash));
    assert_eq!(f.chain.send_calls(), 1);
    assert_eq!(f.chain.lookup_calls(), 0);
    assert_eq!(f.repo.state_of(&key(tx.hash)), None);
}

#[tokio::test]
async fn a_known_transaction_is_recovered_by_hash_after_a_broadcast_error() {
    let f = fixture();
    f.chain.set_send_mode(SendMode::Reject);
    let tx = f.tx_credential(8, true);
    let paid = f.verify_payment(&tx.header).await.unwrap();
    assert_eq!(paid.payment_key(), key(tx.hash));
    assert_eq!(f.chain.send_calls(), 1);
    assert_eq!(f.chain.lookup_calls(), 1);
}

#[tokio::test]
async fn a_node_that_refused_an_unknown_transaction_is_receipt_not_found_after_bounded_lookups() {
    let f = fixture();
    let tx = f.tx_credential(9, false);
    let error = f.verify_payment(&tx.header).await.unwrap_err();
    // Retryable with the same credential (a 503), never a fresh challenge: the client may
    // already have paid.
    assert_eq!(error, MppError::ReceiptNotFound);
    assert!(matches!(error.response(), MppResponse::Retry(_)));
    assert_eq!(f.chain.lookup_calls(), 3, "exactly the configured attempts");
    assert_eq!(f.chain.send_calls(), 1);
}

#[tokio::test]
async fn a_hash_the_node_does_not_know_is_receipt_not_found() {
    let f = fixture();
    let challenge = f.challenge(&body());
    let header = credential_header(&challenge, hash_payload([0x99; 32]), None);
    assert_eq!(
        f.verify_payment(&header).await.unwrap_err(),
        MppError::ReceiptNotFound
    );
}

#[tokio::test]
async fn an_unreachable_node_is_unavailable_not_a_missing_payment() {
    let f = fixture();
    // Hash payload, RPC down.
    f.chain.set_lookup_error(Some(RpcError::Unavailable));
    let (_, header) = f.paid_hash_credential(&body(), TX);
    let error = f.verify_payment(&header).await.unwrap_err();
    assert_eq!(error, MppError::Unavailable(UnavailableReason::Rpc));
    assert!(matches!(error.response(), MppResponse::Retry(_)));
    assert_eq!(f.repo.state_of(&key(TX)), None);

    // Transaction payload, transport failure on the broadcast and nothing found afterwards.
    let f = fixture();
    f.chain.set_send_mode(SendMode::Unavailable);
    let tx = f.tx_credential(3, false);
    assert_eq!(
        f.verify_payment(&tx.header).await.unwrap_err(),
        MppError::Unavailable(UnavailableReason::Rpc)
    );
    // The broadcast error is transport, but the tx is already mined: the lookup rescues it.
    let mined = f.tx_credential(4, true);
    assert!(f.verify_payment(&mined.header).await.is_ok());
}

#[tokio::test]
async fn a_refused_broadcast_with_failing_lookups_is_unavailable_after_one_lookup() {
    // The node refused the transaction, and then cannot be asked about it: that is not
    // "no such transaction", and it is not worth asking again within this request.
    let f = fixture();
    f.chain.set_send_mode(SendMode::Reject);
    f.chain.set_lookup_error(Some(RpcError::Unavailable));
    let tx = f.tx_credential(11, true);
    let error = f.verify_payment(&tx.header).await.unwrap_err();
    assert_eq!(error, MppError::Unavailable(UnavailableReason::Rpc));
    assert_eq!(f.chain.send_calls(), 1);
    assert_eq!(f.chain.lookup_calls(), 1, "a lookup error ends the polling");
}

#[tokio::test]
async fn the_whole_receipt_phase_is_bounded_by_one_deadline() {
    let repo = Arc::new(InMemoryClaimRepository::default());
    let chain = Arc::new(FakeTempo::default());
    let service = MppService::new(&settings(), repo, chain.clone())
        .unwrap()
        .with_receipt_deadline(StdDuration::from_millis(60));
    chain.set_send_mode(SendMode::Hang);
    let challenge = service
        .challenge_for_body(CHAT_COMPLETIONS_ROUTE, &body())
        .unwrap();
    let raw = payment_transaction(&paying(&challenge, 1));
    let header = credential_header(&challenge, transaction_payload(&raw), None);
    let verified = service
        .verify(&header, CHAT_COMPLETIONS_ROUTE, &body())
        .unwrap();
    let started = std::time::Instant::now();
    assert_eq!(
        service.verify_payment(&verified).await.unwrap_err(),
        MppError::Unavailable(UnavailableReason::Rpc)
    );
    assert!(started.elapsed() < StdDuration::from_secs(5));
}

#[tokio::test]
async fn a_node_answering_with_another_transactions_receipt_is_refused() {
    let f = fixture();
    let challenge = f.challenge(&body());
    // Mined under hash [0x99] but claiming to be [0x98].
    let mut wrong = bound_payment_receipt(
        [0x98; 32], PAYER, CURRENCY, RECIPIENT, AMOUNT, REALM, &challenge,
    );
    wrong.tx_hash = [0x98; 32];
    f.chain.mine(wrong.clone());
    f.chain.mined_alias([0x99; 32], wrong.clone());
    let header = credential_header(&challenge, hash_payload([0x99; 32]), None);
    assert_eq!(
        f.verify_payment(&header).await.unwrap_err(),
        MppError::VerificationFailed(VerifyFailure::ReceiptMismatch)
    );
    // The same for a broadcast: the receipt must be the sent transaction's.
    let tx = f.tx_credential(12, false);
    f.chain.set_send_receipt(Some(wrong));
    assert_eq!(
        f.verify_payment(&tx.header).await.unwrap_err(),
        MppError::VerificationFailed(VerifyFailure::ReceiptMismatch)
    );
}

#[tokio::test]
async fn verification_negatives_map_to_the_documented_errors() {
    let f = fixture();
    let challenge = f.challenge(&body());
    let verify = |tx: [u8; 32], receipt: RpcReceipt| {
        let f = &f;
        let challenge = challenge.clone();
        async move {
            f.chain.mine(RpcReceipt {
                tx_hash: tx,
                ..receipt
            });
            let header = credential_header(&challenge, hash_payload(tx), None);
            f.verify_payment(&header).await
        }
    };
    let memo = Some(tempo::attribution_memo(REALM, &challenge.id));
    let ok = |hash| payment_receipt(hash, PAYER, CURRENCY, RECIPIENT, AMOUNT, memo);

    let mut reverted = ok([1; 32]);
    reverted.success = false;
    assert_eq!(
        verify([1; 32], reverted).await.unwrap_err(),
        MppError::VerificationFailed(VerifyFailure::Reverted)
    );
    for (hash, receipt) in [
        (
            [2; 32],
            payment_receipt([2; 32], PAYER, CURRENCY, [0xbb; 20], AMOUNT, memo),
        ),
        (
            [3; 32],
            payment_receipt([3; 32], PAYER, CURRENCY, RECIPIENT, AMOUNT - 1, memo),
        ),
        (
            [4; 32],
            payment_receipt([4; 32], PAYER, [0x21; 20], RECIPIENT, AMOUNT, memo),
        ),
    ] {
        assert_eq!(
            verify(hash, receipt).await.unwrap_err(),
            MppError::VerificationFailed(VerifyFailure::NoMatchingTransfer)
        );
    }
    // Sent by someone other than the transaction sender.
    let mut other_sender = payment_receipt([5; 32], [0x77; 20], CURRENCY, RECIPIENT, AMOUNT, memo);
    other_sender.from = PAYER;
    assert_eq!(
        verify([5; 32], other_sender).await.unwrap_err(),
        MppError::VerificationFailed(VerifyFailure::NoMatchingTransfer)
    );
    // Funds moved without this challenge's memo: final, 403, no new challenge.
    for (hash, memo) in [
        ([6; 32], None),
        (
            [7; 32],
            Some(tempo::attribution_memo(REALM, "another-challenge-id")),
        ),
        (
            [8; 32],
            Some(tempo::attribution_memo("evil.example", &challenge.id)),
        ),
    ] {
        let error = verify(
            hash,
            payment_receipt(hash, PAYER, CURRENCY, RECIPIENT, AMOUNT, memo),
        )
        .await
        .unwrap_err();
        assert_eq!(error, MppError::Consumed(ConsumedReason::MemoNotBound));
        assert!(matches!(error.response(), MppResponse::Problem(..)));
    }
    // Nothing was ever claimed: verification does not touch the ledger.
    assert!(f.repo.is_empty());
}

#[tokio::test]
async fn the_source_did_decides_who_must_have_paid() {
    let f = fixture();
    let challenge = f.challenge(&body());
    // A relayer sent the transaction; PAYER's funds moved.
    let mut receipt =
        bound_payment_receipt(TX, PAYER, CURRENCY, RECIPIENT, AMOUNT, REALM, &challenge);
    receipt.from = [0x55; 20];
    f.chain.mine(receipt);
    let did = |a: [u8; 20]| format!("did:pkh:eip155:{CHAIN}:{}", address(a));
    let without = credential_header(&challenge, hash_payload(TX), None);
    assert_eq!(
        f.verify_payment(&without).await.unwrap_err(),
        MppError::VerificationFailed(VerifyFailure::NoMatchingTransfer)
    );
    let wrong = credential_header(&challenge, hash_payload(TX), Some(&did([0x66; 20])));
    assert_eq!(
        f.verify_payment(&wrong).await.unwrap_err(),
        MppError::VerificationFailed(VerifyFailure::NoMatchingTransfer)
    );
    let right = credential_header(&challenge, hash_payload(TX), Some(&did(PAYER)));
    assert!(f.verify_payment(&right).await.is_ok());
}

// ----- the transaction is checked before it is broadcast -----

#[tokio::test]
async fn a_transaction_that_does_not_pay_the_challenge_is_never_broadcast() {
    let f = fixture();
    let challenge = f.challenge(&body());
    let pays = |tx: &PaymentTx<'_>| payment_transaction(tx);
    let memo = tempo::attribution_memo(REALM, &challenge.id);
    let good_input = transfer_with_memo_input(RECIPIENT, AMOUNT, memo);
    let cases: Vec<(&str, Vec<u8>)> = vec![
        ("not a payment at all", framed_transaction_payload(1).1),
        (
            "another chain",
            pays(&PaymentTx {
                chain_id: 1,
                ..paying(&challenge, 1)
            }),
        ),
        (
            "an underpayment",
            pays(&PaymentTx {
                amount: AMOUNT - 1,
                ..paying(&challenge, 1)
            }),
        ),
        (
            "another recipient",
            pays(&PaymentTx {
                recipient: [0xbb; 20],
                ..paying(&challenge, 1)
            }),
        ),
        (
            "another currency contract",
            pays(&PaymentTx {
                currency: [0x21; 20],
                ..paying(&challenge, 1)
            }),
        ),
        (
            "a memo for another challenge",
            pays(&PaymentTx {
                challenge_id: "another-challenge-id",
                ..paying(&challenge, 1)
            }),
        ),
        (
            "a memo for another realm",
            pays(&PaymentTx {
                realm: "evil.example",
                ..paying(&challenge, 1)
            }),
        ),
        (
            "a call that sends value",
            tempo_transaction(CHAIN, &[(CURRENCY, 1, good_input.clone())], 1),
        ),
        (
            "a different contract call",
            tempo_transaction(CHAIN, &[([0x77; 20], 0, good_input)], 1),
        ),
        ("no calls", tempo_transaction(CHAIN, &[], 1)),
    ];
    for (label, raw) in cases {
        // Even if the chain would happily mine it, it must not be sent.
        f.chain.mine(bound_payment_receipt(
            tempo::transaction_hash(&raw),
            PAYER,
            CURRENCY,
            RECIPIENT,
            AMOUNT,
            REALM,
            &challenge,
        ));
        let header = credential_header(&challenge, transaction_payload(&raw), None);
        let error = f.verify_payment(&header).await.unwrap_err();
        assert!(
            matches!(
                error,
                MppError::VerificationFailed(VerifyFailure::Transaction(_))
            ),
            "{label}: {error:?}"
        );
        assert!(matches!(error.response(), MppResponse::Challenge(_)));
    }
    assert_eq!(f.chain.send_calls(), 0, "nothing reached the node");
    assert_eq!(f.chain.lookup_calls(), 0);
}

// ----- claim state machine through the service -----

#[tokio::test]
async fn a_replayed_credential_is_consumed() {
    let f = fixture();
    let (_, header) = f.paid_hash_credential(&body(), TX);
    let paid = f.verify_payment(&header).await.unwrap();
    f.service.claim(&paid).await.unwrap();
    let replay = f.service.claim(&paid).await.unwrap_err();
    assert_eq!(replay, MppError::Consumed(ConsumedReason::AlreadyClaimed));
    assert!(matches!(
        replay.response(),
        MppResponse::Problem(ProblemStatus::Forbidden, _)
    ));
}

#[tokio::test]
async fn an_unserved_payment_is_retried_without_a_second_payment() {
    let f = fixture();
    let (_, header) = f.paid_hash_credential(&body(), TX);
    let paid = f.verify_payment(&header).await.unwrap();
    let first = f.service.claim(&paid).await.unwrap();
    f.service
        .finish(&first, ServeOutcome::Unserved)
        .await
        .unwrap();
    assert_eq!(f.repo.state_of(&key(TX)), Some(ClaimState::Unserved));
    // The same credential is verified again (the chain read is idempotent) and claimed again.
    let paid = f.verify_payment(&header).await.unwrap();
    let retry = f.service.claim(&paid).await.unwrap();
    assert_eq!(retry.payment_key(), first.payment_key());
    assert_eq!(f.repo.state_of(&key(TX)), Some(ClaimState::Serving));
    // Served is final.
    f.service
        .finish(&retry, ServeOutcome::Served)
        .await
        .unwrap();
    assert_eq!(
        f.service.claim(&paid).await.unwrap_err(),
        MppError::Consumed(ConsumedReason::AlreadyClaimed)
    );
}

#[tokio::test]
async fn a_second_challenge_cannot_reuse_a_claimed_payment_even_when_unserved() {
    let f = fixture();
    let (_, header) = f.paid_hash_credential(&body(), TX);
    let first = f
        .service
        .claim(&f.verify_payment(&header).await.unwrap())
        .await
        .unwrap();
    f.service
        .finish(&first, ServeOutcome::Unserved)
        .await
        .unwrap();
    // Same transaction, presented under a different challenge (different body). The spent
    // check refuses it before the chain is read (memo binding and the claim store would
    // refuse it too).
    let mut other_body = body();
    other_body.extend_from_slice(b"\n");
    let other_challenge = f.challenge(&other_body);
    let other_header = credential_header(&other_challenge, hash_payload(TX), None);
    assert_eq!(
        f.service
            .verify_payment(&f.verify(&other_header, &other_body).unwrap())
            .await
            .unwrap_err(),
        MppError::Consumed(ConsumedReason::AlreadyClaimed)
    );
    assert_eq!(
        f.repo.claim(&key(TX), &other_challenge.id).await.unwrap(),
        ClaimOutcome::AlreadyUsed
    );
    assert_eq!(f.repo.state_of(&key(TX)), Some(ClaimState::Unserved));
}

#[tokio::test]
async fn a_failing_claim_store_is_unavailable_and_claims_nothing() {
    let repo = Arc::new(FlakyClaimRepository::default());
    let chain = Arc::new(FakeTempo::default());
    let service = MppService::new(&settings(), repo.clone(), chain.clone()).unwrap();
    let challenge = service
        .challenge_for_body(CHAT_COMPLETIONS_ROUTE, &body())
        .unwrap();
    chain.mine(bound_payment_receipt(
        TX, PAYER, CURRENCY, RECIPIENT, AMOUNT, REALM, &challenge,
    ));
    let header = credential_header(&challenge, hash_payload(TX), None);
    let verified = service
        .verify(&header, CHAT_COMPLETIONS_ROUTE, &body())
        .unwrap();
    let paid = service.verify_payment(&verified).await.unwrap();

    repo.set_fail_claims(true);
    let error = service.claim(&paid).await.unwrap_err();
    assert_eq!(error, MppError::Unavailable(UnavailableReason::Store));
    assert!(matches!(error.response(), MppResponse::Retry(_)));
    assert_eq!(repo.store().state_of(&key(TX)), None);

    // The store comes back: the same credential is claimed (nothing was half-done).
    repo.set_fail_claims(false);
    assert!(service.claim(&paid).await.is_ok());
    assert_eq!(repo.store().state_of(&key(TX)), Some(ClaimState::Serving));
}

#[tokio::test]
async fn a_failing_finish_is_reported_and_leaves_the_claim_serving() {
    let repo = Arc::new(FlakyClaimRepository::default());
    let chain = Arc::new(FakeTempo::default());
    let service = Arc::new(MppService::new(&settings(), repo.clone(), chain.clone()).unwrap());
    let challenge = service
        .challenge_for_body(CHAT_COMPLETIONS_ROUTE, &body())
        .unwrap();
    chain.mine(bound_payment_receipt(
        TX, PAYER, CURRENCY, RECIPIENT, AMOUNT, REALM, &challenge,
    ));
    let header = credential_header(&challenge, hash_payload(TX), None);
    let verified = service
        .verify(&header, CHAT_COMPLETIONS_ROUTE, &body())
        .unwrap();
    let settled = service
        .claim(&service.verify_payment(&verified).await.unwrap())
        .await
        .unwrap();
    repo.set_fail_finishes(true);
    assert_eq!(
        service
            .finish(&settled, ServeOutcome::Served)
            .await
            .unwrap_err(),
        MppError::Unavailable(UnavailableReason::Store)
    );
    // `serving` also refuses reuse, so a failed finish can never double-spend a payment.
    assert_eq!(repo.store().state_of(&key(TX)), Some(ClaimState::Serving));
    service.serving(settled).finish(ServeOutcome::Served).await;
    assert_eq!(repo.store().state_of(&key(TX)), Some(ClaimState::Serving));
    assert_eq!(
        service.verify_payment(&verified).await.unwrap_err(),
        MppError::Consumed(ConsumedReason::AlreadyClaimed)
    );
}

#[test]
fn repository_failures_are_logged_by_variant_name_only() {
    use crate::mpp::error::repository_error_kind;
    let secret_bearing = RepositoryError::DatabaseError(anyhow::anyhow!(
        "duplicate key value violates unique constraint (payment_key)=(tempo:abc)"
    ));
    assert_eq!(repository_error_kind(&secret_bearing), "database_error");
    assert_eq!(
        repository_error_kind(&RepositoryError::QueryTimeout),
        "query_timeout"
    );
    assert_eq!(RpcError::Unavailable.kind(), "unavailable");
    assert_eq!(RpcError::Rejected.kind(), "rejected");
    assert_eq!(RpcError::InvalidResponse.kind(), "invalid_response");
}

#[tokio::test]
async fn a_transaction_credential_cannot_broadcast_after_the_challenge_expired() {
    // The clock is injected: nothing sleeps.
    let f = fixture();
    let issued = Utc::now();
    let challenge = f
        .service
        .challenge_at(
            issued,
            CHAT_COMPLETIONS_ROUTE,
            &body(),
            &Price { amount: AMOUNT },
        )
        .unwrap();
    let raw = payment_transaction(&paying(&challenge, 5));
    f.chain.mine(bound_payment_receipt(
        tempo::transaction_hash(&raw),
        PAYER,
        CURRENCY,
        RECIPIENT,
        AMOUNT,
        REALM,
        &challenge,
    ));
    // Mined, but only after the challenge expired: too late to count.
    f.chain
        .set_block_time(1, challenge.expires.timestamp() as u64 + 1);
    let header = credential_header(&challenge, transaction_payload(&raw), None);
    let verified = f
        .service
        .verify_at(issued, &header, CHAT_COMPLETIONS_ROUTE, &body())
        .unwrap();
    assert_eq!(
        f.service
            .verify_payment_at(challenge.expires, &verified)
            .await
            .unwrap_err(),
        MppError::Expired
    );
    assert_eq!(
        f.chain.send_calls(),
        0,
        "an expired challenge must not reach the node"
    );
    // One second earlier it still may.
    assert!(f
        .service
        .verify_payment_at(challenge.expires - ChronoDuration::seconds(1), &verified)
        .await
        .is_ok());
    assert_eq!(f.chain.send_calls(), 1);
}

// ----- serving guard -----

#[tokio::test]
async fn the_guard_marks_served_when_dropped_and_respects_finish() {
    let f = fixture();
    let (_, header) = f.paid_hash_credential(&body(), TX);
    let settled = f
        .service
        .claim(&f.verify_payment(&header).await.unwrap())
        .await
        .unwrap();

    let guard = f.service.serving(settled.clone());
    assert_eq!(
        guard.payment().map(|p| p.payment_key().to_string()),
        Some(key(TX))
    );
    drop(guard);
    // The spawned finish runs on the same runtime; wait for it deterministically.
    for _ in 0..100 {
        if f.repo.state_of(&key(TX)) == Some(ClaimState::Served) {
            break;
        }
        tokio::task::yield_now().await;
    }
    assert_eq!(f.repo.state_of(&key(TX)), Some(ClaimState::Served));

    // A client that disconnected cannot present the payment again.
    let replay = f.verify_payment(&header).await;
    assert!(matches!(replay, Err(MppError::Consumed(_))), "{replay:?}");

    // An explicit unserved finish releases the claim for a retry under the same challenge.
    let (_, header) = f.paid_hash_credential(&body(), TX_B);
    let settled = f
        .service
        .claim(&f.verify_payment(&header).await.unwrap())
        .await
        .unwrap();
    f.service
        .serving(settled)
        .finish(ServeOutcome::Unserved)
        .await;
    assert_eq!(f.repo.state_of(&key(TX_B)), Some(ClaimState::Unserved));
    let retried = f
        .service
        .claim(&f.verify_payment(&header).await.unwrap())
        .await;
    assert!(retried.is_ok(), "{retried:?}");
}

// ----- late redemption (a paid credential refused until its challenge expired) -----

/// A challenge issued `ago` before `now`, at the fixture's price for `body()`.
fn challenge_issued(f: &Fixture, now: DateTime<Utc>, ago: ChronoDuration) -> Challenge {
    let Pricing::Priced(price) = f.service.price_body(&body()) else {
        panic!("body() prices");
    };
    f.service
        .challenge_at(now - ago, CHAT_COMPLETIONS_ROUTE, &body(), &price)
        .unwrap()
}

#[tokio::test]
async fn a_payment_mined_before_expiry_is_redeemable_after_it() {
    let f = fixture();
    let now = Utc::now();
    // TTL 600s, issued 2h ago: expired 1h50m ago, inside the late-redemption window.
    let challenge = challenge_issued(&f, now, ChronoDuration::hours(2));
    let mut receipt =
        bound_payment_receipt(TX, PAYER, CURRENCY, RECIPIENT, AMOUNT, REALM, &challenge);
    receipt.block_number = 7;
    f.chain.mine(receipt);
    let expires = challenge.expires.timestamp() as u64;
    f.chain.set_block_time(7, expires - 30);
    let header = credential_header(&challenge, hash_payload(TX), None);

    let verified = f
        .service
        .verify_at(now, &header, CHAT_COMPLETIONS_ROUTE, &body())
        .unwrap();
    let paid = f.service.verify_payment_at(now, &verified).await.unwrap();
    assert!(
        f.service.claim(&paid).await.is_ok(),
        "a stranded payment is redeemed"
    );
    // Still single use: refused by the spent check, and by the atomic claim itself.
    assert_eq!(
        f.service
            .verify_payment_at(now, &verified)
            .await
            .unwrap_err(),
        MppError::Consumed(ConsumedReason::AlreadyClaimed)
    );
    assert!(matches!(
        f.service.claim(&paid).await,
        Err(MppError::Consumed(_))
    ));
}

#[tokio::test]
async fn a_payment_mined_after_expiry_is_not_redeemable() {
    let f = fixture();
    let now = Utc::now();
    let challenge = challenge_issued(&f, now, ChronoDuration::hours(2));
    let mut receipt =
        bound_payment_receipt(TX, PAYER, CURRENCY, RECIPIENT, AMOUNT, REALM, &challenge);
    receipt.block_number = 9;
    f.chain.mine(receipt);
    f.chain
        .set_block_time(9, challenge.expires.timestamp() as u64 + 1);
    let header = credential_header(&challenge, hash_payload(TX), None);
    let verified = f
        .service
        .verify_at(now, &header, CHAT_COMPLETIONS_ROUTE, &body())
        .unwrap();
    assert_eq!(
        f.service
            .verify_payment_at(now, &verified)
            .await
            .unwrap_err(),
        MppError::Expired
    );
}

#[tokio::test]
async fn an_expired_unpaid_credential_is_expired_and_past_the_window_nothing_is_read() {
    let f = fixture();
    let now = Utc::now();
    // Expired but inside the window, never paid: expired, not "retry".
    let challenge = challenge_issued(&f, now, ChronoDuration::hours(2));
    let header = credential_header(&challenge, hash_payload(TX), None);
    let verified = f
        .service
        .verify_at(now, &header, CHAT_COMPLETIONS_ROUTE, &body())
        .unwrap();
    assert_eq!(
        f.service
            .verify_payment_at(now, &verified)
            .await
            .unwrap_err(),
        MppError::Expired
    );
    // Past the late-redemption window the credential is refused before any lookup.
    let stale = challenge_issued(&f, now, ChronoDuration::hours(13));
    let header = credential_header(&stale, hash_payload(TX), None);
    let lookups = f.chain.lookup_calls();
    assert_eq!(
        f.service
            .verify_at(now, &header, CHAT_COMPLETIONS_ROUTE, &body())
            .unwrap_err(),
        MppError::Expired
    );
    assert_eq!(f.chain.lookup_calls(), lookups);
}

#[tokio::test]
async fn an_expired_transaction_credential_is_looked_up_never_broadcast() {
    let f = fixture();
    let now = Utc::now();
    let challenge = challenge_issued(&f, now, ChronoDuration::hours(2));
    let raw = payment_transaction(&paying(&challenge, 11));
    let hash = tempo::transaction_hash(&raw);
    let header = credential_header(&challenge, transaction_payload(&raw), None);
    let verified = f
        .service
        .verify_at(now, &header, CHAT_COMPLETIONS_ROUTE, &body())
        .unwrap();

    // Not mined: expired, and nothing was sent to the node.
    assert_eq!(
        f.service
            .verify_payment_at(now, &verified)
            .await
            .unwrap_err(),
        MppError::Expired
    );
    assert_eq!(f.chain.send_calls(), 0);

    // Mined before expiry (the client or an earlier attempt broadcast it): redeemed by lookup.
    let mut receipt =
        bound_payment_receipt(hash, PAYER, CURRENCY, RECIPIENT, AMOUNT, REALM, &challenge);
    receipt.block_number = 3;
    f.chain.mine(receipt);
    f.chain
        .set_block_time(3, challenge.expires.timestamp() as u64 - 1);
    assert!(f.service.verify_payment_at(now, &verified).await.is_ok());
    assert_eq!(f.chain.send_calls(), 0, "never broadcast after expiry");
}

#[tokio::test]
async fn a_spent_payment_is_refused_before_any_rpc_call() {
    let f = fixture();
    let (_, header) = f.paid_hash_credential(&body(), TX);
    let verified = f.verify(&header, &body()).unwrap();
    let paid = f.service.verify_payment(&verified).await.unwrap();
    let settled = f.service.claim(&paid).await.unwrap();
    f.service
        .finish(&settled, ServeOutcome::Served)
        .await
        .unwrap();

    let lookups = f.chain.lookup_calls();
    assert_eq!(
        f.service.verify_payment(&verified).await.unwrap_err(),
        MppError::Consumed(ConsumedReason::AlreadyClaimed)
    );
    assert_eq!(
        f.chain.lookup_calls(),
        lookups,
        "no RPC call for a spent payment"
    );
}

#[tokio::test]
async fn an_unserved_payment_of_the_same_challenge_is_not_spent() {
    // The retry path stays open: a payment released by an error response is not "spent" for
    // the challenge it was bound to.
    let f = fixture();
    let (_, header) = f.paid_hash_credential(&body(), TX);
    let verified = f.verify(&header, &body()).unwrap();
    let paid = f.service.verify_payment(&verified).await.unwrap();
    let settled = f.service.claim(&paid).await.unwrap();
    f.service
        .finish(&settled, ServeOutcome::Unserved)
        .await
        .unwrap();
    let again = f.service.verify_payment(&verified).await.unwrap();
    assert!(f.service.claim(&again).await.is_ok());
}

#[tokio::test]
async fn a_failing_spent_check_is_unavailable_and_reads_no_chain() {
    let repo = Arc::new(FlakyClaimRepository::default());
    let chain = Arc::new(FakeTempo::default());
    let service = MppService::new(&settings(), repo.clone(), chain.clone()).unwrap();
    let challenge = service
        .challenge_for_body(CHAT_COMPLETIONS_ROUTE, &body())
        .unwrap();
    let header = credential_header(&challenge, hash_payload(TX), None);
    let verified = service
        .verify(&header, CHAT_COMPLETIONS_ROUTE, &body())
        .unwrap();
    repo.set_fail_claims(true);
    assert_eq!(
        service.verify_payment(&verified).await.unwrap_err(),
        MppError::Unavailable(UnavailableReason::Store)
    );
    assert_eq!(chain.lookup_calls(), 0);
}
