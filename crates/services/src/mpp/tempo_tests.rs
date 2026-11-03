use super::*;
use crate::mpp::ports::{RpcLog, RpcReceipt};

const REALM: &str = "cloud-api.near.ai";
const CHALLENGE: &str = "XmJ98SdsAdzwP9Oa-8In322Uh6yweMO6rywdomWk_V4";
const CURRENCY: [u8; 20] = [0x20; 20];
const RECIPIENT: [u8; 20] = [0x30; 20];
const PAYER: [u8; 20] = [0x40; 20];
const OTHER: [u8; 20] = [0x50; 20];
const AMOUNT: u128 = 20_000;

fn word(address: [u8; 20]) -> [u8; 32] {
    let mut topic = [0u8; 32];
    topic[12..].copy_from_slice(&address);
    topic
}

fn amount_word(amount: u128) -> Vec<u8> {
    let mut data = vec![0u8; 32];
    data[16..].copy_from_slice(&amount.to_be_bytes());
    data
}

fn transfer(token: [u8; 20], from: [u8; 20], to: [u8; 20], amount: u128) -> RpcLog {
    RpcLog {
        address: token,
        topics: vec![TRANSFER_TOPIC, word(from), word(to)],
        data: amount_word(amount),
    }
}

fn transfer_with_memo(
    token: [u8; 20],
    from: [u8; 20],
    to: [u8; 20],
    amount: u128,
    memo: [u8; 32],
) -> RpcLog {
    RpcLog {
        address: token,
        topics: vec![TRANSFER_WITH_MEMO_TOPIC, word(from), word(to), memo],
        data: amount_word(amount),
    }
}

/// What `transferWithMemo` emits on Tempo: `Transfer`, then `TransferWithMemo`.
fn memo_pair(memo: [u8; 32]) -> Vec<RpcLog> {
    vec![
        transfer(CURRENCY, PAYER, RECIPIENT, AMOUNT),
        transfer_with_memo(CURRENCY, PAYER, RECIPIENT, AMOUNT, memo),
    ]
}

fn receipt(logs: Vec<RpcLog>) -> RpcReceipt {
    RpcReceipt {
        tx_hash: [0x77; 32],
        success: true,
        from: PAYER,
        logs,
    }
}

fn expected() -> Expected<'static> {
    Expected {
        currency: CURRENCY,
        recipient: RECIPIENT,
        amount: AMOUNT,
        source: None,
        realm: REALM,
        challenge_id: CHALLENGE,
    }
}

fn good_memo() -> [u8; 32] {
    attribution_memo(REALM, CHALLENGE)
}

#[test]
fn event_topics_and_memo_tag_are_the_keccak_of_their_signatures() {
    assert_eq!(
        TRANSFER_TOPIC,
        Keccak256::digest(b"Transfer(address,address,uint256)").as_slice()
    );
    assert_eq!(
        TRANSFER_WITH_MEMO_TOPIC,
        Keccak256::digest(b"TransferWithMemo(address,address,uint256,bytes32)").as_slice()
    );
    assert_eq!(MPP_MEMO_TAG, Keccak256::digest(b"mpp")[..4]);
    // And the well-known ERC-20 value, so a wrong signature string cannot pass both ways.
    assert_eq!(
        hex::encode(TRANSFER_TOPIC),
        "ddf252ad1be2c89b69c2b068fc378daa952ba7f163c4a11628f55a4df523b3ef"
    );
}

/// Layout from `tempo/attribution.rs`: tag(4) | version(1) | serverId(10) | clientId(10) |
/// nonce(7). Expected bytes computed independently of this crate.
#[test]
fn attribution_memo_layout_matches_the_reference() {
    let memo = attribution_memo("api.example.com", "challenge-123");
    assert_eq!(
        hex::encode(memo),
        "ef1ed712011ece072f76bd8b82350e00000000000000000000eb18f2d4402269"
    );
    assert_eq!(memo[..4], Keccak256::digest(b"mpp")[..4]);
    assert_eq!(memo[4], 0x01);
    assert_eq!(memo[5..15], Keccak256::digest(b"api.example.com")[..10]);
    assert_eq!(memo[15..25], [0u8; 10]);
    assert_eq!(memo[25..32], Keccak256::digest(b"challenge-123")[..7]);
}

