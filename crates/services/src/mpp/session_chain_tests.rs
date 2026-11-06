use crate::mpp::abi::settle_calldata;
use crate::mpp::ports::{RpcError, RpcReceipt};
use crate::mpp::session_chain::*;
use crate::mpp::session_vectors::Vectors;
use crate::mpp::signer::LocalSigner;
use crate::mpp::tempo_tx::TxError;
use crate::mpp::testing::{FakeTempo, SendMode};

#[test]
fn only_a_matching_revert_is_definitive_with_the_current_rpc_port() {
    assert!(BroadcastError::Reverted.definitively_failed());
    for error in [
        BroadcastError::NotFound,
        BroadcastError::ReceiptMismatch,
        BroadcastError::Rpc(RpcError::Unavailable),
        BroadcastError::Rpc(RpcError::InvalidResponse),
        BroadcastError::Rpc(RpcError::Rejected),
    ] {
        assert!(!error.definitively_failed());
    }
}

async fn vector_tx(rpc: &FakeTempo) -> SignedPayeeTx {
    let v = Vectors::load();
    rpc.set_gas_price(Ok(v.int("settle.fields.maxFeePerGas")));
    let signer = LocalSigner::new(v.secret_key("payeeKey"));
    let calldata = settle_calldata(
        &v.descriptor(),
        v.int("voucher.cumulativeAmount"),
        &v.bytes("voucher.signature"),
    )
    .unwrap();
    build_payee_tx(
        PayeeChain {
            chain_id: v.chain_id(),
            escrow: v.escrow(),
            fee_token: v.fixed("token"),
        },
        &signer,
        rpc,
        calldata,
        u64::try_from(v.int("settle.fields.validBefore")).unwrap() - 25,
    )
    .await
    .unwrap()
}

fn receipt(tx: &SignedPayeeTx, success: bool) -> RpcReceipt {
    RpcReceipt {
        tx_hash: tx.tx_hash,
        block_number: 1,
        success,
        from: Vectors::load().fixed("payee"),
        logs: vec![],
    }
}

#[tokio::test]
async fn payee_settle_reproduces_the_signed_vector_byte_for_byte() {
    let rpc = FakeTempo::default();
    let tx = vector_tx(&rpc).await;
    let v = Vectors::load();
    assert_eq!(tx.raw, v.bytes("settle.serialized"));
    assert_eq!(tx.tx_hash, v.fixed::<32>("settle.txHash"));
    assert_eq!(
        u128::from(tx.valid_before),
        v.int("settle.fields.validBefore")
    );
    assert_eq!(format!("{tx:?}"), "SignedPayeeTx { <redacted> }");
}

#[tokio::test]
async fn broadcast_success_and_rejected_known_transaction() {
    for mode in [SendMode::Normal, SendMode::Reject] {
        let rpc = FakeTempo::default();
        let tx = vector_tx(&rpc).await;
        let expected = receipt(&tx, true);
        rpc.mine(expected.clone());
        rpc.set_send_mode(mode);
        assert_eq!(broadcast(&rpc, &tx).await, Ok(expected));
        assert_eq!(rpc.send_calls(), 1);
        assert_eq!(rpc.lookup_calls(), usize::from(mode == SendMode::Reject));
    }
}

#[tokio::test]
async fn broadcast_rejects_reverts_and_wrong_hashes_on_both_paths() {
    for mode in [SendMode::Normal, SendMode::Reject] {
        for mismatch in [false, true] {
            let rpc = FakeTempo::default();
            let tx = vector_tx(&rpc).await;
            let mut answer = receipt(&tx, mismatch);
            if mismatch {
                answer.tx_hash = [0x55; 32];
            }
            rpc.set_send_receipt(Some(answer.clone()));
            rpc.mined_alias(tx.tx_hash, answer);
            rpc.set_send_mode(mode);
            assert_eq!(
                broadcast(&rpc, &tx).await,
                Err(if mismatch {
                    BroadcastError::ReceiptMismatch
                } else {
                    BroadcastError::Reverted
                })
            );
        }
    }
}

#[tokio::test]
async fn broadcast_lookup_is_once_and_only_for_rejection() {
    let rpc = FakeTempo::default();
    let tx = vector_tx(&rpc).await;
    rpc.set_send_mode(SendMode::Unavailable);
    assert_eq!(
        broadcast(&rpc, &tx).await,
        Err(BroadcastError::Rpc(RpcError::Unavailable))
    );
    assert_eq!(rpc.lookup_calls(), 0);
    rpc.set_send_mode(SendMode::Reject);
    assert_eq!(broadcast(&rpc, &tx).await, Err(BroadcastError::NotFound));
    assert_eq!(rpc.lookup_calls(), 1);
    rpc.set_lookup_error(Some(RpcError::InvalidResponse));
    assert_eq!(
        broadcast(&rpc, &tx).await,
        Err(BroadcastError::Rpc(RpcError::InvalidResponse))
    );
    assert_eq!(rpc.lookup_calls(), 2);
}

#[tokio::test]
async fn build_failures_have_closed_causes() {
    let v = Vectors::load();
    let chain = PayeeChain {
        chain_id: v.chain_id(),
        escrow: v.escrow(),
        fee_token: v.fixed("token"),
    };
    let signer = LocalSigner::new(v.secret_key("payeeKey"));
    let rpc = FakeTempo::default();
    assert_eq!(
        build_payee_tx(chain, &signer, &rpc, vec![], u64::MAX).await,
        Err(PayeeTxError::Transaction(TxError::TimeOverflow))
    );
    rpc.set_gas_price(Err(RpcError::Unavailable));
    assert_eq!(
        build_payee_tx(chain, &signer, &rpc, vec![], 1).await,
        Err(PayeeTxError::GasPrice(RpcError::Unavailable))
    );
}
