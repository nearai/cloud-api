use super::parse_u64_env;

/// Cache in front of the per-request API-key spend check.
///
/// `API_KEY_SPEND_CACHE_TTL_SECS` is how long a spend reading may be reused
/// (default [`Self::DEFAULT_TTL_SECS`]). `0` turns the cache off: every
/// check reads the database, as before the cache existed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ApiKeySpendCacheConfig {
    pub ttl_secs: u64,
}

impl ApiKeySpendCacheConfig {
    pub const DEFAULT_TTL_SECS: u64 = 10;
    /// Upper bound, so a typo cannot make spend limits unenforceable for hours.
    pub const MAX_TTL_SECS: u64 = 300;

    pub fn from_env() -> Result<Self, String> {
        let ttl_secs = parse_u64_env("API_KEY_SPEND_CACHE_TTL_SECS", Self::DEFAULT_TTL_SECS)?;
        Self::new(ttl_secs)
    }

    pub fn new(ttl_secs: u64) -> Result<Self, String> {
        if ttl_secs > Self::MAX_TTL_SECS {
            return Err(format!(
                "API_KEY_SPEND_CACHE_TTL_SECS must be at most {}",
                Self::MAX_TTL_SECS
            ));
        }
        Ok(Self { ttl_secs })
    }

    /// The cache is off: every check reads the database.
    pub fn disabled(&self) -> bool {
        self.ttl_secs == 0
    }
}

impl Default for ApiKeySpendCacheConfig {
    fn default() -> Self {
        Self {
            ttl_secs: Self::DEFAULT_TTL_SECS,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_is_ten_seconds_and_enabled() {
        let config = ApiKeySpendCacheConfig::default();
        assert_eq!(config.ttl_secs, 10);
        assert!(!config.disabled());
    }

    #[test]
    fn zero_disables_and_too_large_is_rejected() {
        assert!(ApiKeySpendCacheConfig::new(0).unwrap().disabled());
        assert!(ApiKeySpendCacheConfig::new(300).is_ok());
        assert!(ApiKeySpendCacheConfig::new(301).is_err());
    }

    #[test]
    fn from_env_reads_ttl() {
        // Only this test touches the variable.
        static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
        let _guard = LOCK.lock().unwrap_or_else(|e| e.into_inner());
        std::env::set_var("API_KEY_SPEND_CACHE_TTL_SECS", " 3 ");
        let set = ApiKeySpendCacheConfig::from_env().map(|c| c.ttl_secs);
        std::env::set_var("API_KEY_SPEND_CACHE_TTL_SECS", "abc");
        let bad = ApiKeySpendCacheConfig::from_env();
        std::env::remove_var("API_KEY_SPEND_CACHE_TTL_SECS");
        let unset = ApiKeySpendCacheConfig::from_env().map(|c| c.ttl_secs);
        assert_eq!(set, Ok(3));
        assert!(bad.is_err());
        assert_eq!(unset, Ok(10));
    }
}
