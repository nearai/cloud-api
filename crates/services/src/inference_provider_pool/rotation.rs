//! Rotation-SNI helpers for backend-deterministic attestation discovery.
//!
//! Model-proxy publishes a synthetic SNI scheme `<canonical>-i<N>.<base>` that
//! routes a fresh-TCP connection to backend `N % healthy_count`, bypassing the
//! least-connections LB that otherwise collapses our probes onto a stable
//! subset. Cloud-api combines this with `GET /backends/count?domain=<host>` to
//! learn the live healthy count per cycle and iterate every backend in one
//! pass.
//!
//! See model-proxy PR #27.

use std::time::Duration;

use serde::Deserialize;
use tracing::debug;
use url::Url;

/// Pieces of an inference URL that the rotation path needs.
///
/// `host` is the original SNI/Host the provider was registered with
/// (`glm-5-1.completions.near.ai`), `canonical_label` is just the leftmost
/// DNS label (`glm-5-1`), and `base` is everything after that
/// (`completions.near.ai`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct UrlParts {
    pub host: String,
    pub canonical_label: String,
    pub base: String,
    /// Scheme + optional port preserved so we can rebuild URLs without
    /// dropping non-default ports (e.g. mock servers on `:8443`).
    scheme: String,
    port: Option<u16>,
}

/// Decompose an inference URL into the parts needed for rotation routing.
///
/// Returns `None` when the URL's host doesn't look like a child of a single
/// base domain (one-label hostnames, IP literals, missing host). Callers
/// treat that as "skip rotation for this URL" — there is no other path in
/// this PR, so the discovery cycle records zero observations and the
/// existing fail-closed eviction logic runs.
pub(super) fn split_inference_url(url: &Url) -> Option<UrlParts> {
    let host = url.host_str()?;
    // IP literals (`url::Host::Ipv4` / `Ipv6`) parse into `host_str` too, so
    // gate on the URL host *type*: only Domain is meaningful for SNI rotation.
    if !matches!(url.host()?, url::Host::Domain(_)) {
        return None;
    }
    let host = host.to_ascii_lowercase();
    let (canonical_label, base) = host.split_once('.')?;
    if canonical_label.is_empty() || base.is_empty() || !base.contains('.') {
        // base must itself be a multi-label name (e.g. completions.near.ai),
        // otherwise we'd treat `localhost`-style two-label hosts as valid.
        return None;
    }

    Some(UrlParts {
        host: host.clone(),
        canonical_label: canonical_label.to_string(),
        base: base.to_string(),
        scheme: url.scheme().to_string(),
        port: url.port(),
    })
}

/// Build the rotation URL for index `i`:
/// `https://<canonical>-i<i>.<base>/v1/attestation/report?...`
///
/// The caller appends the query string itself; this helper only builds the
/// authority part. Returns `None` only if URL construction fails (which
/// requires a malformed `UrlParts`, an unreachable state given that
/// `split_inference_url` validated the inputs).
pub(super) fn rotation_base_url(parts: &UrlParts, index: u64) -> Option<Url> {
    let host = format!("{}-i{}.{}", parts.canonical_label, index, parts.base);
    let authority = match parts.port {
        Some(p) => format!("{}://{}:{}", parts.scheme, host, p),
        None => format!("{}://{}", parts.scheme, host),
    };
    Url::parse(&authority).ok()
}

/// Build the count endpoint URL for this provider's base:
/// `https://<base>/backends/count?domain=<host>`
pub(super) fn count_url(parts: &UrlParts) -> Option<Url> {
    let authority = match parts.port {
        Some(p) => format!("{}://{}:{}", parts.scheme, parts.base, p),
        None => format!("{}://{}", parts.scheme, parts.base),
    };
    let mut u = Url::parse(&authority).ok()?;
    u.set_path("/backends/count");
    u.query_pairs_mut().append_pair("domain", &parts.host);
    Some(u)
}

#[derive(Deserialize)]
struct CountResponse {
    healthy: usize,
    #[serde(default)]
    #[allow(dead_code)]
    total: usize,
}

/// Outcome of `/backends/count` fetch.
///
/// `Ok(healthy)` means model-proxy authoritatively reported the live healthy
/// count for this domain. `Err(reason)` means we couldn't get a count and
/// must skip the rotation cycle; the reason is recorded in
/// `DiscoveryOutcome::failure_reasons` for observability and the cycle ends
/// with zero observations.
pub(super) enum CountFetch {
    Ok(usize),
    Err(String),
}

