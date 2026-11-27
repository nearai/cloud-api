use super::parse_u64_env;

/// `API_KEY_SPEND_BUDGET_MODE`: how the per-request spend check reads spend.
///
/// - `on` (the default): a local budget. The database is read only after a
///   key has used a share of its remaining headroom, and a key that has used
///   its share can be answered with 429 until the next read. Enforcement is
///   approximate.
/// - `off`: every check reads the database.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum SpendBudgetMode {
    #[default]
    On,
    Off,
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

/// Settings of the per-request API-key spend check: `API_KEY_SPEND_BUDGET_MODE`
/// and `API_KEY_SPEND_BUDGET_MAX_AGE_SECS` (how old a reading may get before a
/// refresh starts, default [`Self::DEFAULT_BUDGET_MAX_AGE_SECS`], 10 to
/// [`Self::MAX_BUDGET_MAX_AGE_SECS`]).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ApiKeySpendCacheConfig {
    pub budget_mode: SpendBudgetMode,
    pub budget_max_age_secs: u64,
}

impl ApiKeySpendCacheConfig {
    pub const DEFAULT_BUDGET_MAX_AGE_SECS: u64 = 60;
    pub const MIN_BUDGET_MAX_AGE_SECS: u64 = 10;
    pub const MAX_BUDGET_MAX_AGE_SECS: u64 = 3_600;

    pub fn from_env() -> Result<Self, String> {
        let budget_mode = match std::env::var("API_KEY_SPEND_BUDGET_MODE") {
            Ok(raw) => SpendBudgetMode::parse(&raw)?,
            Err(_) => SpendBudgetMode::default(),
        };
        let budget_max_age_secs = parse_u64_env(
            "API_KEY_SPEND_BUDGET_MAX_AGE_SECS",
            Self::DEFAULT_BUDGET_MAX_AGE_SECS,
        )?;
        Self::new(budget_mode, budget_max_age_secs)
    }

    /// Checks the bounds of the settings.
    pub fn new(mode: SpendBudgetMode, max_age_secs: u64) -> Result<Self, String> {
        if !(Self::MIN_BUDGET_MAX_AGE_SECS..=Self::MAX_BUDGET_MAX_AGE_SECS).contains(&max_age_secs)
        {
            return Err(format!(
                "API_KEY_SPEND_BUDGET_MAX_AGE_SECS must be between {} and {}",
                Self::MIN_BUDGET_MAX_AGE_SECS,
                Self::MAX_BUDGET_MAX_AGE_SECS
            ));
        }
        Ok(Self {
            budget_mode: mode,
            budget_max_age_secs: max_age_secs,
        })
    }

    /// The local budget is on.
    pub fn budget_enabled(&self) -> bool {
        self.budget_mode == SpendBudgetMode::On
    }
}

impl Default for ApiKeySpendCacheConfig {
    fn default() -> Self {
        Self {
            budget_mode: SpendBudgetMode::default(),
            budget_max_age_secs: Self::DEFAULT_BUDGET_MAX_AGE_SECS,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serial_test::serial;
    use std::ffi::OsString;

    const KEYS: [&str; 2] = [
        "API_KEY_SPEND_BUDGET_MODE",
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
    fn defaults_are_on_and_one_minute() {
        let config = ApiKeySpendCacheConfig::default();
        assert_eq!(config.budget_mode, SpendBudgetMode::On);
        assert!(config.budget_enabled());
        assert_eq!(config.budget_max_age_secs, 60);
    }

    #[test]
    fn max_age_bounds_are_checked() {
        for age in [10, 60, 3_600] {
            assert!(ApiKeySpendCacheConfig::new(SpendBudgetMode::On, age).is_ok());
        }
        for age in [0, 9, 3_601] {
            assert!(ApiKeySpendCacheConfig::new(SpendBudgetMode::On, age).is_err());
        }
        let off = ApiKeySpendCacheConfig::new(SpendBudgetMode::Off, 60).unwrap();
        assert!(!off.budget_enabled());
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
    fn from_env_reads_the_settings_and_rejects_bad_values() {
        let _env = Environment::new();
        assert_eq!(
            ApiKeySpendCacheConfig::from_env().unwrap(),
            ApiKeySpendCacheConfig::default()
        );

        std::env::set_var("API_KEY_SPEND_BUDGET_MODE", " off ");
        std::env::set_var("API_KEY_SPEND_BUDGET_MAX_AGE_SECS", " 120 ");
        let set = ApiKeySpendCacheConfig::from_env().unwrap();
        assert!(!set.budget_enabled());
        assert_eq!(set.budget_max_age_secs, 120);

        for (key, bad) in [
            ("API_KEY_SPEND_BUDGET_MODE", "maybe"),
            ("API_KEY_SPEND_BUDGET_MAX_AGE_SECS", "9"),
            ("API_KEY_SPEND_BUDGET_MAX_AGE_SECS", "3601"),
            ("API_KEY_SPEND_BUDGET_MAX_AGE_SECS", "abc"),
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
