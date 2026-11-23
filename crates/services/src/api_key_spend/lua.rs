//! The Lua scripts of the shared total, sent with plain `EVAL` (the ACL user
//! cannot `EVALSHA` or `SCRIPT LOAD`), and the key names.
//!
//! Totals can exceed 2^53, so scripts never do arithmetic on them in Lua: the
//! server adds with `INCRBY`, and values travel as strings.
//!
//! Keys for API key id `{id}` and environment `{env}`:
//! - total `T` = `spend:v2:{env}:{id}`: decimal `i64`; expires after the
//!   re-check interval, set only by FINISH;
//! - pending `T:pend`: costs recorded while a seed's `SUM` runs (120 s);
//! - generation `T:gen`: bumped by FINISH and INVALIDATE (3600 s).
//!
//! The three keys of one API key are used together in one script, so they
//! must live on one server (they do not carry a hash tag).

use uuid::Uuid;

pub const PEND_TTL_SECS: u64 = 120;
pub const GEN_TTL_SECS: u64 = 3600;

pub fn total_key(environment: &str, api_key_id: Uuid) -> String {
    format!("spend:v2:{environment}:{api_key_id}")
}

pub fn pend_key(environment: &str, api_key_id: Uuid) -> String {
    format!("{}:pend", total_key(environment, api_key_id))
}

pub fn gen_key(environment: &str, api_key_id: Uuid) -> String {
    format!("{}:gen", total_key(environment, api_key_id))
}

/// KEYS: pend, gen. ARGV: pend ttl, gen ttl. Returns the generation (string).
/// Creates `pend` (value 0) if missing and refreshes its expiry, so costs
/// recorded from now on are collected.
pub const BEGIN: &str = r#"
local g = redis.call('GET', KEYS[2]) or '0'
redis.call('SET', KEYS[2], g, 'EX', ARGV[2])
local p = redis.call('GET', KEYS[1]) or '0'
redis.call('SET', KEYS[1], p, 'EX', ARGV[1])
return g
"#;

/// KEYS: total, pend. ARGV: cost. Adds to each key that exists; never creates
/// one. Returns 1 for total, +2 for pend.
pub const ADD: &str = r#"
local n = 0
if redis.call('EXISTS', KEYS[1]) == 1 then redis.call('INCRBY', KEYS[1], ARGV[1]); n = n + 1 end
if redis.call('EXISTS', KEYS[2]) == 1 then redis.call('INCRBY', KEYS[2], ARGV[1]); n = n + 2 end
return n
"#;

/// KEYS: total, pend, gen. ARGV: sum, total ttl, expected generation, gen ttl.
/// Returns the new total (string), or nil when it aborts.
pub const FINISH: &str = r#"
if (redis.call('GET', KEYS[3]) or '0') ~= ARGV[3] then return false end
local p = redis.call('GET', KEYS[2])
if not p then return false end
if redis.call('EXISTS', KEYS[1]) == 1 then return false end
redis.call('SET', KEYS[1], ARGV[1], 'EX', ARGV[2])
redis.call('INCRBY', KEYS[1], p)
redis.call('DEL', KEYS[2])
local g = redis.call('INCRBY', KEYS[3], 1)
redis.call('SET', KEYS[3], g, 'EX', ARGV[4])
return redis.call('GET', KEYS[1])
"#;

/// KEYS: total, pend, gen. ARGV: gen ttl. Returns the new generation.
pub const INVALIDATE: &str = r#"
redis.call('DEL', KEYS[1])
redis.call('DEL', KEYS[2])
local g = redis.call('INCRBY', KEYS[3], 1)
redis.call('SET', KEYS[3], g, 'EX', ARGV[1])
return g
"#;
