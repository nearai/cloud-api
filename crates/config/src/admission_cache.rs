use std::fmt;

use crate::types::{parse_bool_env, parse_u64_env};

/// Configuration for the replaceable Redis admission snapshot cache.
#[derive(Clone, PartialEq, Eq)]
pub struct AdmissionCacheConfig {
    pub redis_url: Option<String>,
    pub ttl_seconds: u64,
    pub ttl_jitter_seconds: u64,
    pub command_deadline_ms: u64,
    pub fallback_concurrency: usize,
    pub fallback_deadline_ms: u64,
    pub max_fill_age_ms: u64,
}

impl Default for AdmissionCacheConfig {
    fn default() -> Self {
        Self {
            redis_url: None,
            ttl_seconds: 300,
            ttl_jitter_seconds: 30,
            command_deadline_ms: 100,
            fallback_concurrency: 64,
            fallback_deadline_ms: 500,
            max_fill_age_ms: 500,
        }
    }
}

impl fmt::Debug for AdmissionCacheConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("AdmissionCacheConfig")
            .field("redis_url", &self.redis_url.as_ref().map(|_| "<redacted>"))
            .field("ttl_seconds", &self.ttl_seconds)
            .field("ttl_jitter_seconds", &self.ttl_jitter_seconds)
            .field("command_deadline_ms", &self.command_deadline_ms)
            .field("fallback_concurrency", &self.fallback_concurrency)
            .field("fallback_deadline_ms", &self.fallback_deadline_ms)
            .field("max_fill_age_ms", &self.max_fill_age_ms)
            .finish()
    }
}

impl AdmissionCacheConfig {
    pub fn from_env() -> Result<Self, String> {
        let mut config = Self {
            redis_url: std::env::var("ADMISSION_CACHE_REDIS_URL")
                .ok()
                .map(|value| value.trim().to_string())
                .filter(|value| !value.is_empty()),
            ..Self::default()
        };
        let enabled = parse_bool_env("ADMISSION_CACHE_ENABLED", false)?;
        if !enabled {
            config.redis_url = None;
        } else if config.redis_url.is_none() {
            return Err(
                "ADMISSION_CACHE_REDIS_URL must be set when admission cache is enabled".into(),
            );
        }
        config.ttl_seconds = parse_u64_env("ADMISSION_CACHE_TTL_SECONDS", config.ttl_seconds)?;
        config.ttl_jitter_seconds = parse_u64_env(
            "ADMISSION_CACHE_TTL_JITTER_SECONDS",
            config.ttl_jitter_seconds,
        )?;
        config.command_deadline_ms = parse_u64_env(
            "ADMISSION_CACHE_COMMAND_DEADLINE_MS",
            config.command_deadline_ms,
        )?;
        config.fallback_concurrency = parse_u64_env(
            "ADMISSION_CACHE_FALLBACK_CONCURRENCY",
            config.fallback_concurrency as u64,
        )?
        .try_into()
        .map_err(|_| "admission fallback concurrency is too large".to_string())?;
        config.fallback_deadline_ms = parse_u64_env(
            "ADMISSION_CACHE_FALLBACK_DEADLINE_MS",
            config.fallback_deadline_ms,
        )?;
        config.max_fill_age_ms =
            parse_u64_env("ADMISSION_CACHE_MAX_FILL_AGE_MS", config.max_fill_age_ms)?;
        config.validate()?;
        Ok(config)
    }

    pub fn validate(&self) -> Result<(), String> {
        if !(1..=86_400).contains(&self.ttl_seconds)
            || self.ttl_jitter_seconds >= self.ttl_seconds
            || !(1..=60_000).contains(&self.command_deadline_ms)
            || !(1..=60_000).contains(&self.fallback_deadline_ms)
            || !(1..=60_000).contains(&self.max_fill_age_ms)
            || !(1..=10_000).contains(&self.fallback_concurrency)
            || self.max_fill_age_ms >= (self.ttl_seconds - self.ttl_jitter_seconds) * 1000
        {
            return Err("invalid admission cache budgets: TTL must be 1..86400 seconds, jitter below TTL, deadlines 1..60000 ms, fill age below minimum TTL, and concurrency 1..10000".into());
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_is_disabled_and_debug_redacts_endpoint() {
        let mut config = AdmissionCacheConfig::default();
        assert!(config.redis_url.is_none());
        assert!(config.validate().is_ok());
        config.redis_url = Some("rediss://user:secret@example.invalid:6379".into());
        let debug = format!("{config:?}");
        assert!(!debug.contains("secret"));
        assert!(!debug.contains("example.invalid"));
    }

    #[test]
    fn rejects_budgets_that_cannot_preserve_expiration() {
        for invalid in [
            AdmissionCacheConfig {
                ttl_seconds: u64::MAX,
                ..Default::default()
            },
            AdmissionCacheConfig {
                ttl_jitter_seconds: 300,
                ..Default::default()
            },
            AdmissionCacheConfig {
                fallback_concurrency: 0,
                ..Default::default()
            },
            AdmissionCacheConfig {
                max_fill_age_ms: 60_001,
                ..Default::default()
            },
        ] {
            assert!(invalid.validate().is_err());
        }
    }
}
