use super::{parse_u64_env, read_optional_secret_env, PlacementConfig, PlacementEndpoint};

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
/// before the database is summed again), and its own Valkey login:
/// `API_KEY_SPEND_REDIS_USER` (the ACL user, which is also the namespace of
/// the stored keys) and `API_KEY_SPEND_REDIS_PASSWORD` (or a file via
/// `API_KEY_SPEND_REDIS_PASSWORD_FILE`). Host, port, TLS and CA are the
/// placement endpoint's; the placement login is not used.
#[derive(Clone, PartialEq, Eq)]
pub struct ApiKeySpendCacheConfig {
    pub ttl_secs: u64,
    pub total_mode: SpendTotalMode,
    pub total_recheck_secs: u64,
    /// `API_KEY_SPEND_REDIS_USER`, trimmed and validated; `None` when unset
    /// or empty.
    pub redis_user: Option<String>,
    /// Never logged.
    pub redis_password: Option<String>,
}

impl std::fmt::Debug for ApiKeySpendCacheConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ApiKeySpendCacheConfig")
            .field("ttl_secs", &self.ttl_secs)
            .field("total_mode", &self.total_mode)
            .field("total_recheck_secs", &self.total_recheck_secs)
            .field("redis_user", &self.redis_user)
            .field(
                "redis_password",
                &self.redis_password.as_ref().map(|_| "<redacted>"),
            )
            .finish()
    }
}

/// Longest ACL user name accepted.
pub const MAX_REDIS_USER_LEN: usize = 32;

