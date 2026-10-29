//! Configuration for the cloud-platform client.
//!
//! The connection settings are the only switch: with both `CLOUD_PLATFORM_URL`
//! and `CLOUD_PLATFORM_API_KEY_FILE` unset, cloud-api runs without any
//! cloud-platform code path. The timeout, breaker and cache-TTL settings only
//! tune a configured client, so they are not read when it is unconfigured.

use std::path::PathBuf;
use std::time::Duration;

use super::catalog::OUTER_RESOLVE_CACHE_TTL;

#[derive(Debug, Clone)]
pub struct CpClientConfig {
    /// `CLOUD_PLATFORM_URL`, e.g. `http://10.0.2.2:24410`.
    pub base_url: String,
    /// `CLOUD_PLATFORM_API_KEY_FILE`: cloud-api's own API key, used for calls
    /// made with no end caller. The file must be owner-only (0400 or 0600).
    pub api_key_file: PathBuf,
    /// `CLOUD_PLATFORM_READ_TIMEOUT_MS` (default 300).
    pub read_timeout: Duration,
    /// `CLOUD_PLATFORM_WRITE_TIMEOUT_MS` (default 1000).
    pub write_timeout: Duration,
    /// `CLOUD_PLATFORM_CONNECT_TIMEOUT_MS` (default 500).
    pub connect_timeout: Duration,
    /// `CLOUD_PLATFORM_BREAKER_FAILURES` (default 5): consecutive failures
    /// that open a call type's breaker.
    pub breaker_failures: u32,
    /// `CLOUD_PLATFORM_BREAKER_OPEN_MS` (default 5000): how long a breaker
    /// stays open before one probe is let through.
    pub breaker_open: Duration,
    /// `CP_CATALOG_CACHE_TTL_SECS` (default 30): how long a catalog answer is
    /// fresh.
    pub catalog_ttl: Duration,
    /// `CP_CATALOG_MAX_STALE_SECS` (default 300): how long after its fetch a
    /// model lookup may still be served while cloud-platform is down. Past it,
    /// the lookup fails, so a stale `is_listed` cannot skip the grant check.
    pub catalog_max_stale: Duration,
    /// `CP_CHECK_API_KEY_TTL_SECS` (default 300): the longest a cached key
    /// verdict is served. Each verdict lives between 80% and 100% of it.
    pub check_api_key_ttl: Duration,
}

impl Default for CpClientConfig {
    fn default() -> Self {
        Self {
            base_url: String::new(),
            api_key_file: PathBuf::new(),
            read_timeout: Duration::from_millis(300),
            write_timeout: Duration::from_millis(1000),
            connect_timeout: Duration::from_millis(500),
            breaker_failures: 5,
            breaker_open: Duration::from_secs(5),
            catalog_ttl: Duration::from_secs(30),
            catalog_max_stale: Duration::from_secs(300),
            check_api_key_ttl: Duration::from_secs(300),
        }
    }
}

impl CpClientConfig {
    /// `Ok(None)` when neither connection setting is set. Setting exactly one is
    /// an error naming the missing one; the error never includes a value.
    pub fn from_env() -> Result<Option<Self>, String> {
        Self::from_lookup(|name| std::env::var(name).ok())
    }

