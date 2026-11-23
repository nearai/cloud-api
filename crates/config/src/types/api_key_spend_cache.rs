use super::{parse_u64_env, PlacementConfig, PlacementEndpoint};

/// Whether and how the shared (Valkey) running spend total is used.
///
/// `API_KEY_SPEND_TOTAL_MODE`: `off` (default), `shadow` or `on`. A closed
/// set: any other value fails config load.
///
/// - `off`: nothing new runs.
/// - `shadow`: decisions still use the database; the shared total is
///   maintained and compared with the database reading.
/// - `on`: a shared total below the limit replaces the database read. A total
///   at or over the limit is always confirmed against the database.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum SpendTotalMode {
    #[default]
    Off,
    Shadow,
    On,
}

impl SpendTotalMode {
    /// Strict parse: exactly `off`, `shadow` or `on` (surrounding whitespace
    /// is ignored; case is not).
    pub fn parse(raw: &str) -> Result<Self, String> {
        match raw.trim() {
            "off" => Ok(Self::Off),
            "shadow" => Ok(Self::Shadow),
            "on" => Ok(Self::On),
            _ => Err("API_KEY_SPEND_TOTAL_MODE must be one of: off, shadow, on".to_string()),
        }
    }
}

/// Cache in front of the per-request API-key spend check.
///
/// `API_KEY_SPEND_CACHE_TTL_SECS` is how long a spend reading may be reused
/// (default [`Self::DEFAULT_TTL_SECS`]). `0` turns the cache off: every
/// check reads the database, as before the cache existed.
///
/// The shared running total adds `API_KEY_SPEND_TOTAL_MODE`,
/// `API_KEY_SPEND_TOTAL_RECHECK_SECS` (how long a stored total is trusted
/// before the database is summed again) and `ENVIRONMENT` (namespaces the
/// stored keys). The Valkey endpoint and password are the placement ones.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ApiKeySpendCacheConfig {
    pub ttl_secs: u64,
    pub total_mode: SpendTotalMode,
    pub total_recheck_secs: u64,
    /// `ENVIRONMENT`, trimmed; `None` when unset or empty.
    pub environment: Option<String>,
}

impl ApiKeySpendCacheConfig {
    pub const DEFAULT_TTL_SECS: u64 = 10;
    /// Upper bound, so a typo cannot make spend limits unenforceable for hours.
    pub const MAX_TTL_SECS: u64 = 300;
    pub const DEFAULT_RECHECK_SECS: u64 = 600;
    pub const MIN_RECHECK_SECS: u64 = 60;
    pub const MAX_RECHECK_SECS: u64 = 3600;

    pub fn from_env() -> Result<Self, String> {
        let ttl_secs = parse_u64_env("API_KEY_SPEND_CACHE_TTL_SECS", Self::DEFAULT_TTL_SECS)?;
        let total_mode = match std::env::var("API_KEY_SPEND_TOTAL_MODE") {
            Ok(raw) => SpendTotalMode::parse(&raw)?,
            Err(_) => SpendTotalMode::Off,
        };
        let total_recheck_secs = parse_u64_env(
            "API_KEY_SPEND_TOTAL_RECHECK_SECS",
            Self::DEFAULT_RECHECK_SECS,
        )?;
        let environment = std::env::var("ENVIRONMENT").ok();
        Self::from_parts(ttl_secs, total_mode, total_recheck_secs, environment)
    }

    /// The cache with the shared total off.
    pub fn new(ttl_secs: u64) -> Result<Self, String> {
        Self::from_parts(
            ttl_secs,
            SpendTotalMode::Off,
            Self::DEFAULT_RECHECK_SECS,
            None,
        )
    }

    pub fn from_parts(
        ttl_secs: u64,
        total_mode: SpendTotalMode,
        total_recheck_secs: u64,
        environment: Option<String>,
    ) -> Result<Self, String> {
        if ttl_secs > Self::MAX_TTL_SECS {
            return Err(format!(
                "API_KEY_SPEND_CACHE_TTL_SECS must be at most {}",
                Self::MAX_TTL_SECS
            ));
        }
        if !(Self::MIN_RECHECK_SECS..=Self::MAX_RECHECK_SECS).contains(&total_recheck_secs) {
            return Err(format!(
                "API_KEY_SPEND_TOTAL_RECHECK_SECS must be between {} and {}",
                Self::MIN_RECHECK_SECS,
                Self::MAX_RECHECK_SECS
            ));
        }
        let environment = environment
            .map(|e| e.trim().to_string())
            .filter(|e| !e.is_empty());
        Ok(Self {
            ttl_secs,
            total_mode,
            total_recheck_secs,
            environment,
        })
    }

    /// The cache is off: every check reads the database.
    pub fn disabled(&self) -> bool {
        self.ttl_secs == 0
    }

    /// The mode that actually runs. `Ok(Off)` when it is off by configuration;
    /// `Err(reason)` when `shadow` or `on` was asked for but cannot run (the
    /// caller logs the static reason once and behaves as `off`). Reasons name
    /// the setting, never a value.
    pub fn effective_total_mode(
        &self,
        placement: &PlacementConfig,
    ) -> Result<SpendTotalMode, &'static str> {
        if self.total_mode == SpendTotalMode::Off {
            return Ok(SpendTotalMode::Off);
        }
        if self.disabled() {
            return Err("API_KEY_SPEND_CACHE_TTL_SECS is 0");
        }
        if self.environment.is_none() {
            return Err("ENVIRONMENT is not set");
        }
        if placement.redis_password.is_none() {
            return Err("placement Valkey password is not configured");
        }
        match placement.redis_endpoint {
            PlacementEndpoint::Valid(_) => Ok(self.total_mode),
            PlacementEndpoint::Missing => Err("placement Valkey endpoint is not configured"),
            PlacementEndpoint::Invalid(_) => Err("placement Valkey endpoint is invalid"),
        }
    }
}

