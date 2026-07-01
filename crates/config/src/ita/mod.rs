use serde::{Deserialize, Serialize};
use std::{env, fmt};

use crate::types::{
    parse_bool_env, parse_u32_env, parse_u64_env, read_optional_secret_env_absent_empty,
};

mod policy;

pub use policy::*;

pub const DEFAULT_ITA_API_BASE_URL: &str = "https://api.trustauthority.intel.com";
pub const DEFAULT_ITA_PORTAL_BASE_URL: &str = "https://portal.trustauthority.intel.com";
pub const DEFAULT_ITA_TIMEOUT_SECONDS: u64 = 10;
pub const DEFAULT_ITA_MAX_RETRIES: u32 = 2;
pub const DEFAULT_ITA_RETRY_BACKOFF_MS: u64 = 250;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ItaBaseUrl(String);

impl ItaBaseUrl {
    pub fn parse(raw: &str, field: &str) -> Result<Self, String> {
        let trimmed = raw.trim().trim_end_matches('/');
        let Some(after_scheme) = trimmed
            .strip_prefix("https://")
            .or_else(|| trimmed.strip_prefix("http://"))
        else {
            return Err(format!("{field} must be an http(s) URL"));
        };

        let host = after_scheme.split('/').next().unwrap_or_default();
        if host.is_empty()
            || host.contains(char::is_whitespace)
            || trimmed.contains('?')
            || trimmed.contains('#')
        {
            return Err(format!("{field} must be an http(s) URL with a host"));
        }

        Ok(Self(trimmed.to_string()))
    }

    pub fn as_str(&self) -> &str {
        self.0.as_str()
    }
}

#[derive(Clone, PartialEq, Eq)]
pub struct ItaAttestationConfig {
    pub enabled: bool,
    pub api_base_url: ItaBaseUrl,
    pub portal_base_url: ItaBaseUrl,
    pub api_key: Option<String>,
    pub timeout_seconds: u64,
    pub max_retries: u32,
    pub retry_backoff_ms: u64,
    pub policy_ids: ItaPolicyIds,
    pub policy_must_match: bool,
    pub token_signing_alg: ItaTokenSigningAlg,
}

impl fmt::Debug for ItaAttestationConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ItaAttestationConfig")
            .field("enabled", &self.enabled)
            .field("api_base_url", &self.api_base_url)
            .field("portal_base_url", &self.portal_base_url)
            .field("api_key_configured", &self.api_key.is_some())
            .field("timeout_seconds", &self.timeout_seconds)
            .field("max_retries", &self.max_retries)
            .field("retry_backoff_ms", &self.retry_backoff_ms)
            .field("policy_ids", &self.policy_ids)
            .field("policy_must_match", &self.policy_must_match)
            .field("token_signing_alg", &self.token_signing_alg)
            .finish()
    }
}

impl Default for ItaAttestationConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            api_base_url: ItaBaseUrl(DEFAULT_ITA_API_BASE_URL.to_string()),
            portal_base_url: ItaBaseUrl(DEFAULT_ITA_PORTAL_BASE_URL.to_string()),
            api_key: None,
            timeout_seconds: DEFAULT_ITA_TIMEOUT_SECONDS,
            max_retries: DEFAULT_ITA_MAX_RETRIES,
            retry_backoff_ms: DEFAULT_ITA_RETRY_BACKOFF_MS,
            policy_ids: ItaPolicyIds::default(),
            policy_must_match: false,
            token_signing_alg: ItaTokenSigningAlg::default(),
        }
    }
}

impl ItaAttestationConfig {
    pub fn from_env() -> Result<Self, String> {
        let enabled = parse_bool_env("ENABLE_ITA_ATTESTATION", false)?;
        let api_base_url = ItaBaseUrl::parse(
            env::var("ITA_API_BASE_URL")
                .as_deref()
                .unwrap_or(DEFAULT_ITA_API_BASE_URL),
            "ITA_API_BASE_URL",
        )?;
        let portal_base_url = ItaBaseUrl::parse(
            env::var("ITA_PORTAL_BASE_URL")
                .as_deref()
                .unwrap_or(DEFAULT_ITA_PORTAL_BASE_URL),
            "ITA_PORTAL_BASE_URL",
        )?;
        let api_key = if enabled {
            read_optional_secret_env_absent_empty("ITA_API_KEY_FILE", "ITA_API_KEY")?
        } else {
            None
        };
        let timeout_seconds = parse_u64_env("ITA_TIMEOUT_SECONDS", DEFAULT_ITA_TIMEOUT_SECONDS)?;
        let max_retries = parse_u32_env("ITA_MAX_RETRIES", DEFAULT_ITA_MAX_RETRIES)?;
        let retry_backoff_ms = parse_u64_env("ITA_RETRY_BACKOFF_MS", DEFAULT_ITA_RETRY_BACKOFF_MS)?;
        let policy_ids = ItaPolicyIds::parse_csv(
            env::var("ITA_POLICY_IDS").as_deref().unwrap_or_default(),
            "ITA_POLICY_IDS",
        )?;
        let policy_must_match = parse_bool_env("ITA_POLICY_MUST_MATCH", false)?;
        let token_signing_alg = env::var("ITA_TOKEN_SIGNING_ALG")
            .as_deref()
            .unwrap_or("PS384")
            .parse()?;

        if enabled && api_key.is_none() {
            return Err(
                "ITA_API_KEY or ITA_API_KEY_FILE must be set when ENABLE_ITA_ATTESTATION=true"
                    .to_string(),
            );
        }

        Ok(Self {
            enabled,
            api_base_url,
            portal_base_url,
            api_key,
            timeout_seconds,
            max_retries,
            retry_backoff_ms,
            policy_ids,
            policy_must_match,
            token_signing_alg,
        })
    }

    pub fn effective_policy(&self, policy_override: &ItaPolicyOverride) -> ItaEffectivePolicy {
        ItaEffectivePolicy {
            policy_ids: policy_override
                .policy_ids
                .clone()
                .unwrap_or_else(|| self.policy_ids.clone()),
            policy_must_match: policy_override
                .policy_must_match
                .unwrap_or(self.policy_must_match),
            token_signing_alg: policy_override
                .token_signing_alg
                .unwrap_or(self.token_signing_alg),
        }
    }
}

#[cfg(test)]
mod tests;
