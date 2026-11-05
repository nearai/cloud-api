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
use std::sync::Mutex;

use crate::pool::DbPool;
use crate::repositories::utils::map_db_error;
use crate::retry_db;
use anyhow::Context;
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use deadpool_postgres::Transaction;
use serde_json::{json, Value};
use services::common::RepositoryError;
use services::mpp::ports::{chain_read_claim_at, chain_read_may_claim, chain_read_observed_at};
use services::mpp::session::{ChannelDescriptor, ChannelState, SIGNATURE_LEN};
use services::mpp::{
    ChannelLifecycle, ChannelRecord, ChannelRepository, ChargeOutcome, CloseTransition, NewChannel,
    PendingClose, ReserveOutcome, RetainedVoucher, SettlementLease, VoucherOutcome, VoucherRules,
    MAX_RETAINED_VOUCHERS,
};
use tokio_postgres::Row;
use uuid::Uuid;

#[path = "mpp_channel_sql.rs"]
mod sql;
use sql::*;
#[path = "mpp_channel_purge.rs"]
mod purge;

pub struct MppChannelRepository {
    pool: DbPool,
    purge_cursor: Mutex<Vec<u8>>,
}

impl MppChannelRepository {
    pub fn new(pool: DbPool) -> Self {
        Self {
            pool,
            purge_cursor: Mutex::new(Vec::new()),
        }
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
    if record.state == ChannelLifecycle::Finalized {
        record.spent = record.spent.max(record.settled_on_chain);
    }
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
        rules: VoucherRules,
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
        if cumulative - record.highest_cumulative < rules.min_delta {
            return Ok(VoucherOutcome::DeltaTooSmall);
        }
        if rules.settled_floor_from_chain && cumulative < record.settled_on_chain {
            return Ok(VoucherOutcome::BelowSettled);
        }
        if cumulative > record.deposit {
            return Ok(VoucherOutcome::AboveDeposit);
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
        if record.state == ChannelLifecycle::Finalized {
            record.spent = record.spent.max(record.settled_on_chain);
        }
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
            let observed_at = chain_read_observed_at(record.last_chain_read_at, read_at);
            if observed_at == read_at {
                record.close_requested_at = chain.close_requested_at;
            }
            record.last_chain_read_at = Some(observed_at);
            if let Some(updated) = tx
                .query_opt(
                    CHAIN_STATE_SQL,
                    &[
                        &channel_id.as_slice(),
                        &amount_text(record.deposit),
                        &amount_text(record.settled_on_chain),
                        &to_i64(record.close_requested_at),
                    ],
                )
                .await
                .map_err(map_db_error)?
            {
                record.updated_at = updated.try_get(0).map_err(map_db_error)?;
            }
            tx.execute(
                READ_TIMESTAMP_SQL,
                &[&channel_id.as_slice(), &record.last_chain_read_at],
            )
            .await
            .map_err(map_db_error)?;
            commit(tx).await?;
            Ok(Some(record))
        })
    }

    async fn mark_closing(
        &self,
        channel_id: &[u8; 32],
        pending: PendingClose,
        now: DateTime<Utc>,
    ) -> Result<CloseTransition, RepositoryError> {
        let mut client = self.client().await?;
        let tx = client.transaction().await.map_err(map_db_error)?;
        let Some(mut record) = lock_channel(&tx, channel_id).await? else {
            return Ok(CloseTransition::Missing);
        };
        if record.state != ChannelLifecycle::Open {
            return Ok(CloseTransition::NotOpen);
        }
        if lease_until(&tx, channel_id)
            .await?
            .is_some_and(|until| until > now)
        {
            return Ok(CloseTransition::SettlementInFlight);
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

    async fn clear_closing(
        &self,
        channel_id: &[u8; 32],
        expected_tx_hash: &[u8; 32],
    ) -> Result<bool, RepositoryError> {
        close_transition(self, channel_id, expected_tx_hash, None).await
    }

    async fn finalize_pending_close(
        &self,
        channel_id: &[u8; 32],
        expected_tx_hash: &[u8; 32],
        settled_on_chain: u128,
    ) -> Result<bool, RepositoryError> {
        close_transition(self, channel_id, expected_tx_hash, Some(settled_on_chain)).await
    }

    async fn claim_chain_read(
        &self,
        channel_id: &[u8; 32],
        now: DateTime<Utc>,
        min_interval: chrono::Duration,
    ) -> Result<bool, RepositoryError> {
        let mut client = self.client().await?;
        let tx = client.transaction().await.map_err(map_db_error)?;
        let Some(mut record) = lock_channel(&tx, channel_id).await? else {
            return Ok(false);
        };
        if !chain_read_may_claim(record.last_chain_read_at, now, min_interval) {
            return Ok(false);
        }
        record.last_chain_read_at =
            Some(chain_read_claim_at(now, min_interval).ok_or_else(|| {
                RepositoryError::ValidationFailed("chain read interval".to_string())
            })?);
        tx.execute(
            READ_TIMESTAMP_SQL,
            &[&channel_id.as_slice(), &record.last_chain_read_at],
        )
        .await
        .map_err(map_db_error)?;
        commit(tx).await?;
        Ok(true)
    }

    async fn clear_chain_read_claim(
        &self,
        channel_id: &[u8; 32],
        claimed_at: DateTime<Utc>,
    ) -> Result<bool, RepositoryError> {
        let client = self.client().await?;
        Ok(client
            .execute(CLEAR_READ_CLAIM_SQL, &[&channel_id.as_slice(), &claimed_at])
            .await
            .map_err(map_db_error)?
            == 1)
    }

    async fn claim_settlement_lease(
        &self,
        channel_id: &[u8; 32],
        now: DateTime<Utc>,
        until: DateTime<Utc>,
    ) -> Result<Option<(ChannelRecord, SettlementLease)>, RepositoryError> {
        let mut client = self.client().await?;
        let tx = client.transaction().await.map_err(map_db_error)?;
        let Some(mut record) = lock_channel(&tx, channel_id).await? else {
            return Ok(None);
        };
        let until = DateTime::from_timestamp_micros(until.timestamp_micros())
            .ok_or_else(|| RepositoryError::ValidationFailed("lease deadline".to_string()))?;
        if until <= now
            || record.state != ChannelLifecycle::Open
            || lease_until(&tx, channel_id)
                .await?
                .is_some_and(|held| held >= now)
        {
            return Ok(None);
        }
        tx.execute(SET_LEASE_SQL, &[&channel_id.as_slice(), &until])
            .await
            .map_err(map_db_error)?;
        store(&tx, &mut record, false).await?;
        commit(tx).await?;
        Ok(Some((record, SettlementLease::new(until))))
    }

    async fn record_settlement(
        &self,
        channel_id: &[u8; 32],
        lease: SettlementLease,
        cumulative: u128,
        at: DateTime<Utc>,
    ) -> Result<bool, RepositoryError> {
        let mut client = self.client().await?;
        let tx = client.transaction().await.map_err(map_db_error)?;
        let Some(mut record) = lock_channel(&tx, channel_id).await? else {
            return Ok(false);
        };
        if lease_until(&tx, channel_id).await? != Some(lease.until())
            || record.state != ChannelLifecycle::Open
        {
            return Ok(false);
        }
        record.settled_on_chain = record.settled_on_chain.max(cumulative);
        record.last_settlement_at = Some(at);
        record.last_settlement_spent = record.spent;
        record
            .retained_vouchers
            .retain(|voucher| voucher.cumulative > cumulative);
        store(&tx, &mut record, true).await?;
        commit(tx).await?;
        Ok(true)
    }

    async fn release_settlement_lease(
        &self,
        channel_id: &[u8; 32],
        lease: SettlementLease,
    ) -> Result<bool, RepositoryError> {
        let mut client = self.client().await?;
        let tx = client.transaction().await.map_err(map_db_error)?;
        let Some(mut record) = lock_channel(&tx, channel_id).await? else {
            return Ok(false);
        };
        if lease_until(&tx, channel_id).await? != Some(lease.until()) {
            return Ok(false);
        }
        store(&tx, &mut record, true).await?;
        commit(tx).await?;
        Ok(true)
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
        let client = &client;
        purge::scan(
            &self.purge_cursor,
            SCAN_BATCH,
            MAX_BATCHES,
            |cursor| async move {
                let page = client
                    .query(FINALIZED_PAGE_SQL, &[&cursor.as_slice(), &SCAN_BATCH])
                    .await
                    .map_err(map_db_error)?;
                let ids = page
                    .iter()
                    .map(|row| row.try_get::<_, Vec<u8>>(0).map_err(map_db_error))
                    .collect::<Result<Vec<_>, _>>()?;
                let purged = if !ids.is_empty() {
                    client
                        .execute(PURGE_SQL, &[&ids, &cutoff])
                        .await
                        .map_err(map_db_error)?
                } else {
                    0
                };
                Ok((ids, purged))
            },
        )
        .await
    }
}

/// The caller already holds this channel's row lock.
async fn lease_until(
    tx: &Transaction<'_>,
    id: &[u8; 32],
) -> Result<Option<DateTime<Utc>>, RepositoryError> {
    tx.query_one(LEASE_SQL, &[&id.as_slice()])
        .await
        .map_err(map_db_error)?
        .try_get(0)
        .map_err(map_db_error)
}

async fn close_transition(
    repo: &MppChannelRepository,
    id: &[u8; 32],
    expected: &[u8; 32],
    settled: Option<u128>,
) -> Result<bool, RepositoryError> {
    let mut client = repo.client().await?;
    let tx = client.transaction().await.map_err(map_db_error)?;
    let Some(mut record) = lock_channel(&tx, id).await? else {
        return Ok(false);
    };
    if record.state != ChannelLifecycle::Closing
        || record
            .pending_close
            .is_none_or(|pending| pending.tx_hash != *expected)
    {
        return Ok(false);
    }
    record.state = if settled.is_some() {
        ChannelLifecycle::Finalized
    } else {
        ChannelLifecycle::Open
    };
    if let Some(settled) = settled {
        record.settled_on_chain = record.settled_on_chain.max(settled);
    }
    record.pending_close = None;
    store(&tx, &mut record, false).await?;
    commit(tx).await?;
    Ok(true)
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
