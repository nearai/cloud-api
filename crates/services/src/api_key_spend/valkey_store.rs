//! [`SpendTotalStore`] over the placement Valkey (ACL user `router`).

use async_trait::async_trait;
use inference_providers::valkey_kv::{KvErrorKind, KvReply, ValkeyKv};
use uuid::Uuid;

use super::lua;
use super::ports::{SpendTotalStore, StoreErrorKind, TotalRead};

/// Jitter of the total's expiry around the re-check interval, in percent.
const JITTER_PERCENT: u64 = 10;

pub struct ValkeyTotalStore {
    kv: ValkeyKv,
    environment: String,
    recheck_secs: u64,
}

impl ValkeyTotalStore {
    pub fn new(kv: ValkeyKv, environment: String, recheck_secs: u64) -> Self {
        Self {
            kv,
            environment,
            recheck_secs,
        }
    }

    fn total(&self, id: Uuid) -> String {
        lua::total_key(&self.environment, id)
    }
    fn pend(&self, id: Uuid) -> String {
        lua::pend_key(&self.environment, id)
    }
    fn gen(&self, id: Uuid) -> String {
        lua::gen_key(&self.environment, id)
    }
}

/// The re-check interval with +-10% jitter chosen from `entropy`, so totals
/// seeded together do not all expire together.
fn jittered_ttl_secs(recheck_secs: u64, entropy: u128) -> u64 {
    let spread = recheck_secs * JITTER_PERCENT / 100;
    let span = u128::from(spread * 2 + 1);
    recheck_secs - spread + (entropy % span) as u64
}

fn kind(k: KvErrorKind) -> StoreErrorKind {
    match k {
        KvErrorKind::Timeout => StoreErrorKind::Timeout,
        KvErrorKind::NoPerm => StoreErrorKind::NoPerm,
        KvErrorKind::Conn => StoreErrorKind::Conn,
        KvErrorKind::Other => StoreErrorKind::Other,
    }
}

/// A canonical non-negative decimal `i64` (`"007"`, `"+5"`, `"-1"` and
/// `"1e3"` are not).
pub(super) fn parse_total(raw: &str) -> Option<i64> {
    let v: i64 = raw.parse().ok()?;
    (v >= 0 && v.to_string() == raw).then_some(v)
}

#[async_trait]
impl SpendTotalStore for ValkeyTotalStore {
    async fn get_total(&self, id: Uuid) -> Result<TotalRead, StoreErrorKind> {
        match self.kv.get(&self.total(id)).await.map_err(kind)? {
            None => Ok(TotalRead::Missing),
            Some(raw) => Ok(parse_total(&raw).map_or(TotalRead::Malformed, TotalRead::Value)),
        }
    }

    async fn begin(&self, id: Uuid) -> Result<String, StoreErrorKind> {
        let reply = self
            .kv
            .eval(
                lua::BEGIN,
                &[self.pend(id), self.gen(id)],
                &[
                    lua::PEND_TTL_SECS.to_string(),
                    lua::GEN_TTL_SECS.to_string(),
                ],
            )
            .await
            .map_err(kind)?;
        match reply {
            KvReply::Str(g) => Ok(g),
            _ => Err(StoreErrorKind::Other),
        }
    }

    async fn add(&self, id: Uuid, cost: i64) -> Result<u8, StoreErrorKind> {
        let reply = self
            .kv
            .eval(
                lua::ADD,
                &[self.total(id), self.pend(id)],
                &[cost.to_string()],
            )
            .await
            .map_err(kind)?;
        match reply {
            KvReply::Int(n) if (0..=3).contains(&n) => Ok(n as u8),
            _ => Err(StoreErrorKind::Other),
        }
    }

    async fn finish(
        &self,
        id: Uuid,
        sum: i64,
        expected_generation: &str,
    ) -> Result<Option<i64>, StoreErrorKind> {
        let reply = self
            .kv
            .eval(
                lua::FINISH,
                &[self.total(id), self.pend(id), self.gen(id)],
                &[
                    sum.to_string(),
                    jittered_ttl_secs(self.recheck_secs, Uuid::new_v4().as_u128()).to_string(),
                    expected_generation.to_string(),
                    lua::GEN_TTL_SECS.to_string(),
                ],
            )
            .await
            .map_err(kind)?;
        match reply {
            KvReply::Nil => Ok(None),
            KvReply::Str(s) => s.parse().map(Some).map_err(|_| StoreErrorKind::Other),
            KvReply::Int(n) => Ok(Some(n)),
        }
    }

    async fn invalidate(&self, id: Uuid) -> Result<(), StoreErrorKind> {
        self.kv
            .eval(
                lua::INVALIDATE,
                &[self.total(id), self.pend(id), self.gen(id)],
                &[lua::GEN_TTL_SECS.to_string()],
            )
            .await
            .map_err(kind)
            .map(|_| ())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn totals_must_be_canonical_non_negative_decimals() {
        assert_eq!(parse_total("0"), Some(0));
        assert_eq!(parse_total("9007199254740995"), Some(9_007_199_254_740_995));
        assert_eq!(parse_total(&i64::MAX.to_string()), Some(i64::MAX));
        for bad in [
            "",
            "-1",
            "-0",
            "+5",
            "007",
            "1.0",
            "1e3",
            " 5",
            "5 ",
            "abc",
            "9223372036854775808",
        ] {
            assert_eq!(parse_total(bad), None, "{bad:?}");
        }
    }

    #[test]
    fn expiry_jitter_stays_within_ten_percent() {
        let seen: Vec<u64> = (0..1000u128).map(|e| jittered_ttl_secs(600, e)).collect();
        assert_eq!(seen.iter().min(), Some(&540));
        assert_eq!(seen.iter().max(), Some(&660));
        assert_eq!(jittered_ttl_secs(60, 0), 54);
        assert_eq!(
            jittered_ttl_secs(3600, u128::MAX),
            3600 - 360 + (u128::MAX % 721) as u64
        );
    }

    #[test]
    fn keys_are_namespaced_by_environment_and_key_id() {
        let id = Uuid::nil();
        assert_eq!(
            lua::total_key("e", id),
            "spend:v2:e:00000000-0000-0000-0000-000000000000"
        );
        assert!(lua::pend_key("e", id).ends_with(":pend"));
        assert!(lua::gen_key("e", id).ends_with(":gen"));
        assert!(lua::total_key("e", id).starts_with("spend:"));
    }
}
