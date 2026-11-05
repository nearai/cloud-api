//! Payment-channel ledger of the MPP `session` intent (`mpp_channels`,
//! `mpp_channel_reservations`).
//!
//! The tables hold no organization, workspace, request content or API key. The schema is
//! owned by `nearai/cloud-platform` (`V0100__mpp_channels.sql`); cloud-api only reads and
//! writes it, with the `cloud_api_responses` role.
//!
//! Every read-then-write is one transaction that first locks the channel row with
//! `SELECT ... FOR UPDATE`, so two instances serving one channel never spend the same
//! balance. The arithmetic runs in Rust on `u128` against the locked row, mirroring
//! `InMemoryChannelRepository`, which is the reference for the rules. Amounts are
//! `NUMERIC(40,0)`: bound as text through `$n::TEXT::NUMERIC` and read back as `::TEXT`
//! (the workspace has no decimal crate).
//!
//! Operations whose retry after a lost reply would answer differently (`reserve`, `charge`,
//! `release`, `accept_voucher`, `mark_closing`, the lease claim) are not retried; the
//! idempotent ones are. No error carries a row, an address, a signature or a hash.

use std::str::FromStr;

use crate::pool::DbPool;
use crate::repositories::utils::map_db_error;
use crate::retry_db;
use anyhow::Context;
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use deadpool_postgres::Transaction;
use serde_json::{json, Value};
use services::common::RepositoryError;
use services::mpp::session::{ChannelDescriptor, ChannelState, SIGNATURE_LEN};
use services::mpp::{
    ChannelLifecycle, ChannelRecord, ChannelRepository, ChargeOutcome, CloseTransition, NewChannel,
    PendingClose, ReserveOutcome, RetainedVoucher, VoucherOutcome, MAX_RETAINED_VOUCHERS,
};
use tokio_postgres::Row;
use uuid::Uuid;

/// Every column of a channel the ledger reads, in the order `record_from_row` expects.
/// Amounts come back as text.
const COLUMNS: &str = "\
channel_id, chain_id, escrow, payer, payee, operator, token, salt, authorized_signer, \
expiring_nonce_hash, deposit::TEXT, settled_on_chain::TEXT, highest_cumulative::TEXT, \
highest_signature, retained_vouchers, spent::TEXT, reserved::TEXT, units, state, \
close_requested_at, pending_close, last_chain_read_at, last_settlement_at, \
last_settlement_spent::TEXT, updated_at";

const INSERT_SQL: &str = "\
INSERT INTO mpp_channels (channel_id, chain_id, escrow, payer, payee, operator, token, salt, \
authorized_signer, expiring_nonce_hash, deposit, settled_on_chain, highest_cumulative, \
highest_signature, state) \
VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11::TEXT::NUMERIC, $12::TEXT::NUMERIC, \
$13::TEXT::NUMERIC, $14, 'open') \
ON CONFLICT (channel_id) DO NOTHING \
RETURNING updated_at";

const GET_SQL: &str = "SELECT {COLUMNS} FROM mpp_channels WHERE channel_id = $1";

/// The channel row, locked until the transaction ends.
const LOCK_SQL: &str = "SELECT {COLUMNS} FROM mpp_channels WHERE channel_id = $1 FOR UPDATE";

/// Write back every column the ledger moves. `updated_at` takes `clock_timestamp()`, not
/// `NOW()`: a transaction that waited for the row lock must not stamp a time before the
/// write it waited for. `$16` clears the settlement lease.
const STORE_SQL: &str = "\
UPDATE mpp_channels SET deposit = $2::TEXT::NUMERIC, settled_on_chain = $3::TEXT::NUMERIC, \
highest_cumulative = $4::TEXT::NUMERIC, highest_signature = $5, retained_vouchers = $6, \
spent = $7::TEXT::NUMERIC, reserved = $8::TEXT::NUMERIC, units = $9, state = $10, \
close_requested_at = $11, pending_close = $12, last_chain_read_at = $13, \
last_settlement_at = $14, last_settlement_spent = $15::TEXT::NUMERIC, \
settlement_lease_until = CASE WHEN $16 THEN NULL ELSE settlement_lease_until END, \
updated_at = clock_timestamp() \
WHERE channel_id = $1 \
RETURNING updated_at";