pub(super) async fn fetch_backend_count(
    client: &reqwest::Client,
    parts: &UrlParts,
    timeout: Duration,
) -> CountFetch {
    let Some(url) = count_url(parts) else {
        return CountFetch::Err("count_url_build: failed to build count URL".to_string());
    };
    let send = client.get(url.clone()).timeout(timeout).send();
    let res = match send.await {
        Ok(r) => r,
        Err(e) => {
            debug!(error = %e, url = %url, "backend count fetch failed");
            let category = if e.is_connect() {
                "count_connect"
            } else if e.is_timeout() {
                "count_timeout"
            } else {
                "count_send"
            };
            return CountFetch::Err(format!("{category}: {e}"));
        }
    };
    let status = res.status();
    if !status.is_success() {
        return CountFetch::Err(format!("count_status: {status}"));
    }
    match res.json::<CountResponse>().await {
        Ok(payload) => CountFetch::Ok(payload.healthy),
        Err(e) => CountFetch::Err(format!("count_decode: {e}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parts(url: &str) -> UrlParts {
        split_inference_url(&Url::parse(url).unwrap()).expect("expected parseable URL")
    }

    #[test]
    fn split_basic() {
        let p = parts("https://glm-5-1.completions.near.ai");
        assert_eq!(p.host, "glm-5-1.completions.near.ai");
        assert_eq!(p.canonical_label, "glm-5-1");
        assert_eq!(p.base, "completions.near.ai");
    }

    #[test]
    fn split_canonical_with_internal_dashes() {
        let p = parts("https://qwen35-122b.completions.near.ai");
        assert_eq!(p.canonical_label, "qwen35-122b");
        assert_eq!(p.base, "completions.near.ai");
    }

    #[test]
    fn split_lowercases_host() {
        let p = parts("https://GLM-5-1.COMPLETIONS.NEAR.AI");
        assert_eq!(p.host, "glm-5-1.completions.near.ai");
        assert_eq!(p.canonical_label, "glm-5-1");
        assert_eq!(p.base, "completions.near.ai");
    }

    #[test]
    fn split_preserves_port() {
        let p = parts("https://glm-5-1.completions.near.ai:8443");
        assert_eq!(p.port, Some(8443));
        let r = rotation_base_url(&p, 3).unwrap();
        assert_eq!(r.host_str(), Some("glm-5-1-i3.completions.near.ai"));
        assert_eq!(r.port(), Some(8443));
        let c = count_url(&p).unwrap();
        assert_eq!(c.host_str(), Some("completions.near.ai"));
        assert_eq!(c.port(), Some(8443));
    }

    #[test]
    fn split_rejects_one_label_hostnames() {
        assert!(split_inference_url(&Url::parse("https://localhost").unwrap()).is_none());
    }

    #[test]
    fn split_rejects_two_label_hostnames_with_single_label_base() {
        // `foo.localhost` would map to canonical=foo + base=localhost, but
        // base needs to be a multi-label domain for the rotation scheme to
        // be meaningful.
        assert!(split_inference_url(&Url::parse("https://foo.localhost").unwrap()).is_none());
    }

    #[test]
    fn split_rejects_ip_hosts() {
        assert!(split_inference_url(&Url::parse("https://10.0.0.1").unwrap()).is_none());
        assert!(split_inference_url(&Url::parse("https://[::1]").unwrap()).is_none());
    }

    #[test]
    fn rotation_url_shape() {
        let p = parts("https://glm-5-1.completions.near.ai");
        let r = rotation_base_url(&p, 3).unwrap();
        assert_eq!(r.as_str(), "https://glm-5-1-i3.completions.near.ai/");
    }

    #[test]
    fn rotation_url_index_zero_is_valid() {
        let p = parts("https://glm-5-1.completions.near.ai");
        let r = rotation_base_url(&p, 0).unwrap();
        assert_eq!(r.host_str(), Some("glm-5-1-i0.completions.near.ai"));
    }

    #[test]
    fn count_url_shape() {
        let p = parts("https://glm-5-1.completions.near.ai");
        let c = count_url(&p).unwrap();
        assert_eq!(c.scheme(), "https");
        assert_eq!(c.host_str(), Some("completions.near.ai"));
        assert_eq!(c.path(), "/backends/count");
        assert_eq!(c.query(), Some("domain=glm-5-1.completions.near.ai"));
    }
}
