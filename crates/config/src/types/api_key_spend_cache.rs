use super::parse_u64_env;

/// Cache in front of the per-request API-key spend check.
///
/// `API_KEY_SPEND_CACHE_TTL_SECS` is how long a spend reading may be reused
/// (default [`Self::DEFAULT_TTL_SECS`]). `0` turns the cache off: every
/// check reads the database, as before the cache existed.
///
/// `API_KEY_SPEND_READ_TIMEOUT_SECS` bounds one database read behind the
/// cache (default [`Self::DEFAULT_READ_TIMEOUT_SECS`], 1 to
/// [`Self::MAX_READ_TIMEOUT_SECS`]). A read that exceeds it fails like any
/// other failed read and is not cached. It applies with the cache off too.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ApiKeySpendCacheConfig {
    pub ttl_secs: u64,
    pub read_timeout_secs: u64,
}

impl ApiKeySpendCacheConfig {
    pub const DEFAULT_TTL_SECS: u64 = 10;
    /// Upper bound, so a typo cannot make spend limits unenforceable for hours.
    pub const MAX_TTL_SECS: u64 = 300;
    pub const DEFAULT_READ_TIMEOUT_SECS: u64 = 30;
    pub const MIN_READ_TIMEOUT_SECS: u64 = 1;
    pub const MAX_READ_TIMEOUT_SECS: u64 = 300;

    pub fn from_env() -> Result<Self, String> {
        let ttl_secs = parse_u64_env("API_KEY_SPEND_CACHE_TTL_SECS", Self::DEFAULT_TTL_SECS)?;
        let read_timeout_secs = parse_u64_env(
            "API_KEY_SPEND_READ_TIMEOUT_SECS",
            Self::DEFAULT_READ_TIMEOUT_SECS,
        )?;
        Self::new(ttl_secs, read_timeout_secs)
    }

    pub fn new(ttl_secs: u64, read_timeout_secs: u64) -> Result<Self, String> {
        if ttl_secs > Self::MAX_TTL_SECS {
            return Err(format!(
                "API_KEY_SPEND_CACHE_TTL_SECS must be at most {}",
                Self::MAX_TTL_SECS
            ));
        }
        if !(Self::MIN_READ_TIMEOUT_SECS..=Self::MAX_READ_TIMEOUT_SECS).contains(&read_timeout_secs)
        {
            return Err(format!(
                "API_KEY_SPEND_READ_TIMEOUT_SECS must be between {} and {}",
                Self::MIN_READ_TIMEOUT_SECS,
                Self::MAX_READ_TIMEOUT_SECS
            ));
        }
        Ok(Self {
            ttl_secs,
            read_timeout_secs,
        })
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
            read_timeout_secs: Self::DEFAULT_READ_TIMEOUT_SECS,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serial_test::serial;
    use std::ffi::OsString;

    const KEYS: [&str; 2] = [
        "API_KEY_SPEND_CACHE_TTL_SECS",
        "API_KEY_SPEND_READ_TIMEOUT_SECS",
    ];

    /// Saves the variables on creation, clears them, and restores them on drop.
    struct Environment(Vec<(&'static str, Option<OsString>)>);

    impl Environment {
        fn new() -> Self {
            let saved = Self(KEYS.iter().map(|k| (*k, std::env::var_os(k))).collect());
            for key in KEYS {
                std::env::remove_var(key);
            }
            saved
        }
    }

    impl Drop for Environment {
        fn drop(&mut self) {
            for (key, value) in &self.0 {
                match value {
                    Some(value) => std::env::set_var(key, value),
                    None => std::env::remove_var(key),
                }
            }
        }
    }

    #[test]
    fn default_is_ten_seconds_and_enabled() {
        let config = ApiKeySpendCacheConfig::default();
        assert_eq!(config.ttl_secs, 10);
        assert_eq!(config.read_timeout_secs, 30);
        assert!(!config.disabled());
    }

    #[test]
    fn zero_disables_and_too_large_is_rejected() {
        assert!(ApiKeySpendCacheConfig::new(0, 30).unwrap().disabled());
        assert!(ApiKeySpendCacheConfig::new(300, 30).is_ok());
        assert!(ApiKeySpendCacheConfig::new(301, 30).is_err());
    }

    #[test]
    fn read_timeout_bounds_are_one_to_three_hundred() {
        assert!(ApiKeySpendCacheConfig::new(10, 0).is_err());
        assert_eq!(
            ApiKeySpendCacheConfig::new(10, 1)
                .unwrap()
                .read_timeout_secs,
            1
        );
        assert_eq!(
            ApiKeySpendCacheConfig::new(10, 300)
                .unwrap()
                .read_timeout_secs,
            300
        );
        assert!(ApiKeySpendCacheConfig::new(10, 301).is_err());
    }

    #[test]
    #[serial]
    fn from_env_reads_ttl() {
        let _env = Environment::new();
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

    #[test]
    #[serial]
    fn from_env_reads_read_timeout_and_rejects_bad_values() {
        let _env = Environment::new();
        assert_eq!(
            ApiKeySpendCacheConfig::from_env().map(|c| c.read_timeout_secs),
            Ok(30)
        );
        std::env::set_var("API_KEY_SPEND_READ_TIMEOUT_SECS", " 5 ");
        assert_eq!(
            ApiKeySpendCacheConfig::from_env().map(|c| c.read_timeout_secs),
            Ok(5)
        );
        for bad in ["abc", "0", "301", "-1", ""] {
            std::env::set_var("API_KEY_SPEND_READ_TIMEOUT_SECS", bad);
            assert!(ApiKeySpendCacheConfig::from_env().is_err(), "value {bad:?}");
        }
    }
}
