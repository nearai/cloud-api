use super::*;

fn top_up_input(descriptor: &ChannelDescriptor, amount: u128) -> Vec<u8> {
    let mut input = vec![0xdc, 0x48, 0x47, 0x1e];
    for address in [
        descriptor.payer,
        descriptor.payee,
        descriptor.operator,
        descriptor.token,
    ] {
        input.extend(abi::address_word(&address));
    }
    input.extend(descriptor.salt);
    input.extend(abi::address_word(&descriptor.authorized_signer));
    input.extend(descriptor.expiring_nonce_hash);
    input.extend(abi::uint_word(amount));
    input
}

fn top_up(raw: &[u8], additional: u128) -> Value {
    let v = Vectors::load();
    json!({"action":"topUp","type":"transaction", "channelId":v.text("channelId"),
        "descriptor":descriptor_json(&v.descriptor()), "additionalDeposit":additional.to_string(), "transaction":hex_prefixed(raw)})
}

#[tokio::test]
async fn top_up_validates_call_descriptor_sender_amount_and_chain_before_send() {
    for (case, expected) in [
        (0, TopUpReason::Selector),
        (1, TopUpReason::Descriptor),
        (2, TopUpReason::Sender),
        (3, TopUpReason::Amount),
        (4, TopUpReason::ChainId),
        (5, TopUpReason::CallCount),
        (6, TopUpReason::Target),
        (7, TopUpReason::Value),
        (8, TopUpReason::Calldata),
        (9, TopUpReason::ExpiringNonce),
    ] {
        let f = Fixture::new();
        f.seed().await;
        let raw = resigned(
            |tx| {
                let mut d = Vectors::load().descriptor();
                if case == 1 {
                    d.salt[0] ^= 1;
                }
                tx.calls[0].input = top_up_input(&d, if case == 3 { 201 } else { 200 });
                match case {
                    0 => tx.calls[0].input[0] ^= 1,
                    4 => tx.chain_id += 1,
                    5 => tx.calls.push(tx.calls[0].clone()),
                    6 => tx.calls[0].to[0] ^= 1,
                    7 => tx.calls[0].value = 1,
                    8 => {
                        tx.calls[0].input.pop();
                    }
                    9 => tx.nonce_key = [0; 32],
                    _ => {}
                }
            },
            case != 2,
        );
        let credential = f.credential(top_up(&raw, 200)).await;
        assert_eq!(
            f.service.top_up(&credential).await.err(),
            Some(MppError::TopUpRejected(expected)),
            "case {case}"
        );
        assert_eq!(f.rpc.send_calls(), 0);
        assert_eq!(f.record().await.deposit, Vectors::load().int("deposit"));
    }
}

#[tokio::test]
async fn valid_top_up_broadcasts_and_refreshes_deposit_without_spending() {
    let f = Fixture::new();
    f.seed().await;
    let raw = resigned(
        |tx| tx.calls[0].input = top_up_input(&Vectors::load().descriptor(), 200),
        true,
    );
    f.rpc.mine(receipt(&raw));
    set_chain(
        &f.rpc,
        ChannelState {
            deposit: Vectors::load().int("deposit") + 200,
            ..state()
        },
    );
    let credential = f.credential(top_up(&raw, 200)).await;
    let receipt = f.service.top_up(&credential).await.unwrap().receipt;
    assert_eq!(receipt.tx_hash, Some(tx_hash(&raw)));
    assert_eq!(receipt.accepted_cumulative, INITIAL);
    let row = f.record().await;
    assert_eq!((row.deposit, row.spent, row.reserved), (5_000_200, 0, 0));
    assert_eq!(f.rpc.send_calls(), 1);
}

#[tokio::test]
async fn top_up_revert_transport_and_chain_read_failure_preserve_ledger() {
    for case in 0..3 {
        let f = Fixture::new();
        f.seed().await;
        let before = f.record().await;
        let raw = resigned(
            |tx| tx.calls[0].input = top_up_input(&Vectors::load().descriptor(), 200),
            true,
        );
        match case {
            0 => {
                let mut mined = receipt(&raw);
                mined.success = false;
                f.rpc.mine(mined);
            }
            1 => f
                .rpc
                .set_send_mode(crate::mpp::testing::SendMode::Unavailable),
            _ => {
                f.rpc.mine(receipt(&raw));
                f.rpc
                    .set_call_handler(Box::new(|_, _| Err(RpcError::Unavailable)));
            }
        }
        let credential = f.credential(top_up(&raw, 200)).await;
        assert!(f.service.top_up(&credential).await.is_err());
        assert_eq!(f.record().await, before, "case {case}");
        assert_eq!(f.repo.reservation_count(), 0);
        assert_eq!(f.rpc.eth_calls(), usize::from(case == 2));
    }
}

#[tokio::test]
async fn top_up_replay_uses_chain_deposit_without_double_crediting() {
    let f = Fixture::new();
    f.seed().await;
    let raw = resigned(
        |tx| tx.calls[0].input = top_up_input(&Vectors::load().descriptor(), 200),
        true,
    );
    f.rpc.mine(receipt(&raw));
    set_chain(
        &f.rpc,
        ChannelState {
            deposit: Vectors::load().int("deposit") + 200,
            ..state()
        },
    );
    let credential = f.credential(top_up(&raw, 200)).await;
    f.service.top_up(&credential).await.unwrap();
    f.rpc.set_send_mode(crate::mpp::testing::SendMode::Reject);
    f.service.top_up(&credential).await.unwrap();
    let row = f.record().await;
    assert_eq!(row.deposit, Vectors::load().int("deposit") + 200);
    assert_eq!((row.spent, row.reserved, row.units), (0, 0, 0));
}
