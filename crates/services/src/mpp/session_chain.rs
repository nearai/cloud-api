//! Shared signing and broadcast path for payee settlements and closes.

use crate::mpp::ports::{RpcError, RpcReceipt, TempoRpc, TempoSigner};
use crate::mpp::secp256k1::SigError;
use crate::mpp::tempo_tx::{tx_hash, Call, TempoTx, TxError};

/// Chain and fee currency used by the payee.
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct PayeeChain {
    /// Tempo chain id.
    pub chain_id: u64,
    /// TIP-1034 channel reserve precompile.
    pub escrow: [u8; 20],
    /// Token in which transaction fees are paid.
    pub fee_token: [u8; 20],
}

/// Gas budget for one payee settlement or close.
pub const PAYEE_GAS_LIMIT: u64 = 2_000_000;

/// Signed transaction and the identity needed to reconcile a lost broadcast reply.
#[derive(Clone, PartialEq, Eq)]
pub struct SignedPayeeTx {
    /// Full signed Tempo encoding, ready to broadcast.
    pub raw: Vec<u8>,
    /// Keccak256 of `raw`.
    pub tx_hash: [u8; 32],
    /// Expiring nonce deadline, in Unix seconds.
    pub valid_before: u64,
}

impl std::fmt::Debug for SignedPayeeTx {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("SignedPayeeTx { <redacted> }")
    }
}

/// A build failure containing only closed, input-free causes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum PayeeTxError {
    /// Gas price could not be read.
    #[error("payee gas price unavailable")]
    GasPrice(#[source] RpcError),
    /// Expiring nonce could not be encoded.
    #[error("payee transaction invalid")]
    Transaction(#[source] TxError),
    /// Payee signing failed.
    #[error("payee signing failed")]
    Signing(#[source] SigError),
}

impl PayeeTxError {
    /// Closed outcome code safe for logs.
    pub fn code(self) -> &'static str {
        match self {
            Self::GasPrice(_) => "gas_price_failed",
            Self::Transaction(_) => "transaction_failed",
            Self::Signing(_) => "signing_failed",
        }
    }
}

/// Build one zero-value call to the escrow, using the node's gas price for both fee
/// fields, the configured fee token and an expiring nonce. RPC implementations must
/// bound each request (as `HttpTempoRpc` does).
pub async fn build_payee_tx(
    chain: PayeeChain,
    signer: &dyn TempoSigner,
    rpc: &dyn TempoRpc,
    calldata: Vec<u8>,
    now_unix: u64,
) -> Result<SignedPayeeTx, PayeeTxError> {
    let price = rpc.gas_price().await.map_err(PayeeTxError::GasPrice)?;
    let mut tx = TempoTx {
        chain_id: chain.chain_id,
        max_priority_fee_per_gas: price,
        max_fee_per_gas: price,
        gas_limit: PAYEE_GAS_LIMIT,
        calls: vec![Call {
            to: chain.escrow,
            value: 0,
            input: calldata,
        }],
        nonce_key: [0; 32],
        nonce: 0,
        valid_before: None,
        valid_after: None,
        fee_token: Some(chain.fee_token),
    };
    tx.set_expiring_nonce(now_unix)
        .map_err(PayeeTxError::Transaction)?;
    let valid_before = tx
        .valid_before
        .ok_or(PayeeTxError::Transaction(TxError::TimeOverflow))?;
    let signature = signer
        .sign(&tx.signing_hash())
        .map_err(PayeeTxError::Signing)?;
    let raw = tx.serialize_signed(&signature);
    Ok(SignedPayeeTx {
        tx_hash: tx_hash(&raw),
        raw,
        valid_before,
    })
}

/// A broadcast failure, without transaction data.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum BroadcastError {
    /// Sending or the one receipt lookup failed.
    #[error("payee broadcast failed")]
    Rpc(#[source] RpcError),
    /// A rejected transaction was not found by the receipt lookup.
    #[error("payee receipt not found")]
    NotFound,
    /// The receipt belongs to a different transaction.
    #[error("payee receipt mismatch")]
    ReceiptMismatch,
    /// The transaction was mined unsuccessfully.
    #[error("payee transaction reverted")]
    Reverted,
}

impl BroadcastError {
    /// Whether a matching receipt proves that this transaction failed. The RPC
    /// port conflates a refusal with "already known", so NotFound can still mean
    /// an unmined transaction and must be reconciled through expiry.
    pub fn definitively_failed(self) -> bool {
        matches!(self, Self::Reverted)
    }

    /// Closed outcome code safe for logs.
    pub fn code(self) -> &'static str {
        match self {
            Self::Rpc(error) => error.kind(),
            Self::NotFound => "receipt_not_found",
            Self::ReceiptMismatch => "receipt_mismatch",
            Self::Reverted => "reverted",
        }
    }
}

/// Broadcast synchronously. A node rejection gets exactly one lookup by the signed
/// transaction hash, covering a transaction the node already knows. Success requires
/// a matching, successful receipt. Transport failures are returned to the caller.
pub async fn broadcast(
    rpc: &dyn TempoRpc,
    tx: &SignedPayeeTx,
) -> Result<RpcReceipt, BroadcastError> {
    let receipt = match rpc.send_raw_transaction_sync(&tx.raw).await {
        Ok(receipt) => receipt,
        Err(RpcError::Rejected) => rpc
            .get_transaction_receipt(tx.tx_hash)
            .await
            .map_err(BroadcastError::Rpc)?
            .ok_or(BroadcastError::NotFound)?,
        Err(error) => return Err(BroadcastError::Rpc(error)),
    };
    if receipt.tx_hash != tx.tx_hash {
        return Err(BroadcastError::ReceiptMismatch);
    }
    if !receipt.success {
        return Err(BroadcastError::Reverted);
    }
    Ok(receipt)
}
