//! Tinfoil provider configuration. Endpoints are code constants: there is no
//! environment override in production builds.

use std::time::Duration;

/// Tinfoil's inference router. Requests actually go to `https://{domain}` of
/// the verified ATC bundle (a validated `*.tinfoil.sh` host); this is the
/// canonical host the ATC normally names.
pub const BASE_URL: &str = "https://inference.tinfoil.sh";
/// Attestation transparency endpoint that serves the router's attestation bundle.
pub const ATC_URL: &str = "https://atc.tinfoil.sh/attestation";
/// How often the router's model document is re-read.
pub const PROXY_REREAD: Duration = Duration::from_secs(60);
/// How often the router attestation is fully re-verified.
pub const ROUTER_REVERIFY: Duration = Duration::from_secs(300);

/// Where requests actually go, as seen by the transport builder. The production
/// value is `Redirect::default()` (no change: connect to the attested domain
/// over normal DNS). Only the test constructors below ever set it, so tests and
/// release run the same `build_transport` code path. Not configurable from the
/// environment or any public API.
#[derive(Clone, Default)]
pub(super) struct Redirect {
    /// Resolve whatever domain the bundle names to this address (and port).
    pub(super) resolve: Option<std::net::SocketAddr>,
    /// Send requests to this base URL regardless of the attested domain.
    pub(super) base: Option<String>,
}

/// Fallback when a non-positive timeout is supplied.
const DEFAULT_TIMEOUT_SECONDS: u64 = 300;

#[derive(Clone)]
pub struct Config {
    /// Tinfoil API key. A secret: private, with a redacting `Debug`.
    api_key: String,
    pub atc_url: String,
    /// Always default outside the crate-private test constructors.
    pub(super) redirect: Redirect,
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
            atc_url: ATC_URL.to_string(),
            redirect: Redirect::default(),
            timeout: Duration::from_secs(secs),
        }
    }

    /// Point at a local test server. Test builds only.
    #[cfg(test)]
    pub fn with_urls(mut self, base: &str, atc: &str) -> Self {
        self.redirect.base = Some(base.trim_end_matches('/').to_string());
        self.atc_url = atc.to_string();
        self
    }

    /// Test only: map the attested domain to a local server.
    #[cfg(test)]
    pub fn with_route(mut self, addr: std::net::SocketAddr, atc: &str) -> Self {
        self.redirect.resolve = Some(addr);
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
            .field("atc_url", &self.atc_url)
            .field("timeout", &self.timeout)
            .finish()
    }
}