    pub fn from_lookup(get: impl Fn(&str) -> Option<String>) -> Result<Option<Self>, String> {
        let base_url = get("CLOUD_PLATFORM_URL")
            .map(|url| url.trim().to_string())
            .filter(|url| !url.is_empty());
        let api_key_file = get("CLOUD_PLATFORM_API_KEY_FILE")
            .map(|path| path.trim().to_string())
            .filter(|path| !path.is_empty())
            .map(PathBuf::from);
        let (base_url, api_key_file) = match (base_url, api_key_file) {
            (None, None) => return Ok(None),
            (Some(_), None) => {
                return Err(
                    "CLOUD_PLATFORM_API_KEY_FILE is required when CLOUD_PLATFORM_URL is set"
                        .to_string(),
                )
            }
            (None, Some(_)) => {
                return Err(
                    "CLOUD_PLATFORM_URL is required when CLOUD_PLATFORM_API_KEY_FILE is set"
                        .to_string(),
                )
            }
            (Some(base_url), Some(api_key_file)) => (base_url, api_key_file),
        };

        // Tuning is read only for a configured client.
        let defaults = Self::default();
        let positive = |name: &str| -> Result<Option<u64>, String> {
            get(name)
                .map(|value| {
                    value
                        .trim()
                        .parse::<u64>()
                        .ok()
                        .filter(|n| *n > 0)
                        .ok_or_else(|| format!("{name} must be a positive integer"))
                })
                .transpose()
        };
        let millis = |name: &str, default: Duration| -> Result<Duration, String> {
            Ok(positive(name)?.map_or(default, Duration::from_millis))
        };
        let secs = |name: &str, default: Duration| -> Result<Duration, String> {
            Ok(positive(name)?.map_or(default, Duration::from_secs))
        };
        let breaker_failures = match positive("CLOUD_PLATFORM_BREAKER_FAILURES")? {
            None => defaults.breaker_failures,
            Some(n) => u32::try_from(n)
                .map_err(|_| "CLOUD_PLATFORM_BREAKER_FAILURES is too large".to_string())?,
        };
        Ok(Some(Self {
            base_url,
            api_key_file,
            read_timeout: millis("CLOUD_PLATFORM_READ_TIMEOUT_MS", defaults.read_timeout)?,
            write_timeout: millis("CLOUD_PLATFORM_WRITE_TIMEOUT_MS", defaults.write_timeout)?,
            connect_timeout: millis(
                "CLOUD_PLATFORM_CONNECT_TIMEOUT_MS",
                defaults.connect_timeout,
            )?,
            breaker_failures,
            breaker_open: millis("CLOUD_PLATFORM_BREAKER_OPEN_MS", defaults.breaker_open)?,
            catalog_ttl: secs("CP_CATALOG_CACHE_TTL_SECS", defaults.catalog_ttl)?,
            catalog_max_stale: secs("CP_CATALOG_MAX_STALE_SECS", defaults.catalog_max_stale)?,
            check_api_key_ttl: secs("CP_CHECK_API_KEY_TTL_SECS", defaults.check_api_key_ttl)?,
        }))
        .and_then(|config| match config {
            // The models service holds a resolved lookup for 30 s more, so the bound
            // must leave room for the TTL plus that hold (see
            // `catalog::OUTER_RESOLVE_CACHE_TTL`).
            Some(config)
                if config.catalog_max_stale
                    < config.catalog_ttl + OUTER_RESOLVE_CACHE_TTL =>
            {
                Err(format!(
                    "CP_CATALOG_MAX_STALE_SECS ({}s) must be at least CP_CATALOG_CACHE_TTL_SECS ({}s) + {}s",
                    config.catalog_max_stale.as_secs(),
                    config.catalog_ttl.as_secs(),
                    OUTER_RESOLVE_CACHE_TTL.as_secs()
                ))
            }
            other => Ok(other),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn config(vars: &[(&str, &str)]) -> Result<Option<CpClientConfig>, String> {
        let vars: HashMap<String, String> = vars
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        CpClientConfig::from_lookup(|name| vars.get(name).cloned())
    }

    const URL: (&str, &str) = ("CLOUD_PLATFORM_URL", "http://10.0.2.2:24410");
    const KEY: (&str, &str) = ("CLOUD_PLATFORM_API_KEY_FILE", "/run/secrets/cp-key");

    #[test]
    fn unconfigured_when_neither_setting_is_set_and_tuning_is_not_read() {
        // Invalid tuning values do not matter without the connection settings.
        let unset = config(&[
            ("CLOUD_PLATFORM_READ_TIMEOUT_MS", "0"),
            ("CLOUD_PLATFORM_BREAKER_FAILURES", "x"),
            ("CP_CATALOG_CACHE_TTL_SECS", "nope"),
        ]);
        assert!(unset.unwrap().is_none());
    }

    #[test]
    fn both_settings_configure_the_client_with_tuning() {
        let config = config(&[URL, KEY, ("CLOUD_PLATFORM_READ_TIMEOUT_MS", "900")])
            .unwrap()
            .unwrap();
        assert_eq!(config.base_url, "http://10.0.2.2:24410");
        assert_eq!(config.read_timeout, Duration::from_millis(900));
        assert_eq!(config.catalog_ttl, Duration::from_secs(30));
        assert_eq!(config.check_api_key_ttl, Duration::from_secs(300));
    }

    #[test]
    fn exactly_one_setting_fails_naming_the_missing_one() {
        let error = config(&[URL]).unwrap_err();
        assert!(error.contains("CLOUD_PLATFORM_API_KEY_FILE"), "{error}");
        assert!(!error.contains("10.0.2.2"), "{error}");
        let error = config(&[KEY]).unwrap_err();
        assert!(error.contains("CLOUD_PLATFORM_URL"), "{error}");
        assert!(!error.contains("/run/secrets"), "{error}");
    }

    #[test]
    fn invalid_tuning_is_refused_once_configured() {
        assert!(config(&[URL, KEY, ("CLOUD_PLATFORM_READ_TIMEOUT_MS", "0")]).is_err());
        assert!(config(&[URL, KEY, ("CLOUD_PLATFORM_BREAKER_FAILURES", "x")]).is_err());
    }

    #[test]
    fn a_max_stale_bound_without_room_for_the_outer_hold_is_refused_naming_both() {
        let error = config(&[
            URL,
            KEY,
            ("CP_CATALOG_CACHE_TTL_SECS", "60"),
            ("CP_CATALOG_MAX_STALE_SECS", "60"),
        ])
        .unwrap_err();
        assert!(error.contains("CP_CATALOG_CACHE_TTL_SECS"), "{error}");
        assert!(error.contains("CP_CATALOG_MAX_STALE_SECS"), "{error}");
        // TTL + 30 s is the least room that is allowed.
        assert!(config(&[
            URL,
            KEY,
            ("CP_CATALOG_CACHE_TTL_SECS", "60"),
            ("CP_CATALOG_MAX_STALE_SECS", "90"),
        ])
        .is_ok());
        // The defaults (30 s TTL, 300 s bound) pass.
        assert!(config(&[URL, KEY]).unwrap().is_some());
    }
}
