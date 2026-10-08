//! Tinfoil provider configuration. Endpoints are code constants: there is no
//! environment override in production builds.

use std::time::Duration;

/// Tinfoil's inference router.
pub const BASE_URL: &str = "https://inference.tinfoil.sh";
/// Attestation transparency endpoint that serves the router's attestation bundle.
pub const ATC_URL: &str = "https://atc.tinfoil.sh/attestation";
/// How often the router's model document is re-read.
pub const PROXY_REREAD: Duration = Duration::from_secs(60);
/// How often the router attestation is fully re-verified.
pub const ROUTER_REVERIFY: Duration = Duration::from_secs(300);

/// Fallback when a non-positive timeout is supplied.
const DEFAULT_TIMEOUT_SECONDS: u64 = 300;

#[derive(Clone)]
pub struct Config {
    /// Tinfoil API key. A secret: private, with a redacting `Debug`.
    api_key: String,
    pub base_url: String,
    pub atc_url: String,
    pub timeout: Duration,
}

impl Config {
    pub fn new(api_key: String, timeout_secs: i64) -> Self {
        let secs = u64::try_from(timeout_secs)
            .ok()
            .filter(|s| *s > 0)
            .unwrap_or(DEFAULT_TIMEOUT_SECONDS);
        Self {
            api_key,
            base_url: BASE_URL.to_string(),
            atc_url: ATC_URL.to_string(),
            timeout: Duration::from_secs(secs),
        }
    }

    /// Point at a local test server. Test builds only.
    #[cfg(test)]
    pub fn with_urls(mut self, base: &str, atc: &str) -> Self {
        self.base_url = base.trim_end_matches('/').to_string();
        self.atc_url = atc.to_string();
        self
    }

    pub(super) fn api_key(&self) -> &str {
        &self.api_key
    }
}

impl std::fmt::Debug for Config {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Config")
            .field("api_key", &"<redacted>")
            .field("base_url", &self.base_url)
            .field("atc_url", &self.atc_url)
            .field("timeout", &self.timeout)
            .finish()
    }
}
