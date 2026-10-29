//! Configuration for the cloud-platform client and the per-area flags that
//! choose, at composition time, which implementation serves each area.

use std::path::PathBuf;
use std::time::Duration;

/// How one area is served.
///
/// - `Off`: the local repository serves it; cloud-platform is not called.
/// - `Shadow`: both are called, the answers are compared (mismatches are
///   counted) and the local answer is served.
/// - `On`: cloud-platform serves it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum AreaMode {
    #[default]
    Off,
    Shadow,
    On,
}

impl AreaMode {
    pub fn parse(value: &str) -> Result<Self, String> {
        match value.trim().to_ascii_lowercase().as_str() {
            "" | "off" => Ok(Self::Off),
            "shadow" => Ok(Self::Shadow),
            "on" => Ok(Self::On),
            other => Err(format!("expected off|shadow|on, got '{other}'")),
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Off => "off",
            Self::Shadow => "shadow",
            Self::On => "on",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct AreaFlags {
    /// `CP_CATALOG`: `ModelsRepository` reads.
    pub catalog: AreaMode,
    /// `CP_SIGNATURE_READ`: chat signature reads.
    pub signature_read: AreaMode,
    /// `CP_CHECK_API_KEY`: `POST /v1/check_api_key`.
    pub check_api_key: AreaMode,
}

impl AreaFlags {
    pub fn any_enabled(&self) -> bool {
        [self.catalog, self.signature_read, self.check_api_key]
            .iter()
            .any(|mode| *mode != AreaMode::Off)
    }
}

#[derive(Debug, Clone)]
pub struct CpClientConfig {
    /// `CLOUD_PLATFORM_URL`, e.g. `http://10.0.2.2:24410`. Required when any
    /// area is not `off`.
    pub base_url: Option<String>,
    /// `CLOUD_PLATFORM_API_KEY_FILE`: cloud-api's own API key, used for calls
    /// made with no end caller. The file must not be readable by group or
    /// others.
    pub api_key_file: Option<PathBuf>,
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
    /// `CP_CATALOG_CACHE_TTL_SECS` (default 30): catalog cache and
    /// model-access refresh period.
    pub catalog_ttl: Duration,
    /// `CP_CHECK_API_KEY_TTL_SECS` (default 300): cached key verdict
    /// lifetime, before ±20% jitter.
    pub check_api_key_ttl: Duration,
    pub flags: AreaFlags,
}

impl Default for CpClientConfig {
    fn default() -> Self {
        Self {
            base_url: None,
            api_key_file: None,
            read_timeout: Duration::from_millis(300),
            write_timeout: Duration::from_millis(1000),
            connect_timeout: Duration::from_millis(500),
            breaker_failures: 5,
            breaker_open: Duration::from_secs(5),
            catalog_ttl: Duration::from_secs(30),
            check_api_key_ttl: Duration::from_secs(300),
            flags: AreaFlags::default(),
        }
    }
}

impl CpClientConfig {
    pub fn from_env() -> Result<Self, String> {
        Self::from_lookup(|name| std::env::var(name).ok())
    }

    pub fn from_lookup(get: impl Fn(&str) -> Option<String>) -> Result<Self, String> {
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
        let mode = |name: &str| -> Result<AreaMode, String> {
            get(name)
                .map(|value| AreaMode::parse(&value).map_err(|e| format!("{name}: {e}")))
                .unwrap_or(Ok(AreaMode::Off))
        };
        let breaker_failures = match positive("CLOUD_PLATFORM_BREAKER_FAILURES")? {
            None => defaults.breaker_failures,
            Some(n) => u32::try_from(n)
                .map_err(|_| "CLOUD_PLATFORM_BREAKER_FAILURES is too large".to_string())?,
        };
        let config = Self {
            base_url: get("CLOUD_PLATFORM_URL")
                .map(|url| url.trim().to_string())
                .filter(|url| !url.is_empty()),
            api_key_file: get("CLOUD_PLATFORM_API_KEY_FILE")
                .map(|path| path.trim().to_string())
                .filter(|path| !path.is_empty())
                .map(PathBuf::from),
            read_timeout: millis("CLOUD_PLATFORM_READ_TIMEOUT_MS", defaults.read_timeout)?,
            write_timeout: millis("CLOUD_PLATFORM_WRITE_TIMEOUT_MS", defaults.write_timeout)?,
            connect_timeout: millis(
                "CLOUD_PLATFORM_CONNECT_TIMEOUT_MS",
                defaults.connect_timeout,
            )?,
            breaker_failures,
            breaker_open: millis("CLOUD_PLATFORM_BREAKER_OPEN_MS", defaults.breaker_open)?,
            catalog_ttl: secs("CP_CATALOG_CACHE_TTL_SECS", defaults.catalog_ttl)?,
            check_api_key_ttl: secs("CP_CHECK_API_KEY_TTL_SECS", defaults.check_api_key_ttl)?,
            flags: AreaFlags {
                catalog: mode("CP_CATALOG")?,
                signature_read: mode("CP_SIGNATURE_READ")?,
                check_api_key: mode("CP_CHECK_API_KEY")?,
            },
        };
        if config.flags.any_enabled() {
            if config.base_url.is_none() {
                return Err(
                    "CLOUD_PLATFORM_URL is required when a CP_* area is not off".to_string()
                );
            }
            if config.api_key_file.is_none() {
                return Err(
                    "CLOUD_PLATFORM_API_KEY_FILE is required when a CP_* area is not off"
                        .to_string(),
                );
            }
        }
        Ok(config)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn config(vars: &[(&str, &str)]) -> Result<CpClientConfig, String> {
        let vars: HashMap<String, String> = vars
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        CpClientConfig::from_lookup(|name| vars.get(name).cloned())
    }

    #[test]
    fn defaults_are_off_and_need_no_url() {
        let config = config(&[]).unwrap();
        assert_eq!(config.flags, AreaFlags::default());
        assert_eq!(config.read_timeout, Duration::from_millis(300));
        assert_eq!(config.write_timeout, Duration::from_secs(1));
        assert_eq!(config.connect_timeout, Duration::from_millis(500));
        assert_eq!(config.catalog_ttl, Duration::from_secs(30));
        assert_eq!(config.check_api_key_ttl, Duration::from_secs(300));
    }

    #[test]
    fn an_enabled_area_requires_a_url_and_a_key_file_named_in_the_error() {
        let error = config(&[("CP_CATALOG", "shadow")]).unwrap_err();
        assert!(error.contains("CLOUD_PLATFORM_URL"), "{error}");
        let error = config(&[
            ("CP_CATALOG", "shadow"),
            ("CLOUD_PLATFORM_URL", "http://10.0.2.2:24410"),
        ])
        .unwrap_err();
        assert!(error.contains("CLOUD_PLATFORM_API_KEY_FILE"), "{error}");
        let config = config(&[
            ("CP_CATALOG", "shadow"),
            ("CP_CHECK_API_KEY", "ON"),
            ("CLOUD_PLATFORM_URL", "http://10.0.2.2:24410"),
            ("CLOUD_PLATFORM_API_KEY_FILE", "/run/secrets/cp-key"),
            ("CLOUD_PLATFORM_READ_TIMEOUT_MS", "900"),
        ])
        .unwrap();
        assert_eq!(config.flags.catalog, AreaMode::Shadow);
        assert_eq!(config.flags.check_api_key, AreaMode::On);
        assert_eq!(config.flags.signature_read, AreaMode::Off);
        assert_eq!(config.read_timeout, Duration::from_millis(900));
    }

    #[test]
    fn invalid_values_are_refused() {
        assert!(config(&[("CP_CATALOG", "maybe")]).is_err());
        assert!(config(&[("CLOUD_PLATFORM_READ_TIMEOUT_MS", "0")]).is_err());
        assert!(config(&[("CLOUD_PLATFORM_BREAKER_FAILURES", "x")]).is_err());
    }
}
