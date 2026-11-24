use super::parse_u64_env;

/// `API_KEY_SPEND_BUDGET_MODE`: how the per-request spend check reads spend.
///
/// - `off`: the TTL cache described on [`ApiKeySpendCacheConfig`].
/// - `on`: a local budget. The database is read only after a key has used a
///   share of its remaining headroom, and a key that has used its share can be
///   answered with 429 until the next read. Enforcement is approximate.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum SpendBudgetMode {
    #[default]
    Off,
    On,
}

impl SpendBudgetMode {
    /// Strict parse: exactly `off` or `on` (surrounding whitespace is
    /// ignored; case is not).
    pub fn parse(raw: &str) -> Result<Self, String> {
        match raw.trim() {
            "off" => Ok(Self::Off),
            "on" => Ok(Self::On),
            _ => Err("API_KEY_SPEND_BUDGET_MODE must be one of: off, on".to_string()),
        }
    }
}

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
///
/// The local budget adds `API_KEY_SPEND_BUDGET_MODE` (`off` by default),
/// `API_KEY_SPEND_BUDGET_SHARE_PERCENT` (the share of a key's remaining
/// headroom one process may spend between database reads, default
/// [`Self::DEFAULT_BUDGET_SHARE_PERCENT`], 1 to
/// [`Self::MAX_BUDGET_SHARE_PERCENT`]) and `API_KEY_SPEND_BUDGET_MAX_AGE_SECS`
/// (how old a reading may get before a refresh starts, default
/// [`Self::DEFAULT_BUDGET_MAX_AGE_SECS`], 10 to
/// [`Self::MAX_BUDGET_MAX_AGE_SECS`]).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ApiKeySpendCacheConfig {
    pub ttl_secs: u64,
    pub read_timeout_secs: u64,
    pub budget_mode: SpendBudgetMode,
    pub budget_share_percent: u64,
    pub budget_max_age_secs: u64,
}

impl ApiKeySpendCacheConfig {
    pub const DEFAULT_TTL_SECS: u64 = 10;
    /// Upper bound, so a typo cannot make spend limits unenforceable for hours.
    pub const MAX_TTL_SECS: u64 = 300;
    pub const DEFAULT_READ_TIMEOUT_SECS: u64 = 30;
    pub const MIN_READ_TIMEOUT_SECS: u64 = 1;
    pub const MAX_READ_TIMEOUT_SECS: u64 = 300;
    pub const DEFAULT_BUDGET_SHARE_PERCENT: u64 = 25;
    pub const MIN_BUDGET_SHARE_PERCENT: u64 = 1;
    pub const MAX_BUDGET_SHARE_PERCENT: u64 = 50;
    pub const DEFAULT_BUDGET_MAX_AGE_SECS: u64 = 60;
    pub const MIN_BUDGET_MAX_AGE_SECS: u64 = 10;
    pub const MAX_BUDGET_MAX_AGE_SECS: u64 = 3_600;

    pub fn from_env() -> Result<Self, String> {
        let ttl_secs = parse_u64_env("API_KEY_SPEND_CACHE_TTL_SECS", Self::DEFAULT_TTL_SECS)?;
        let read_timeout_secs = parse_u64_env(
            "API_KEY_SPEND_READ_TIMEOUT_SECS",
            Self::DEFAULT_READ_TIMEOUT_SECS,
        )?;
        let budget_mode = match std::env::var("API_KEY_SPEND_BUDGET_MODE") {
            Ok(raw) => SpendBudgetMode::parse(&raw)?,
            Err(_) => SpendBudgetMode::Off,
        };
        let budget_share_percent = parse_u64_env(
            "API_KEY_SPEND_BUDGET_SHARE_PERCENT",
            Self::DEFAULT_BUDGET_SHARE_PERCENT,
        )?;
        let budget_max_age_secs = parse_u64_env(
            "API_KEY_SPEND_BUDGET_MAX_AGE_SECS",
            Self::DEFAULT_BUDGET_MAX_AGE_SECS,
        )?;
        Self::new(ttl_secs, read_timeout_secs)?.with_budget(
            budget_mode,
            budget_share_percent,
            budget_max_age_secs,
        )
    }

    /// Sets the local budget settings, checking their bounds.
    pub fn with_budget(
        mut self,
        mode: SpendBudgetMode,
        share_percent: u64,
        max_age_secs: u64,
    ) -> Result<Self, String> {
        if !(Self::MIN_BUDGET_SHARE_PERCENT..=Self::MAX_BUDGET_SHARE_PERCENT)
            .contains(&share_percent)
        {
            return Err(format!(
                "API_KEY_SPEND_BUDGET_SHARE_PERCENT must be between {} and {}",
                Self::MIN_BUDGET_SHARE_PERCENT,
                Self::MAX_BUDGET_SHARE_PERCENT
            ));
        }
        if !(Self::MIN_BUDGET_MAX_AGE_SECS..=Self::MAX_BUDGET_MAX_AGE_SECS).contains(&max_age_secs)
        {
            return Err(format!(
                "API_KEY_SPEND_BUDGET_MAX_AGE_SECS must be between {} and {}",
                Self::MIN_BUDGET_MAX_AGE_SECS,
                Self::MAX_BUDGET_MAX_AGE_SECS
            ));
        }
        self.budget_mode = mode;
        self.budget_share_percent = share_percent;
        self.budget_max_age_secs = max_age_secs;
        Ok(self)
    }

