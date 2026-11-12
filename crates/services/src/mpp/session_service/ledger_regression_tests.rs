use super::tests::{voucher, Fixture};
use crate::mpp::error::{MppError, UnavailableReason};
use crate::mpp::ports::{ChannelRepository, RpcError};
use crate::mpp::session_vectors::Vectors;
use crate::mpp::session_wire::hex_prefixed;
use chrono::Utc;
use serde_json::json;
use std::sync::Arc;
use uuid::Uuid;

const INITIAL: u128 = 1_250_000;

fn open_payload() -> serde_json::Value {
    let mut payload = voucher(INITIAL);
    payload["action"] = json!("open");
    payload["type"] = json!("transaction");
    payload["transaction"] = json!(Vectors::load().text("open.serialized"));
    payload
}

async fn wait_for_sends(rpc: &crate::mpp::testing::FakeTempo, count: usize) {
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        while rpc.send_calls() < count {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn hanging_open_does_not_block_another_open_in_the_same_stripe() {
    let f = Fixture::new();
    let v = Vectors::load();
    let original = v.fixed::<32>("channelId");
    let (base_tx, _) = crate::mpp::tempo_tx::decode_signed(&v.bytes("open.serialized")).unwrap();
    let (descriptor, other_id, tx) = (0_u32..65_536)
        .find_map(|salt| {
            let mut descriptor = v.descriptor();
            descriptor.salt[..4].copy_from_slice(&salt.to_be_bytes());
            let mut tx = base_tx.clone();
            tx.calls[0].input = crate::mpp::abi::open_calldata(
                &descriptor.payee,
                &descriptor.operator,
                &descriptor.token,
                v.int("deposit"),
                &descriptor.salt,
                &descriptor.authorized_signer,
            )
            .unwrap();
            descriptor.expiring_nonce_hash =
                crate::mpp::tempo_tx::expiring_nonce_hash(&tx, &descriptor.payer);
            let id = crate::mpp::channel::channel_id(&descriptor, &v.escrow(), v.chain_id());
            (id != original && id.first() == original.first()).then_some((descriptor, id, tx))
        })
        .expect("distinct channel sharing the removed stripe");
    let key = v.secret_key("payerKey");
    let raw =
        tx.serialize_signed(&crate::mpp::secp256k1::sign_digest(&key, &tx.signing_hash()).unwrap());
    let digest =
        crate::mpp::eip712::voucher_digest(&v.escrow(), v.chain_id(), &other_id, INITIAL).unwrap();
    let other = f.credential(json!({"action":"open", "type":"transaction", "channelId":hex_prefixed(&other_id),
        "descriptor":crate::mpp::session_wire::descriptor_json(&descriptor), "cumulativeAmount":INITIAL.to_string(),
        "signature":hex_prefixed(&crate::mpp::secp256k1::sign_digest(&key, &digest).unwrap()), "transaction":hex_prefixed(&raw)})).await;
    let service = Arc::new(super::tests::service(
        super::tests::config(),
        f.repo.clone(),
        f.rpc.clone(),
    ));
    let credential = f.credential(open_payload()).await;
    f.rpc.set_send_mode(crate::mpp::testing::SendMode::Hang);
    let first = {
        let service = service.clone();
        let credential = credential.clone();
        tokio::spawn(async move { service.open(&credential).await })
    };
    wait_for_sends(&f.rpc, 1).await;
    f.rpc.mine(super::tests::receipt(&raw));
    f.rpc.set_send_mode(crate::mpp::testing::SendMode::Normal);
    f.rpc.set_call_handler(Box::new(move |to, input| {
        let v = Vectors::load();
        assert_eq!(to, v.escrow());
        assert_eq!(
            input,
            crate::mpp::abi::get_channel_state_calldata(&other_id)
        );
        Ok([
            crate::mpp::abi::uint_word(0),
            crate::mpp::abi::uint_word(v.int("deposit")),
            crate::mpp::abi::uint_word(0),
        ]
        .concat())
    }));
    let second =
        tokio::time::timeout(std::time::Duration::from_secs(1), service.open(&other)).await;
    first.abort();
    let _ = first.await;
    second
        .expect("independent open does not wait for the hanging RPC")
        .expect("signed independent open succeeds");
    assert_eq!(f.rpc.send_calls(), 2);
}

#[tokio::test]
#[ignore = "Run alone because it saturates the process-wide management semaphore"]
async fn eight_hanging_management_actions_reject_ninth_and_restore_capacity_on_cancel() {
    let f = Fixture::new();
    let service = Arc::new(super::tests::service(
        super::tests::config(),
        f.repo.clone(),
        f.rpc.clone(),
    ));
    let credential = f.credential(open_payload()).await;
    f.rpc.set_send_mode(crate::mpp::testing::SendMode::Hang);
    let mut actions = Vec::new();
    for count in 1..=8 {
        let service = service.clone();
        let credential = credential.clone();
        actions.push(tokio::spawn(async move { service.open(&credential).await }));
        wait_for_sends(&f.rpc, count).await;
    }
    let result = tokio::time::timeout(std::time::Duration::from_secs(1), service.open(&credential))
        .await
        .unwrap();
    assert_eq!(
        result.err(),
        Some(MppError::Unavailable(UnavailableReason::ManagementBusy))
    );
    assert_eq!(f.rpc.send_calls(), 8);
    for action in actions {
        action.abort();
        let _ = action.await;
    }
    f.rpc.mine(super::tests::receipt(
        &Vectors::load().bytes("open.serialized"),
    ));
    f.rpc.set_send_mode(crate::mpp::testing::SendMode::Normal);
    assert!(service.open(&credential).await.is_ok());
    assert_eq!(f.rpc.send_calls(), 9);
}

#[tokio::test]
async fn failed_refresh_retry_remains_fail_closed_without_reservation() {
    let f = Fixture::new();
    f.seed().await;
    f.rpc
        .set_call_handler(Box::new(|_, _| Err(RpcError::Unavailable)));
    let credential = f.credential(voucher(INITIAL)).await;
    for _ in 0..2 {
        assert_eq!(
            f.service
                .voucher(&credential, 10, Uuid::new_v4())
                .await
                .err(),
            Some(MppError::Unavailable(UnavailableReason::Rpc))
        );
        assert_eq!(f.repo.reservation_count(), 0);
    }
    assert_eq!(f.rpc.eth_calls(), 2);
}

#[tokio::test]
async fn fresh_successful_chain_read_does_not_claim_or_write_again() {
    let f = Fixture::new();
    f.seed().await;
    let id = Vectors::load().fixed("channelId");
    let before = f
        .repo
        .record_chain_state(&id, super::tests::state(), Utc::now())
        .await
        .unwrap()
        .unwrap();
    let before_clock = f.repo.now();
    let after = f.service.refresh(before.clone()).await.unwrap();
    assert_eq!(after, before);
    assert_eq!(f.record().await, before);
    assert_eq!(f.rpc.eth_calls(), 0);
    assert_eq!(f.repo.now(), before_clock);
    assert_eq!(f.repo.chain_read_claim_count(), 0);
}

#[tokio::test]
async fn incomplete_usage_charges_full_reservation_without_inventing_units() {
    let f = Fixture::new();
    f.seed().await;
    let credential = f.credential(voucher(INITIAL)).await;
    let reservation = f
        .service
        .voucher(&credential, 100, Uuid::new_v4())
        .await
        .unwrap();
    let receipt = f.service.charge_reserved(&reservation).await.unwrap();
    assert_eq!((receipt.spent, receipt.units), (100, 0));
    let row = f.record().await;
    assert_eq!((row.spent, row.reserved, row.units), (100, 0, 0));
    assert_eq!(f.repo.reservation_count(), 0);
    assert_eq!(
        f.service.charge_reserved(&reservation).await.err(),
        Some(MppError::ReservationMissing)
    );
}

#[tokio::test]
async fn invalid_voucher_signature_is_rejected_before_refresh_claim_or_rpc() {
    let f = Fixture::new();
    f.seed().await;
    let before = f.record().await;
    let mut payload = voucher(INITIAL + 100);
    payload["signature"] = json!(hex_prefixed(&[0; 65]));
    let credential = f.credential(payload).await;
    assert!(f
        .service
        .voucher(&credential, 10, Uuid::new_v4())
        .await
        .is_err());
    assert_eq!(f.record().await, before);
    assert_eq!(f.rpc.eth_calls(), 0);
}

#[tokio::test]
async fn insufficient_request_does_not_promote_incoming_voucher() {
    let f = Fixture::new();
    f.seed().await;
    let credential = f.credential(voucher(INITIAL + 100)).await;
    assert!(matches!(
        f.service
            .voucher(&credential, INITIAL + 101, Uuid::new_v4())
            .await,
        Err(MppError::VoucherTooLow(_))
    ));
    let row = f.record().await;
    assert_eq!(row.highest_cumulative, INITIAL);
    assert!(row.retained_vouchers.is_empty());
    assert_eq!((row.spent, row.reserved), (0, 0));
}