#[test]
fn memo_binding_checks_realm_challenge_tag_and_version() {
    let memo = attribution_memo(REALM, CHALLENGE);
    assert!(is_mpp_memo(&memo));
    assert!(is_bound_memo(&memo, REALM, CHALLENGE));
    assert!(!is_bound_memo(&memo, "other.example", CHALLENGE));
    assert!(!is_bound_memo(&memo, REALM, "another-challenge"));
    let mut wrong_version = memo;
    wrong_version[4] = 2;
    assert!(!is_mpp_memo(&wrong_version));
    assert!(!is_bound_memo(&wrong_version, REALM, CHALLENGE));
    assert!(!is_mpp_memo(&[0u8; 32]));
    // A client fingerprint in bytes 15..25 does not break the binding.
    let mut with_client = memo;
    with_client[15..25].copy_from_slice(&[7u8; 10]);
    assert!(is_bound_memo(&with_client, REALM, CHALLENGE));
}

#[test]
fn transaction_hash_is_keccak_of_the_raw_bytes() {
    assert_eq!(
        hex::encode(transaction_hash(b"")),
        "c5d2460186f7233c927e7db2dcc703c0e500b653ca82273b7bfad8045d85a470"
    );
}

#[test]
fn a_bound_memo_transfer_pays_the_challenge() {
    assert_eq!(
        verify_receipt(&receipt(memo_pair(good_memo())), &expected()),
        Ok(())
    );
    // The twin order does not matter for a lone memo event either.
    let lone = vec![transfer_with_memo(
        CURRENCY,
        PAYER,
        RECIPIENT,
        AMOUNT,
        good_memo(),
    )];
    assert_eq!(verify_receipt(&receipt(lone), &expected()), Ok(()));
}

#[test]
fn an_identical_transfer_with_a_foreign_memo_does_not_hide_the_bound_one() {
    // Same token, sender, recipient and amount; the first carries a non-MPP memo.
    let mut logs = memo_pair([0xab; 32]);
    logs.extend(memo_pair(good_memo()));
    assert_eq!(verify_receipt(&receipt(logs), &expected()), Ok(()));

    // Neither bound: still refused.
    let mut logs = memo_pair([0xab; 32]);
    logs.extend(memo_pair(attribution_memo(REALM, "another-challenge")));
    assert_eq!(
        verify_receipt(&receipt(logs), &expected()),
        Err(ReceiptVerdict::MemoNotBound)
    );
}

#[test]
fn unrelated_logs_around_the_payment_are_ignored() {
    let mut logs = vec![
        transfer(OTHER, PAYER, OTHER, 5),
        RpcLog {
            address: OTHER,
            topics: vec![[9u8; 32]],
            data: vec![1, 2, 3],
        },
    ];
    logs.extend(memo_pair(good_memo()));
    logs.push(transfer(CURRENCY, PAYER, OTHER, 1));
    assert_eq!(verify_receipt(&receipt(logs), &expected()), Ok(()));
}

#[test]
fn rejects_wrong_recipient_amount_currency_and_sender() {
    let pay = |token, from, to, amount| {
        receipt(vec![
            transfer(token, from, to, amount),
            transfer_with_memo(token, from, to, amount, good_memo()),
        ])
    };
    assert_eq!(
        verify_receipt(&pay(CURRENCY, PAYER, RECIPIENT, AMOUNT), &expected()),
        Ok(())
    );
    for (label, bad) in [
        ("recipient", pay(CURRENCY, PAYER, OTHER, AMOUNT)),
        ("amount low", pay(CURRENCY, PAYER, RECIPIENT, AMOUNT - 1)),
        ("amount high", pay(CURRENCY, PAYER, RECIPIENT, AMOUNT + 1)),
        ("currency contract", pay(OTHER, PAYER, RECIPIENT, AMOUNT)),
        // Funds that left some other account do not count for this transaction's sender.
        ("sender", pay(CURRENCY, OTHER, RECIPIENT, AMOUNT)),
    ] {
        assert_eq!(
            verify_receipt(&bad, &expected()),
            Err(ReceiptVerdict::NoMatchingTransfer),
            "{label}"
        );
    }
}

