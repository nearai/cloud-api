/// Every column of a channel the ledger reads, in the order `record_from_row` expects.
/// Amounts come back as text.
pub(super) const COLUMNS: &str = "\
channel_id, chain_id, escrow, payer, payee, operator, token, salt, authorized_signer, \
expiring_nonce_hash, deposit::TEXT, settled_on_chain::TEXT, highest_cumulative::TEXT, \
highest_signature, retained_vouchers, spent::TEXT, reserved::TEXT, units, state, \
close_requested_at, pending_close, last_chain_read_at, last_settlement_at, \
last_settlement_spent::TEXT, updated_at";

pub(super) const INSERT_SQL: &str = "\
INSERT INTO mpp_channels (channel_id, chain_id, escrow, payer, payee, operator, token, salt, \
authorized_signer, expiring_nonce_hash, deposit, settled_on_chain, highest_cumulative, \
highest_signature, state) \
VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11::TEXT::NUMERIC, $12::TEXT::NUMERIC, \
$13::TEXT::NUMERIC, $14, 'open') \
ON CONFLICT (channel_id) DO NOTHING \
RETURNING updated_at";

pub(super) const GET_SQL: &str = "SELECT {COLUMNS} FROM mpp_channels WHERE channel_id = $1";

/// The channel row, locked until the transaction ends.
pub(super) const LOCK_SQL: &str =
    "SELECT {COLUMNS} FROM mpp_channels WHERE channel_id = $1 FOR UPDATE";

/// Write back every column the ledger moves. `updated_at` takes `clock_timestamp()`, not
/// `NOW()`: a transaction that waited for the row lock must not stamp a time before the
/// write it waited for. `$16` clears the settlement lease.
pub(super) const STORE_SQL: &str = "\
UPDATE mpp_channels SET deposit = $2::TEXT::NUMERIC, settled_on_chain = $3::TEXT::NUMERIC, \
highest_cumulative = $4::TEXT::NUMERIC, highest_signature = $5, retained_vouchers = $6, \
spent = $7::TEXT::NUMERIC, reserved = $8::TEXT::NUMERIC, units = $9, state = $10, \
close_requested_at = $11, pending_close = $12, last_chain_read_at = $13, \
last_settlement_at = $14, last_settlement_spent = $15::TEXT::NUMERIC, \
settlement_lease_until = CASE WHEN $16 THEN NULL ELSE settlement_lease_until END, \
updated_at = clock_timestamp() \
WHERE channel_id = $1 \
RETURNING updated_at";

pub(super) const READ_TIMESTAMP_SQL: &str = "UPDATE mpp_channels SET last_chain_read_at = $2 \
WHERE channel_id = $1 AND last_chain_read_at IS DISTINCT FROM $2";
pub(super) const CLEAR_READ_CLAIM_SQL: &str = "UPDATE mpp_channels SET last_chain_read_at = NULL \
WHERE channel_id = $1 AND last_chain_read_at = $2";

pub(super) const CHAIN_STATE_SQL: &str = "UPDATE mpp_channels SET deposit = $2::TEXT::NUMERIC, \
settled_on_chain = $3::TEXT::NUMERIC, close_requested_at = $4, updated_at = clock_timestamp() \
WHERE channel_id = $1 AND (deposit, settled_on_chain, close_requested_at) IS DISTINCT FROM \
($2::TEXT::NUMERIC, $3::TEXT::NUMERIC, $4::BIGINT) RETURNING updated_at";

pub(super) const INSERT_RESERVATION_SQL: &str = "\
INSERT INTO mpp_channel_reservations (request_id, channel_id, amount) \
VALUES ($1, $2, $3::TEXT::NUMERIC)";

/// Which channel a reservation belongs to, read before the channel is locked.
pub(super) const RESERVATION_CHANNEL_SQL: &str =
    "SELECT channel_id FROM mpp_channel_reservations WHERE request_id = $1";

/// Taken after the channel lock, so a reservation another transaction already settled
/// returns no row.
pub(super) const TAKE_RESERVATION_SQL: &str = "\
DELETE FROM mpp_channel_reservations WHERE request_id = $1 RETURNING amount::TEXT";

pub(super) const FINALIZE_SQL: &str = "\
UPDATE mpp_channels SET state = 'finalized', pending_close = NULL, \
settled_on_chain = GREATEST(settled_on_chain, $2::TEXT::NUMERIC), \
spent = GREATEST(spent, settled_on_chain, $2::TEXT::NUMERIC), \
updated_at = clock_timestamp() \
WHERE channel_id = $1";

pub(super) const LEASE_SQL: &str =
    "SELECT settlement_lease_until FROM mpp_channels WHERE channel_id = $1";
pub(super) const SET_LEASE_SQL: &str = "UPDATE mpp_channels SET settlement_lease_until = $2, \
updated_at = clock_timestamp() WHERE channel_id = $1";

/// Through `idx_mpp_channels_open`.
pub(super) const LIST_FIRST_SQL: &str = "\
SELECT {COLUMNS} FROM mpp_channels WHERE state <> 'finalized' \
ORDER BY channel_id LIMIT $1";
pub(super) const LIST_AFTER_SQL: &str = "\
SELECT {COLUMNS} FROM mpp_channels WHERE state <> 'finalized' AND channel_id > $1 \
ORDER BY channel_id LIMIT $2";

/// Channels with a stale reservation, after a cursor: a channel another instance holds
/// locked is skipped (see `RELEASE_STALE_LOCK_SQL`), so paging is by key, not by retrying.
pub(super) const STALE_CHANNELS_SQL: &str = "\
SELECT DISTINCT channel_id FROM mpp_channel_reservations \
WHERE created_at < $1 AND channel_id > $2 ORDER BY channel_id LIMIT $3";
pub(super) const RELEASE_STALE_LOCK_SQL: &str = "\
SELECT {COLUMNS} FROM mpp_channels WHERE channel_id = $1 FOR UPDATE SKIP LOCKED";
pub(super) const TAKE_STALE_SQL: &str = "\
DELETE FROM mpp_channel_reservations WHERE channel_id = $1 AND created_at < $2 \
RETURNING amount::TEXT";

/// Finalized channels after a cursor, then the delete of those old enough. The scan walks
/// `idx_mpp_channels_finalized` in key order and is bounded by the batch; `SKIP LOCKED`
/// lets two instances purge at once. Reservations go with their channel (`ON DELETE
/// CASCADE`).
pub(super) const FINALIZED_PAGE_SQL: &str = "\
SELECT channel_id FROM mpp_channels WHERE state = 'finalized' AND channel_id > $1 \
ORDER BY channel_id LIMIT $2";
pub(super) const PURGE_SQL: &str = "\
DELETE FROM mpp_channels WHERE channel_id IN ( \
SELECT channel_id FROM mpp_channels WHERE channel_id = ANY($1) AND state = 'finalized' \
AND updated_at < $2 FOR UPDATE SKIP LOCKED)";

/// Rows examined per statement: small enough that no statement holds its locks for long.
pub(super) const SCAN_BATCH: i64 = 500;
/// Statements per call; anything left goes at the next call.
pub(super) const MAX_BATCHES: u32 = 200;
/// Longest page `list_active` returns, whatever the caller asks.
pub(super) const MAX_LIST_LIMIT: usize = 1000;
