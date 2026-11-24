//! The key names and expiries of the shared total.
//!
//! The server-side logic is the function library `spend_total.lua` (next to
//! this module); its comments are the contract for the keys and arguments.
//! Totals can exceed 2^53, so the library never does arithmetic on them in
//! Lua: the server adds with `INCRBY`, and values travel as strings.
//!
//! Keys for API key id `{id}` and ACL user `{user}` (the user name is the
//! namespace, so an ACL pattern `~spend:v2:{user}:*` confines the user to it):
//! - total `T` = `spend:v2:{user}:{id}`: decimal `i64`; expires after the
//!   re-check interval, set only by FINISH;
//! - pending `T:pend`: costs recorded while a seed's `SUM` runs (120 s);
//! - generation `T:gen`: a random token replaced by FINISH and INVALIDATE
//!   (3600 s).
//!
//! The three keys of one API key are used together in one function call, so
//! they must live on one server (they do not carry a hash tag).

use uuid::Uuid;

pub const PEND_TTL_SECS: u64 = 120;
pub const GEN_TTL_SECS: u64 = 3600;

pub fn total_key(user: &str, api_key_id: Uuid) -> String {
    format!("spend:v2:{user}:{api_key_id}")
}

pub fn pend_key(user: &str, api_key_id: Uuid) -> String {
    format!("{}:pend", total_key(user, api_key_id))
}

pub fn gen_key(user: &str, api_key_id: Uuid) -> String {
    format!("{}:gen", total_key(user, api_key_id))
}