#[test]
fn source_did_address_replaces_the_transaction_sender() {
    // The transfer left PAYER but a relayer (OTHER) sent the transaction.
    let mut paid = receipt(memo_pair(good_memo()));
    paid.from = OTHER;
    assert_eq!(
        verify_receipt(&paid, &expected()),
        Err(ReceiptVerdict::NoMatchingTransfer),
        "without a source the transaction sender must have paid"
    );
    let mut with_source = expected();
    with_source.source = Some(PAYER);
    assert_eq!(verify_receipt(&paid, &with_source), Ok(()));
    // A source that did not pay is refused even though the transaction sender did.
    let paid_by_sender = receipt(memo_pair(good_memo()));
    let mut wrong_source = expected();
    wrong_source.source = Some(OTHER);
    assert_eq!(
        verify_receipt(&paid_by_sender, &wrong_source),
        Err(ReceiptVerdict::NoMatchingTransfer)
    );
}

#[test]
fn rejects_a_reverted_transaction_even_with_matching_logs() {
    let mut reverted = receipt(memo_pair(good_memo()));
    reverted.success = false;
    assert_eq!(
        verify_receipt(&reverted, &expected()),
        Err(ReceiptVerdict::Reverted)
    );
}

#[test]
fn memo_must_exist_and_be_bound_to_this_realm_and_challenge() {
    // No memo at all: a plain Transfer.
    let plain = receipt(vec![transfer(CURRENCY, PAYER, RECIPIENT, AMOUNT)]);
    assert_eq!(
        verify_receipt(&plain, &expected()),
        Err(ReceiptVerdict::MemoNotBound)
    );
    // A non-MPP memo is ignored, so it cannot bind.
    let foreign = receipt(memo_pair([0xAA; 32]));
    assert_eq!(
        verify_receipt(&foreign, &expected()),
        Err(ReceiptVerdict::MemoNotBound)
    );
    // MPP memo for another challenge, and for another realm.
    for (label, memo) in [
        (
            "other challenge",
            attribution_memo(REALM, "some-other-challenge-id"),
        ),
        ("other realm", attribution_memo("evil.example", CHALLENGE)),
    ] {
        assert_eq!(
            verify_receipt(&receipt(memo_pair(memo)), &expected()),
            Err(ReceiptVerdict::MemoNotBound),
            "{label}"
        );
    }
}

#[test]
fn a_memo_transfer_for_another_challenge_wins_over_a_plain_one() {
    // Reference behaviour: memo transfers are preferred, so a transaction cannot carry
    // attribution for two challenges and be accepted for either.
    let mut logs = vec![transfer(CURRENCY, PAYER, RECIPIENT, AMOUNT)];
    logs.push(transfer(CURRENCY, PAYER, RECIPIENT, AMOUNT));
    logs.push(transfer_with_memo(
        CURRENCY,
        PAYER,
        RECIPIENT,
        AMOUNT,
        attribution_memo(REALM, "another-challenge"),
    ));
    assert_eq!(
        verify_receipt(&receipt(logs), &expected()),
        Err(ReceiptVerdict::MemoNotBound)
    );
}