const INSERT_RESERVATION_SQL: &str = "\
INSERT INTO mpp_channel_reservations (request_id, channel_id, amount) \
VALUES ($1, $2, $3::TEXT::NUMERIC)";

/// Which channel a reservation belongs to, read before the channel is locked.
const RESERVATION_CHANNEL_SQL: &str =
    "SELECT channel_id FROM mpp_channel_reservations WHERE request_id = $1";

/// Taken after the channel lock, so a reservation another transaction already settled
/// returns no row.
const TAKE_RESERVATION_SQL: &str = "\
DELETE FROM mpp_channel_reservations WHERE request_id = $1 RETURNING amount::TEXT";

const FINALIZE_SQL: &str = "\
UPDATE mpp_channels SET state = 'finalized', pending_close = NULL, \
settled_on_chain = GREATEST(settled_on_chain, $2::TEXT::NUMERIC), \
updated_at = clock_timestamp() \
WHERE channel_id = $1";

const CLEAR_CLOSING_SQL: &str = "\
UPDATE mpp_channels SET state = 'open', pending_close = NULL, updated_at = clock_timestamp() \
WHERE channel_id = $1 AND state = 'closing'";

/// One atomic statement: the lease is taken only by the instance whose update still sees it
/// free (a concurrent claimant re-evaluates the condition after the first commits).
const CLAIM_LEASE_SQL: &str = "\
UPDATE mpp_channels SET settlement_lease_until = $3, updated_at = clock_timestamp() \
WHERE channel_id = $1 AND (settlement_lease_until IS NULL OR settlement_lease_until < $2) \
RETURNING {COLUMNS}";

const RELEASE_LEASE_SQL: &str = "\
UPDATE mpp_channels SET settlement_lease_until = NULL, updated_at = clock_timestamp() \
WHERE channel_id = $1";

/// Through `idx_mpp_channels_open`.
const LIST_FIRST_SQL: &str = "\
SELECT {COLUMNS} FROM mpp_channels WHERE state <> 'finalized' \
ORDER BY channel_id LIMIT $1";
const LIST_AFTER_SQL: &str = "\
SELECT {COLUMNS} FROM mpp_channels WHERE state <> 'finalized' AND channel_id > $1 \
ORDER BY channel_id LIMIT $2";

/// Channels with a stale reservation, after a cursor: a channel another instance holds
/// locked is skipped (see `RELEASE_STALE_LOCK_SQL`), so paging is by key, not by retrying.
const STALE_CHANNELS_SQL: &str = "\
SELECT DISTINCT channel_id FROM mpp_channel_reservations \
WHERE created_at < $1 AND channel_id > $2 ORDER BY channel_id LIMIT $3";
const RELEASE_STALE_LOCK_SQL: &str = "\
SELECT {COLUMNS} FROM mpp_channels WHERE channel_id = $1 FOR UPDATE SKIP LOCKED";
const TAKE_STALE_SQL: &str = "\
DELETE FROM mpp_channel_reservations WHERE channel_id = $1 AND created_at < $2 \
RETURNING amount::TEXT";

/// Finalized channels after a cursor, then the delete of those old enough. The scan walks
/// `idx_mpp_channels_finalized` in key order and is bounded by the batch; `SKIP LOCKED`
/// lets two instances purge at once. Reservations go with their channel (`ON DELETE
/// CASCADE`).
const FINALIZED_PAGE_SQL: &str = "\
SELECT channel_id FROM mpp_channels WHERE state = 'finalized' AND channel_id > $1 \
ORDER BY channel_id LIMIT $2";
const PURGE_SQL: &str = "\
DELETE FROM mpp_channels WHERE channel_id IN ( \
SELECT channel_id FROM mpp_channels WHERE channel_id = ANY($1) AND state = 'finalized' \
AND updated_at < $2 FOR UPDATE SKIP LOCKED)";