    /// The local budget is on.
    pub fn budget_enabled(&self) -> bool {
        self.budget_mode == SpendBudgetMode::On
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
            ..Self::default()
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
            budget_mode: SpendBudgetMode::Off,
            budget_share_percent: Self::DEFAULT_BUDGET_SHARE_PERCENT,
            budget_max_age_secs: Self::DEFAULT_BUDGET_MAX_AGE_SECS,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serial_test::serial;
    use std::ffi::OsString;

    const KEYS: [&str; 5] = [
        "API_KEY_SPEND_CACHE_TTL_SECS",
        "API_KEY_SPEND_READ_TIMEOUT_SECS",
        "API_KEY_SPEND_BUDGET_MODE",
        "API_KEY_SPEND_BUDGET_SHARE_PERCENT",
        "API_KEY_SPEND_BUDGET_MAX_AGE_SECS",
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

    #[test]
    fn budget_defaults_to_off_with_twenty_five_percent_and_one_minute() {
        let config = ApiKeySpendCacheConfig::default();
        assert_eq!(config.budget_mode, SpendBudgetMode::Off);
        assert!(!config.budget_enabled());
        assert_eq!(config.budget_share_percent, 25);
        assert_eq!(config.budget_max_age_secs, 60);
    }

    #[test]
    fn budget_bounds_are_checked() {
        let on = SpendBudgetMode::On;
        let base = ApiKeySpendCacheConfig::default();
        assert!(base.clone().with_budget(on, 1, 10).is_ok());
        assert!(base.clone().with_budget(on, 50, 3_600).is_ok());
        for share in [0, 51, 100] {
            assert!(base.clone().with_budget(on, share, 60).is_err(), "{share}");
        }
        for age in [0, 9, 3_601] {
            assert!(base.clone().with_budget(on, 10, age).is_err(), "{age}");
        }
        assert!(base.with_budget(on, 10, 60).unwrap().budget_enabled());
    }

    #[test]
    fn budget_mode_parse_is_strict() {
        assert_eq!(SpendBudgetMode::parse(" on "), Ok(SpendBudgetMode::On));
        assert_eq!(SpendBudgetMode::parse("off"), Ok(SpendBudgetMode::Off));
        for bad in ["", "ON", "Off", "shadow", "true", "1"] {
            assert!(SpendBudgetMode::parse(bad).is_err(), "value {bad:?}");
        }
    }

    #[test]
    #[serial]
    fn from_env_reads_the_budget_settings_and_rejects_bad_values() {
        let _env = Environment::new();
        let unset = ApiKeySpendCacheConfig::from_env().unwrap();
        assert_eq!(unset.budget_mode, SpendBudgetMode::Off);
        assert_eq!(unset.budget_share_percent, 25);
        assert_eq!(unset.budget_max_age_secs, 60);

        std::env::set_var("API_KEY_SPEND_BUDGET_MODE", " on ");
        std::env::set_var("API_KEY_SPEND_BUDGET_SHARE_PERCENT", " 30 ");
        std::env::set_var("API_KEY_SPEND_BUDGET_MAX_AGE_SECS", "60");
        let set = ApiKeySpendCacheConfig::from_env().unwrap();
        assert!(set.budget_enabled());
        assert_eq!(set.budget_share_percent, 30);
        assert_eq!(set.budget_max_age_secs, 60);

        for (key, bad) in [
            ("API_KEY_SPEND_BUDGET_MODE", "maybe"),
            ("API_KEY_SPEND_BUDGET_SHARE_PERCENT", "0"),
            ("API_KEY_SPEND_BUDGET_SHARE_PERCENT", "51"),
            ("API_KEY_SPEND_BUDGET_SHARE_PERCENT", "abc"),
            ("API_KEY_SPEND_BUDGET_MAX_AGE_SECS", "9"),
            ("API_KEY_SPEND_BUDGET_MAX_AGE_SECS", "3601"),
            ("API_KEY_SPEND_BUDGET_MAX_AGE_SECS", "-1"),
        ] {
            let previous = std::env::var_os(key);
            std::env::set_var(key, bad);
            assert!(ApiKeySpendCacheConfig::from_env().is_err(), "{key}={bad:?}");
            match previous {
                Some(value) => std::env::set_var(key, value),
                None => std::env::remove_var(key),
            }
        }
    }
}