#[test]
fn twin_logs_merge_into_one_transfer_and_distinct_transfers_do_not() {
    // A Transfer immediately followed by its TransferWithMemo is one transfer.
    let pair = receipt(memo_pair(good_memo()));
    assert_eq!(transfer_effects(&pair).len(), 1);
    assert_eq!(transfer_effects(&pair)[0].memo, Some(good_memo()));

    // Memo event first, then plain: still one.
    let reversed = receipt(vec![
        transfer_with_memo(CURRENCY, PAYER, RECIPIENT, AMOUNT, good_memo()),
        transfer(CURRENCY, PAYER, RECIPIENT, AMOUNT),
    ]);
    assert_eq!(transfer_effects(&reversed).len(), 1);

    // Two plain transfers of the same amount are two transfers (no memo to pair).
    let two_plain = receipt(vec![
        transfer(CURRENCY, PAYER, RECIPIENT, AMOUNT),
        transfer(CURRENCY, PAYER, RECIPIENT, AMOUNT),
    ]);
    assert_eq!(transfer_effects(&two_plain).len(), 2);

    // A memo event that is not the twin of the plain event before it (other amount/payee)
    // stays separate.
    let unrelated = receipt(vec![
        transfer(CURRENCY, PAYER, RECIPIENT, AMOUNT),
        transfer_with_memo(CURRENCY, PAYER, RECIPIENT, AMOUNT + 1, good_memo()),
    ]);
    assert_eq!(transfer_effects(&unrelated).len(), 2);

    // A third, non-adjacent log breaks adjacency: the pair is no longer a twin.
    let separated = receipt(vec![
        transfer(CURRENCY, PAYER, RECIPIENT, AMOUNT),
        transfer(OTHER, PAYER, OTHER, 1),
        transfer_with_memo(CURRENCY, PAYER, RECIPIENT, AMOUNT, good_memo()),
    ]);
    assert_eq!(transfer_effects(&separated).len(), 3);

    // The merge matters: two memo pairs in one receipt are two effects, not four.
    let mut both = memo_pair(good_memo());
    both.extend(memo_pair(attribution_memo(REALM, "another-challenge")));
    assert_eq!(transfer_effects(&receipt(both)).len(), 2);
}

#[test]
fn malformed_and_oversized_logs_are_not_transfers() {
    let mut dirty_topic = transfer(CURRENCY, PAYER, RECIPIENT, AMOUNT);
    dirty_topic.topics[2][0] = 1; // non-zero address padding
    let mut short_data = transfer(CURRENCY, PAYER, RECIPIENT, AMOUNT);
    short_data.data.pop();
    let mut wrong_topic_count = transfer(CURRENCY, PAYER, RECIPIENT, AMOUNT);
    wrong_topic_count.topics.push([0; 32]);
    let mut memo_missing_topic =
        transfer_with_memo(CURRENCY, PAYER, RECIPIENT, AMOUNT, good_memo());
    memo_missing_topic.topics.pop();
    let no_topics = RpcLog {
        address: CURRENCY,
        topics: vec![],
        data: amount_word(AMOUNT),
    };
    for (label, log) in [
        ("address padding", dirty_topic),
        ("short data", short_data),
        ("extra topic", wrong_topic_count),
        ("memo topic missing", memo_missing_topic),
        ("no topics", no_topics),
    ] {
        assert!(transfer_effects(&receipt(vec![log])).is_empty(), "{label}");
    }
}

#[test]
fn an_amount_above_u128_never_satisfies_a_request() {
    // 2^128 + AMOUNT has the same low 16 bytes as AMOUNT: it must not match.
    let mut data = amount_word(AMOUNT);
    data[15] = 1;
    let mut log = transfer(CURRENCY, PAYER, RECIPIENT, AMOUNT);
    log.data = data.clone();
    let mut memo_log = transfer_with_memo(CURRENCY, PAYER, RECIPIENT, AMOUNT, good_memo());
    memo_log.data = data;
    assert_eq!(
        verify_receipt(&receipt(vec![log, memo_log]), &expected()),
        Err(ReceiptVerdict::NoMatchingTransfer)
    );
}

#[test]
fn requests_at_the_u128_ceiling_match_exactly() {
    let mut expected = expected();
    expected.amount = u128::MAX;
    let pay = vec![
        transfer(CURRENCY, PAYER, RECIPIENT, u128::MAX),
        transfer_with_memo(CURRENCY, PAYER, RECIPIENT, u128::MAX, good_memo()),
    ];
    assert_eq!(verify_receipt(&receipt(pay), &expected), Ok(()));
}