/// Rows examined per statement: small enough that no statement holds its locks for long.
const SCAN_BATCH: i64 = 500;
/// Statements per call; anything left goes at the next call.
const MAX_BATCHES: u32 = 200;
/// Longest page `list_active` returns, whatever the caller asks.
const MAX_LIST_LIMIT: usize = 1000;

pub struct MppChannelRepository {
    pool: DbPool,
}

impl MppChannelRepository {
    pub fn new(pool: DbPool) -> Self {
        Self { pool }
    }

    async fn client(&self) -> Result<deadpool_postgres::Object, RepositoryError> {
        self.pool
            .get()
            .await
            .context("Failed to get database connection")
            .map_err(RepositoryError::PoolError)
    }
}

/// `COLUMNS` into a statement's `{COLUMNS}` placeholder.
fn with_columns(sql: &str) -> String {
    sql.replace("{COLUMNS}", COLUMNS)
}

fn conversion(what: &'static str) -> RepositoryError {
    RepositoryError::DataConversionError(anyhow::anyhow!("channel row: {what}"))
}

fn amount_text(amount: u128) -> String {
    amount.to_string()
}

fn parse_amount(text: &str) -> Result<u128, RepositoryError> {
    u128::from_str(text).map_err(|_| conversion("amount"))
}

/// Whether a page filled its batch, so another page may follow.
fn batch_is_full(len: usize) -> bool {
    i64::try_from(len).map_or(true, |len| len >= SCAN_BATCH)
}

fn to_i64(value: u64) -> i64 {
    i64::try_from(value).unwrap_or(i64::MAX)
}

fn fixed<const N: usize>(bytes: Vec<u8>, what: &'static str) -> Result<[u8; N], RepositoryError> {
    bytes.try_into().map_err(|_| conversion(what))
}

fn hex_signature(signature: &[u8; SIGNATURE_LEN]) -> String {
    format!("0x{}", hex::encode(signature))
}

fn vouchers_json(vouchers: &[RetainedVoucher]) -> Value {
    Value::Array(
        vouchers
            .iter()
            .map(|voucher| {
                json!({
                    "cumulative": amount_text(voucher.cumulative),
                    "signature": hex_signature(&voucher.signature),
                })
            })
            .collect(),
    )
}

fn vouchers_from_json(value: &Value) -> Result<Vec<RetainedVoucher>, RepositoryError> {
    let items = value.as_array().ok_or_else(|| conversion("vouchers"))?;
    items
        .iter()
        .map(|item| {
            let cumulative = item
                .get("cumulative")
                .and_then(Value::as_str)
                .ok_or_else(|| conversion("voucher"))
                .and_then(parse_amount)?;
            let signature = item
                .get("signature")
                .and_then(Value::as_str)
                .and_then(|text| text.strip_prefix("0x"))
                .and_then(|digits| hex::decode(digits).ok())
                .ok_or_else(|| conversion("voucher"))
                .and_then(|bytes| fixed::<SIGNATURE_LEN>(bytes, "voucher"))?;
            Ok(RetainedVoucher {
                cumulative,
                signature,
            })
        })
        .collect()
}

fn pending_json(pending: &PendingClose) -> Value {
    json!({
        "txHash": format!("0x{}", hex::encode(pending.tx_hash)),
        "cumulative": amount_text(pending.cumulative),
        "capture": amount_text(pending.capture),
        "validBefore": pending.valid_before,
    })
}

fn pending_from_json(value: &Value) -> Result<PendingClose, RepositoryError> {
    let text = |key: &str| value.get(key).and_then(Value::as_str);
    let tx_hash = text("txHash")
        .and_then(|text| text.strip_prefix("0x"))
        .and_then(|digits| hex::decode(digits).ok())
        .ok_or_else(|| conversion("pending close"))
        .and_then(|bytes| fixed::<32>(bytes, "pending close"))?;
    Ok(PendingClose {
        tx_hash,
        cumulative: text("cumulative")
            .ok_or_else(|| conversion("pending close"))
            .and_then(parse_amount)?,
        capture: text("capture")
            .ok_or_else(|| conversion("pending close"))
            .and_then(parse_amount)?,
        valid_before: value
            .get("validBefore")
            .and_then(Value::as_u64)
            .ok_or_else(|| conversion("pending close"))?,
    })
}