/// The user name goes into key names and the connection URL: 1 to
/// [`MAX_REDIS_USER_LEN`] characters of `a-z`, `0-9` and `-`.
fn valid_redis_user(user: &str) -> bool {
    (1..=MAX_REDIS_USER_LEN).contains(&user.len())
        && user
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
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
        let redis_user = std::env::var("API_KEY_SPEND_REDIS_USER").ok();
        let redis_password = read_optional_secret_env(
            "API_KEY_SPEND_REDIS_PASSWORD_FILE",
            "API_KEY_SPEND_REDIS_PASSWORD",
        )?;
        Self::from_parts(
            ttl_secs,
            total_mode,
            total_recheck_secs,
            redis_user,
            redis_password,
        )
    }

    /// The cache with the shared total off.
    pub fn new(ttl_secs: u64) -> Result<Self, String> {
        Self::from_parts(
            ttl_secs,
            SpendTotalMode::Off,
            Self::DEFAULT_RECHECK_SECS,
            None,
            None,
        )
    }

    pub fn from_parts(
        ttl_secs: u64,
        total_mode: SpendTotalMode,
        total_recheck_secs: u64,
        redis_user: Option<String>,
        redis_password: Option<String>,
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
        let redis_user = redis_user
            .map(|u| u.trim().to_string())
            .filter(|u| !u.is_empty());
        if redis_user.as_deref().is_some_and(|u| !valid_redis_user(u)) {
            // The message never contains the value.
            return Err(format!(
                "API_KEY_SPEND_REDIS_USER must be 1 to {MAX_REDIS_USER_LEN} characters of a-z, 0-9 and -"
            ));
        }
        let redis_password = redis_password.filter(|p| !p.is_empty());
        Ok(Self {
            ttl_secs,
            total_mode,
            total_recheck_secs,
            redis_user,
            redis_password,
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
        if self.redis_user.is_none() {
            return Err("API_KEY_SPEND_REDIS_USER is not set");
        }
        if self.redis_password.is_none() {
            return Err("API_KEY_SPEND_REDIS_PASSWORD is not set");
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
            redis_user: None,
            redis_password: None,
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

    /// The placement login is deliberately not needed: only the endpoint is.
    fn placement(valid: bool) -> PlacementConfig {
        PlacementConfig {
            redis_password: None,
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

    fn with_mode(
        mode: SpendTotalMode,
        ttl: u64,
        user: Option<&str>,
        password: Option<&str>,
    ) -> ApiKeySpendCacheConfig {
        ApiKeySpendCacheConfig::from_parts(
            ttl,
            mode,
            600,
            user.map(str::to_string),
            password.map(str::to_string),
        )
        .unwrap()
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
        let make = |secs| {
            ApiKeySpendCacheConfig::from_parts(10, SpendTotalMode::Off, secs, None, None).is_ok()
        };
        assert!(!make(59));
        assert!(make(60));
        assert!(make(3600));
        assert!(!make(3601));
        assert_eq!(ApiKeySpendCacheConfig::default().total_recheck_secs, 600);
    }

    #[test]
    fn off_by_default_and_the_user_is_trimmed() {
        let config = ApiKeySpendCacheConfig::default();
        assert_eq!(config.total_mode, SpendTotalMode::Off);
        assert_eq!(
            config.effective_total_mode(&placement(true)),
            Ok(SpendTotalMode::Off)
        );
        let user = |u| with_mode(SpendTotalMode::On, 10, Some(u), Some("pw")).redis_user;
        assert_eq!(user("  "), None);
        assert_eq!(user(" spend-prod ").as_deref(), Some("spend-prod"));
    }

    #[test]
    fn the_user_name_is_validated_because_it_goes_into_keys_and_the_url() {
        let make = |u: &str| {
            ApiKeySpendCacheConfig::from_parts(
                10,
                SpendTotalMode::On,
                600,
                Some(u.to_string()),
                Some("pw".into()),
            )
        };
        for ok in ["spend-prod", "a", "spend-2", &"a".repeat(32)] {
            assert!(make(ok).is_ok(), "{ok:?}");
        }
        for bad in [
            "Spend",
            "a:b",
            "a@b",
            "a/b",
            "a b",
            "a_b",
            "a*",
            "é",
            &"a".repeat(33),
        ] {
            let err = make(bad).unwrap_err();
            assert!(err.contains("API_KEY_SPEND_REDIS_USER"));
            assert!(!err.contains(bad.trim()) || bad.trim().is_empty(), "{err}");
        }
    }

    #[test]
    fn shadow_and_on_fall_back_to_off_when_they_cannot_run() {
        for mode in [SpendTotalMode::Shadow, SpendTotalMode::On] {
            let ok = with_mode(mode, 10, Some("u"), Some("pw"));
            assert_eq!(ok.effective_total_mode(&placement(true)), Ok(mode));
            // Each missing precondition is an error with a static reason.
            assert!(with_mode(mode, 0, Some("u"), Some("pw"))
                .effective_total_mode(&placement(true))
                .is_err());
            assert_eq!(
                with_mode(mode, 10, None, Some("pw")).effective_total_mode(&placement(true)),
                Err("API_KEY_SPEND_REDIS_USER is not set")
            );
            assert_eq!(
                with_mode(mode, 10, Some("u"), None).effective_total_mode(&placement(true)),
                Err("API_KEY_SPEND_REDIS_PASSWORD is not set")
            );
            // An empty password is no password.
            assert!(with_mode(mode, 10, Some("u"), Some(""))
                .effective_total_mode(&placement(true))
                .is_err());
            assert!(ok.effective_total_mode(&placement(false)).is_err());
            let invalid = PlacementConfig {
                redis_password: Some("pw".into()),
                redis_endpoint: PlacementEndpoint::Invalid("bad".into()),
            };
            assert!(ok.effective_total_mode(&invalid).is_err());
        }
    }

    #[test]
    fn the_password_never_appears_in_debug_output() {
        let config = with_mode(
            SpendTotalMode::On,
            10,
            Some("spend-prod"),
            Some("sentinel-spend-secret"),
        );
        let text = format!("{config:?}");
        assert!(!text.contains("sentinel-spend-secret"));
        assert!(text.contains("<redacted>"));
        assert!(text.contains("spend-prod"));
    }

    #[test]
    fn from_env_reads_total_settings_and_rejects_bad_values() {
        let _guard = LOCK.lock().unwrap_or_else(|e| e.into_inner());
        std::env::set_var("API_KEY_SPEND_TOTAL_MODE", "shadow");
        std::env::set_var("API_KEY_SPEND_TOTAL_RECHECK_SECS", "120");
        std::env::set_var("API_KEY_SPEND_REDIS_USER", "spend-test");
        std::env::set_var("API_KEY_SPEND_REDIS_PASSWORD", "  pw  ");
        let good = ApiKeySpendCacheConfig::from_env();
        // The password may come from a file instead.
        let file = std::env::temp_dir().join(format!("spend-pw-{}", std::process::id()));
        std::fs::write(&file, "file-pw\n").unwrap();
        std::env::set_var("API_KEY_SPEND_REDIS_PASSWORD_FILE", &file);
        let from_file = ApiKeySpendCacheConfig::from_env();
        std::env::remove_var("API_KEY_SPEND_REDIS_PASSWORD_FILE");
        std::fs::remove_file(&file).unwrap();
        std::env::set_var("API_KEY_SPEND_TOTAL_MODE", "maybe");
        let bad_mode = ApiKeySpendCacheConfig::from_env();
        std::env::set_var("API_KEY_SPEND_TOTAL_MODE", "on");
        std::env::set_var("API_KEY_SPEND_TOTAL_RECHECK_SECS", "5");
        let bad_recheck = ApiKeySpendCacheConfig::from_env();
        std::env::set_var("API_KEY_SPEND_TOTAL_RECHECK_SECS", "120");
        std::env::set_var("API_KEY_SPEND_REDIS_USER", "Not Valid");
        let bad_user = ApiKeySpendCacheConfig::from_env();
        std::env::remove_var("API_KEY_SPEND_TOTAL_MODE");
        std::env::remove_var("API_KEY_SPEND_TOTAL_RECHECK_SECS");
        std::env::remove_var("API_KEY_SPEND_REDIS_USER");
        std::env::remove_var("API_KEY_SPEND_REDIS_PASSWORD");
        let unset = ApiKeySpendCacheConfig::from_env();
        let good = good.unwrap();
        assert_eq!(good.total_mode, SpendTotalMode::Shadow);
        assert_eq!(good.total_recheck_secs, 120);
        assert_eq!(good.redis_user.as_deref(), Some("spend-test"));
        assert_eq!(good.redis_password.as_deref(), Some("pw"));
        assert_eq!(
            from_file.unwrap().redis_password.as_deref(),
            Some("file-pw")
        );
        assert!(bad_mode.is_err());
        assert!(bad_recheck.is_err());
        assert!(bad_user.is_err());
        let unset = unset.unwrap();
        assert_eq!(unset.total_mode, SpendTotalMode::Off);
        assert_eq!(unset.redis_user, None);
        assert_eq!(unset.redis_password, None);
    }
}