impl Default for ApiKeySpendCacheConfig {
    fn default() -> Self {
        Self {
            ttl_secs: Self::DEFAULT_TTL_SECS,
            total_mode: SpendTotalMode::Off,
            total_recheck_secs: Self::DEFAULT_RECHECK_SECS,
            environment: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The `from_env` tests share process-wide variables.
    static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

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

    fn placement(valid: bool, password: bool) -> PlacementConfig {
        PlacementConfig {
            redis_password: password.then(|| "pw".to_string()),
            redis_endpoint: if valid {
                PlacementEndpoint::Valid(super::super::PlacementRedisEndpoint {
                    host: "127.0.0.1".to_string(),
                    port: 6379,
                    tls: false,
                    ca_pem: None,
                })
            } else {
                PlacementEndpoint::Missing
            },
        }
    }

    fn with_mode(mode: SpendTotalMode, ttl: u64, env: Option<&str>) -> ApiKeySpendCacheConfig {
        ApiKeySpendCacheConfig::from_parts(ttl, mode, 600, env.map(str::to_string)).unwrap()
    }

    #[test]
    fn mode_parsing_is_strict() {
        assert_eq!(SpendTotalMode::parse("off"), Ok(SpendTotalMode::Off));
        assert_eq!(
            SpendTotalMode::parse(" shadow "),
            Ok(SpendTotalMode::Shadow)
        );
        assert_eq!(SpendTotalMode::parse("on"), Ok(SpendTotalMode::On));
        for bad in ["", "ON", "Shadow", "true", "1", "enabled", "of f"] {
            assert!(SpendTotalMode::parse(bad).is_err(), "{bad:?} must fail");
        }
    }

    #[test]
    fn recheck_bounds_are_enforced() {
        let make =
            |secs| ApiKeySpendCacheConfig::from_parts(10, SpendTotalMode::Off, secs, None).is_ok();
        assert!(!make(59));
        assert!(make(60));
        assert!(make(3600));
        assert!(!make(3601));
        assert_eq!(ApiKeySpendCacheConfig::default().total_recheck_secs, 600);
    }

    #[test]
    fn off_by_default_and_environment_is_trimmed() {
        let config = ApiKeySpendCacheConfig::default();
        assert_eq!(config.total_mode, SpendTotalMode::Off);
        assert_eq!(
            config.effective_total_mode(&placement(true, true)),
            Ok(SpendTotalMode::Off)
        );
        assert_eq!(
            with_mode(SpendTotalMode::On, 10, Some("  ")).environment,
            None
        );
        assert_eq!(
            with_mode(SpendTotalMode::On, 10, Some(" env "))
                .environment
                .as_deref(),
            Some("env")
        );
    }

    #[test]
    fn shadow_and_on_fall_back_to_off_when_they_cannot_run() {
        for mode in [SpendTotalMode::Shadow, SpendTotalMode::On] {
            let ok = with_mode(mode, 10, Some("e"));
            assert_eq!(ok.effective_total_mode(&placement(true, true)), Ok(mode));
            // Each missing precondition is an error with a static reason.
            assert!(with_mode(mode, 0, Some("e"))
                .effective_total_mode(&placement(true, true))
                .is_err());
            assert!(with_mode(mode, 10, None)
                .effective_total_mode(&placement(true, true))
                .is_err());
            assert!(ok.effective_total_mode(&placement(false, true)).is_err());
            assert!(ok.effective_total_mode(&placement(true, false)).is_err());
            let invalid = PlacementConfig {
                redis_password: Some("pw".into()),
                redis_endpoint: PlacementEndpoint::Invalid("bad".into()),
            };
            assert!(ok.effective_total_mode(&invalid).is_err());
        }
    }

    #[test]
    fn from_env_reads_total_settings_and_rejects_bad_values() {
        let _guard = LOCK.lock().unwrap_or_else(|e| e.into_inner());
        std::env::set_var("API_KEY_SPEND_TOTAL_MODE", "shadow");
        std::env::set_var("API_KEY_SPEND_TOTAL_RECHECK_SECS", "120");
        std::env::set_var("ENVIRONMENT", "test-env");
        let good = ApiKeySpendCacheConfig::from_env();
        std::env::set_var("API_KEY_SPEND_TOTAL_MODE", "maybe");
        let bad_mode = ApiKeySpendCacheConfig::from_env();
        std::env::set_var("API_KEY_SPEND_TOTAL_MODE", "on");
        std::env::set_var("API_KEY_SPEND_TOTAL_RECHECK_SECS", "5");
        let bad_recheck = ApiKeySpendCacheConfig::from_env();
        std::env::remove_var("API_KEY_SPEND_TOTAL_MODE");
        std::env::remove_var("API_KEY_SPEND_TOTAL_RECHECK_SECS");
        std::env::remove_var("ENVIRONMENT");
        let unset = ApiKeySpendCacheConfig::from_env();
        let good = good.unwrap();
        assert_eq!(good.total_mode, SpendTotalMode::Shadow);
        assert_eq!(good.total_recheck_secs, 120);
        assert_eq!(good.environment.as_deref(), Some("test-env"));
        assert!(bad_mode.is_err());
        assert!(bad_recheck.is_err());
        assert_eq!(unset.unwrap().total_mode, SpendTotalMode::Off);
    }
}