fn record_from_row(row: &Row) -> Result<ChannelRecord, RepositoryError> {
    let get_err = map_db_error;
    let amount = |index: usize| -> Result<u128, RepositoryError> {
        let text: String = row.try_get(index).map_err(get_err)?;
        parse_amount(&text)
    };
    let bytes = |index: usize| -> Result<Vec<u8>, RepositoryError> {
        row.try_get::<_, Vec<u8>>(index).map_err(get_err)
    };
    let state_text: String = row.try_get(18).map_err(get_err)?;
    let retained: Value = row.try_get(14).map_err(get_err)?;
    let pending: Option<Value> = row.try_get(20).map_err(get_err)?;
    let units: i64 = row.try_get(17).map_err(get_err)?;
    let chain_id: i64 = row.try_get(1).map_err(get_err)?;
    let close_requested_at: i64 = row.try_get(19).map_err(get_err)?;
    Ok(ChannelRecord {
        channel_id: fixed(bytes(0)?, "channel id")?,
        chain_id: u64::try_from(chain_id).map_err(|_| conversion("chain id"))?,
        escrow: fixed(bytes(2)?, "escrow")?,
        descriptor: ChannelDescriptor {
            payer: fixed(bytes(3)?, "payer")?,
            payee: fixed(bytes(4)?, "payee")?,
            operator: fixed(bytes(5)?, "operator")?,
            token: fixed(bytes(6)?, "token")?,
            salt: fixed(bytes(7)?, "salt")?,
            authorized_signer: fixed(bytes(8)?, "authorized signer")?,
            expiring_nonce_hash: fixed(bytes(9)?, "nonce hash")?,
        },
        deposit: amount(10)?,
        settled_on_chain: amount(11)?,
        highest_cumulative: amount(12)?,
        highest_signature: fixed(bytes(13)?, "signature")?,
        retained_vouchers: vouchers_from_json(&retained)?,
        spent: amount(15)?,
        reserved: amount(16)?,
        units: u64::try_from(units).map_err(|_| conversion("units"))?,
        state: ChannelLifecycle::parse(&state_text).ok_or_else(|| conversion("state"))?,
        close_requested_at: u64::try_from(close_requested_at)
            .map_err(|_| conversion("close request"))?,
        pending_close: pending.as_ref().map(pending_from_json).transpose()?,
        last_chain_read_at: row.try_get(21).map_err(get_err)?,
        last_settlement_at: row.try_get(22).map_err(get_err)?,
        last_settlement_spent: amount(23)?,
        updated_at: row.try_get(24).map_err(get_err)?,
    })
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

/// Lock the channel row; `None` when there is no such channel.
async fn lock_channel(
    tx: &Transaction<'_>,
    channel_id: &[u8; 32],
) -> Result<Option<ChannelRecord>, RepositoryError> {
    tx.query_opt(&with_columns(LOCK_SQL), &[&channel_id.as_slice()])
        .await
        .map_err(map_db_error)?
        .as_ref()
        .map(record_from_row)
        .transpose()
}

/// Write the record back (the row is locked by this transaction) and return it with the
/// `updated_at` the database stamped. `clear_lease` also releases the settlement lease.
async fn store(
    tx: &Transaction<'_>,
    record: &mut ChannelRecord,
    clear_lease: bool,
) -> Result<(), RepositoryError> {
    let pending = record.pending_close.as_ref().map(pending_json);
    let row = tx
        .query_one(
            STORE_SQL,
            &[
                &record.channel_id.as_slice(),
                &amount_text(record.deposit),
                &amount_text(record.settled_on_chain),
                &amount_text(record.highest_cumulative),
                &record.highest_signature.as_slice(),
                &vouchers_json(&record.retained_vouchers),
                &amount_text(record.spent),
                &amount_text(record.reserved),
                &to_i64(record.units),
                &record.state.as_str(),
                &to_i64(record.close_requested_at),
                &pending,
                &record.last_chain_read_at,
                &record.last_settlement_at,
                &amount_text(record.last_settlement_spent),
                &clear_lease,
            ],
        )
        .await
        .map_err(map_db_error)?;
    record.updated_at = row.try_get(0).map_err(map_db_error)?;
    Ok(())
}

async fn commit(tx: Transaction<'_>) -> Result<(), RepositoryError> {
    tx.commit().await.map_err(map_db_error)
}

/// `spent -= amount`, `reserved -= amount` on a locked channel.
fn give_back(record: &mut ChannelRecord, amount: u128) {
    record.spent = record.spent.saturating_sub(amount);
    record.reserved = record.reserved.saturating_sub(amount);
}

#[async_trait]
impl ChannelRepository for MppChannelRepository {
    /// Idempotent: a retry after a lost reply merges the same values again.
    async fn open_or_refresh(&self, channel: NewChannel) -> Result<ChannelRecord, RepositoryError> {
        retry_db!("open_mpp_channel", {
            let mut client = self.client().await?;
            let tx = client.transaction().await.map_err(map_db_error)?;
            let descriptor = &channel.descriptor;
            let inserted = tx
                .query_opt(
                    INSERT_SQL,
                    &[
                        &channel.channel_id.as_slice(),
                        &to_i64(channel.chain_id),
                        &channel.escrow.as_slice(),
                        &descriptor.payer.as_slice(),
                        &descriptor.payee.as_slice(),
                        &descriptor.operator.as_slice(),
                        &descriptor.token.as_slice(),
                        &descriptor.salt.as_slice(),
                        &descriptor.authorized_signer.as_slice(),
                        &descriptor.expiring_nonce_hash.as_slice(),
                        &amount_text(channel.deposit),
                        &amount_text(channel.settled_on_chain),
                        &amount_text(channel.highest_cumulative),
                        &channel.highest_signature.as_slice(),
                    ],
                )
                .await
                .map_err(map_db_error)?;
            let record = if inserted.is_some() {
                lock_channel(&tx, &channel.channel_id).await?
            } else {
                let existing = lock_channel(&tx, &channel.channel_id).await?;
                match existing {
                    Some(mut record) => {
                        record.deposit = record.deposit.max(channel.deposit);
                        record.settled_on_chain =
                            record.settled_on_chain.max(channel.settled_on_chain);
                        if channel.highest_cumulative > record.highest_cumulative {
                            promote_voucher(
                                &mut record,
                                channel.highest_cumulative,
                                channel.highest_signature,
                            );
                        }
                        store(&tx, &mut record, false).await?;
                        Some(record)
                    }
                    None => None,
                }
            };
            commit_and(tx, record).await
        })
    }

    async fn get(&self, channel_id: &[u8; 32]) -> Result<Option<ChannelRecord>, RepositoryError> {
        retry_db!("get_mpp_channel", {
            let client = self.client().await?;
            client
                .query_opt(&with_columns(GET_SQL), &[&channel_id.as_slice()])
                .await
                .map_err(map_db_error)?
                .as_ref()
                .map(record_from_row)
                .transpose()
        })
    }

    async fn accept_voucher(
        &self,
        channel_id: &[u8; 32],
        cumulative: u128,
        signature: [u8; SIGNATURE_LEN],
    ) -> Result<VoucherOutcome, RepositoryError> {
        let mut client = self.client().await?;
        let tx = client.transaction().await.map_err(map_db_error)?;
        let Some(mut record) = lock_channel(&tx, channel_id).await? else {
            return Ok(VoucherOutcome::Missing);
        };
        if !record.accepts_vouchers() {
            return Ok(VoucherOutcome::NotAccepting);
        }
        if cumulative <= record.highest_cumulative {
            return Ok(VoucherOutcome::NotHigher);
        }
        promote_voucher(&mut record, cumulative, signature);
        store(&tx, &mut record, false).await?;
        commit(tx).await?;
        Ok(VoucherOutcome::Accepted(Box::new(record)))
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
        let mut client = self.client().await?;
        let tx = client.transaction().await.map_err(map_db_error)?;
        let Some(mut record) = lock_channel(&tx, channel_id).await? else {
            return Ok(ReserveOutcome::Missing);
        };
        if !record.accepts_vouchers() {
            return Ok(ReserveOutcome::NotAccepting);
        }
        let available = record.available();
        if available < amount {
            return Ok(ReserveOutcome::Insufficient { available });
        }
        record.spent = record.spent.saturating_add(amount);
        record.reserved = record.reserved.saturating_add(amount);
        // A request id already in use is a unique violation: `AlreadyExists`, and the
        // dropped transaction rolls the balance back.
        tx.execute(
            INSERT_RESERVATION_SQL,
            &[&request_id, &channel_id.as_slice(), &amount_text(amount)],
        )
        .await
        .map_err(map_db_error)?;
        store(&tx, &mut record, false).await?;
        commit(tx).await?;
        Ok(ReserveOutcome::Reserved(Box::new(record)))
    }

    async fn charge(
        &self,
        request_id: Uuid,
        actual: u128,
        units: u64,
    ) -> Result<ChargeOutcome, RepositoryError> {
        let mut client = self.client().await?;
        let tx = client.transaction().await.map_err(map_db_error)?;
        let Some((mut record, amount)) = take_reservation(&tx, request_id).await? else {
            return Ok(ChargeOutcome::NoReservation);
        };
        let before = record.spent.saturating_sub(amount);
        let cap = record.highest_cumulative.saturating_sub(before);
        let charged = actual.min(cap);
        record.spent = before.saturating_add(charged);
        record.reserved = record.reserved.saturating_sub(amount);
        record.units = record
            .units
            .saturating_add(units)
            .min(i64::MAX.unsigned_abs());
        store(&tx, &mut record, false).await?;
        commit(tx).await?;
        Ok(ChargeOutcome::Charged {
            record: Box::new(record),
            charged,
        })
    }

    async fn release(&self, request_id: Uuid) -> Result<bool, RepositoryError> {
        let mut client = self.client().await?;
        let tx = client.transaction().await.map_err(map_db_error)?;
        let Some((mut record, amount)) = take_reservation(&tx, request_id).await? else {
            return Ok(false);
        };
        give_back(&mut record, amount);
        store(&tx, &mut record, false).await?;
        commit(tx).await?;
        Ok(true)
    }

    /// Idempotent: the greater values are taken again.
    async fn record_chain_state(
        &self,
        channel_id: &[u8; 32],
        chain: ChannelState,
        read_at: DateTime<Utc>,
    ) -> Result<Option<ChannelRecord>, RepositoryError> {
        retry_db!("record_mpp_channel_chain_state", {
            let mut client = self.client().await?;
            let tx = client.transaction().await.map_err(map_db_error)?;
            let Some(mut record) = lock_channel(&tx, channel_id).await? else {
                return Ok(None);
            };
            if chain.deposit != 0 {
                record.deposit = record.deposit.max(chain.deposit);
            }
            record.settled_on_chain = record.settled_on_chain.max(chain.settled);
            if chain.close_requested_at != 0 {
                record.close_requested_at = chain.close_requested_at;
            }
            record.last_chain_read_at = Some(read_at);
            store(&tx, &mut record, false).await?;
            commit(tx).await?;
            Ok(Some(record))
        })
    }

    async fn mark_closing(
        &self,
        channel_id: &[u8; 32],
        pending: PendingClose,
    ) -> Result<CloseTransition, RepositoryError> {
        let mut client = self.client().await?;
        let tx = client.transaction().await.map_err(map_db_error)?;
        let Some(mut record) = lock_channel(&tx, channel_id).await? else {
            return Ok(CloseTransition::Missing);
        };
        if record.state != ChannelLifecycle::Open {
            return Ok(CloseTransition::NotOpen);
        }
        record.state = ChannelLifecycle::Closing;
        record.pending_close = Some(pending);
        store(&tx, &mut record, false).await?;
        commit(tx).await?;
        Ok(CloseTransition::Marked(Box::new(record)))
    }

    /// One statement, idempotent.
    async fn finalize(
        &self,
        channel_id: &[u8; 32],
        settled_on_chain: u128,
    ) -> Result<(), RepositoryError> {
        retry_db!("finalize_mpp_channel", {
            let client = self.client().await?;
            client
                .execute(
                    FINALIZE_SQL,
                    &[&channel_id.as_slice(), &amount_text(settled_on_chain)],
                )
                .await
                .map_err(map_db_error)?;
            Ok(())
        })
    }

    /// One statement, idempotent.
    async fn clear_closing(&self, channel_id: &[u8; 32]) -> Result<(), RepositoryError> {
        retry_db!("clear_mpp_channel_closing", {
            let client = self.client().await?;
            client
                .execute(CLEAR_CLOSING_SQL, &[&channel_id.as_slice()])
                .await
                .map_err(map_db_error)?;
            Ok(())
        })
    }

    /// Not retried: after a lost reply the retry would find the lease held by this caller
    /// and report that someone else has it.
    async fn claim_settlement_lease(
        &self,
        channel_id: &[u8; 32],
        now: DateTime<Utc>,
        until: DateTime<Utc>,
    ) -> Result<Option<ChannelRecord>, RepositoryError> {
        let client = self.client().await?;
        client
            .query_opt(
                &with_columns(CLAIM_LEASE_SQL),
                &[&channel_id.as_slice(), &now, &until],
            )
            .await
            .map_err(map_db_error)?
            .as_ref()
            .map(record_from_row)
            .transpose()
    }

    /// Idempotent: the greater settled amount, the same retained-voucher filter.
    async fn record_settlement(
        &self,
        channel_id: &[u8; 32],
        cumulative: u128,
        at: DateTime<Utc>,
    ) -> Result<(), RepositoryError> {
        retry_db!("record_mpp_channel_settlement", {
            let mut client = self.client().await?;
            let tx = client.transaction().await.map_err(map_db_error)?;
            let Some(mut record) = lock_channel(&tx, channel_id).await? else {
                return Ok(());
            };
            record.settled_on_chain = record.settled_on_chain.max(cumulative);
            record.last_settlement_at = Some(at);
            record.last_settlement_spent = record.spent;
            record
                .retained_vouchers
                .retain(|voucher| voucher.cumulative > cumulative);
            store(&tx, &mut record, true).await?;
            commit(tx).await
        })
    }

    /// One statement, idempotent.
    async fn release_settlement_lease(&self, channel_id: &[u8; 32]) -> Result<(), RepositoryError> {
        retry_db!("release_mpp_channel_lease", {
            let client = self.client().await?;
            client
                .execute(RELEASE_LEASE_SQL, &[&channel_id.as_slice()])
                .await
                .map_err(map_db_error)?;
            Ok(())
        })
    }

    async fn list_active(
        &self,
        after: Option<[u8; 32]>,
        limit: usize,
    ) -> Result<Vec<ChannelRecord>, RepositoryError> {
        let limit = i64::try_from(limit.min(MAX_LIST_LIMIT)).unwrap_or(i64::MAX);
        retry_db!("list_active_mpp_channels", {
            let client = self.client().await?;
            let rows = match &after {
                Some(after) => {
                    client
                        .query(&with_columns(LIST_AFTER_SQL), &[&after.as_slice(), &limit])
                        .await
                }
                None => client.query(&with_columns(LIST_FIRST_SQL), &[&limit]).await,
            }
            .map_err(map_db_error)?;
            rows.iter().map(record_from_row).collect()
        })
    }

    /// Per channel, one transaction: lock the channel, delete its stale reservations, give
    /// their amounts back. A channel another instance holds is left to it.
    async fn release_stale_reservations(
        &self,
        cutoff: DateTime<Utc>,
    ) -> Result<u64, RepositoryError> {
        let mut client = self.client().await?;
        let mut released = 0u64;
        let mut cursor: Vec<u8> = Vec::new();
        for _ in 0..MAX_BATCHES {
            let channels = client
                .query(
                    STALE_CHANNELS_SQL,
                    &[&cutoff, &cursor.as_slice(), &SCAN_BATCH],
                )
                .await
                .map_err(map_db_error)?;
            for row in &channels {
                let channel_id: Vec<u8> = row.try_get(0).map_err(map_db_error)?;
                let tx = client.transaction().await.map_err(map_db_error)?;
                let locked = tx
                    .query_opt(&with_columns(RELEASE_STALE_LOCK_SQL), &[&channel_id])
                    .await
                    .map_err(map_db_error)?;
                let Some(locked) = locked else { continue };
                let mut record = record_from_row(&locked)?;
                let taken = tx
                    .query(TAKE_STALE_SQL, &[&channel_id, &cutoff])
                    .await
                    .map_err(map_db_error)?;
                if taken.is_empty() {
                    continue;
                }
                for row in &taken {
                    let text: String = row.try_get(0).map_err(map_db_error)?;
                    give_back(&mut record, parse_amount(&text)?);
                }
                store(&tx, &mut record, false).await?;
                commit(tx).await?;
                released += u64::try_from(taken.len()).unwrap_or(u64::MAX);
            }
            match channels.last() {
                Some(last) if batch_is_full(channels.len()) => {
                    cursor = last.try_get(0).map_err(map_db_error)?;
                }
                _ => break,
            }
        }
        Ok(released)
    }

    async fn purge_finalized(&self, cutoff: DateTime<Utc>) -> Result<u64, RepositoryError> {
        let client = self.client().await?;
        let mut purged = 0u64;
        let mut cursor: Vec<u8> = Vec::new();
        for _ in 0..MAX_BATCHES {
            let page = client
                .query(FINALIZED_PAGE_SQL, &[&cursor.as_slice(), &SCAN_BATCH])
                .await
                .map_err(map_db_error)?;
            let ids = page
                .iter()
                .map(|row| row.try_get::<_, Vec<u8>>(0).map_err(map_db_error))
                .collect::<Result<Vec<_>, _>>()?;
            if !ids.is_empty() {
                purged += client
                    .execute(PURGE_SQL, &[&ids, &cutoff])
                    .await
                    .map_err(map_db_error)?;
            }
            match ids.last() {
                Some(last) if batch_is_full(ids.len()) => cursor = last.clone(),
                _ => break,
            }
        }
        Ok(purged)
    }
}

/// Commit, then hand back the record the transaction produced.
async fn commit_and(
    tx: Transaction<'_>,
    record: Option<ChannelRecord>,
) -> Result<ChannelRecord, RepositoryError> {
    commit(tx).await?;
    record.ok_or_else(|| conversion("row vanished"))
}

/// Find the reservation's channel, lock it, then delete the reservation under that lock.
/// `None` when the reservation is gone (charged, released or reaped), or its channel is.
async fn take_reservation(
    tx: &Transaction<'_>,
    request_id: Uuid,
) -> Result<Option<(ChannelRecord, u128)>, RepositoryError> {
    let Some(row) = tx
        .query_opt(RESERVATION_CHANNEL_SQL, &[&request_id])
        .await
        .map_err(map_db_error)?
    else {
        return Ok(None);
    };
    let channel_id: Vec<u8> = row.try_get(0).map_err(map_db_error)?;
    let channel_id: [u8; 32] = fixed(channel_id, "channel id")?;
    let Some(record) = lock_channel(tx, &channel_id).await? else {
        return Ok(None);
    };
    let Some(taken) = tx
        .query_opt(TAKE_RESERVATION_SQL, &[&request_id])
        .await
        .map_err(map_db_error)?
    else {
        return Ok(None);
    };
    let text: String = taken.try_get(0).map_err(map_db_error)?;
    Ok(Some((record, parse_amount(&text)?)))
}

#[cfg(test)]
mod tests;
