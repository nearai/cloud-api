use std::sync::Arc;

use rand_core::{OsRng, RngCore};

use super::{models::AttestationReport, AttestationError, AttestationService, GatewayQuoteInput};
use crate::metrics::consts::{
    get_environment, METRIC_ATTESTATION_REPORT_CACHE, TAG_ENVIRONMENT, TAG_RESULT,
};
use crate::models::{resolve_model_cached, ModelCaller};
use inference_providers::ProviderFilter;

pub(in crate::attestation) fn generate_nonce_hex() -> String {
    let mut nonce_bytes = [0u8; 32];
    OsRng.fill_bytes(&mut nonce_bytes);
    nonce_bytes
        .into_iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>()
}

pub(in crate::attestation) fn decode_nonce_hex(nonce: &str) -> Result<Vec<u8>, AttestationError> {
    let nonce_bytes = hex::decode(nonce).map_err(|e| {
        // Malformed caller-supplied nonce → clean 400 below; a
        // client input error, not a server fault.
        tracing::warn!("Failed to decode nonce hex string: {}", e);
        AttestationError::InvalidParameter(format!("Invalid nonce format: {e}"))
    })?;

    if nonce_bytes.len() != 32 {
        return Err(AttestationError::InvalidParameter(format!(
            "Nonce must be exactly 32 bytes, got {} bytes",
            nonce_bytes.len()
        )));
    }

    Ok(nonce_bytes)
}

/// Key of the no-nonce attestation-report cache.
///
/// SAFETY-CRITICAL: the nonce is deliberately NOT a field — this cache is only
/// ever consulted for nonce-less requests (the caller checks `nonce.is_none()`
/// before using the key). Every other input that changes the report contents
/// IS a field, so a request can never receive a report built for a different
/// model / algo / tls-fingerprint / provider tier / signing address / policy.
/// Each input is its own typed field: no caller-supplied value (the signing
/// address is not validated) can spell another caller's key.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(in crate::attestation) struct ReportCacheKey {
    model: Option<String>,
    /// The output of [`normalize_signing_algo`]. An omitted algorithm keys
    /// separately from an explicit `ed25519`: the gateway quote defaults to
    /// ed25519, but the model backend applies its own default (ecdsa on
    /// inference-proxy), so the two requests get different reports.
    signing_algo: Option<String>,
    include_tls_fingerprint: bool,
    provider_filter: Option<ProviderFilter>,
    signing_address: Option<String>,
    /// The live `attested_3p` kill-switch policy: a cache hit skips the build
    /// (and the pool's disabled-source filter), so a report cached before a
    /// source was disabled must not be reachable once the policy changes.
    disabled_sources: Vec<inference_providers::ProviderSource>,
    /// The caller may not use the model's long-context tier, so its report
    /// leaves that tier out.
    long_context_restricted: bool,
}

/// The cache key for the request parameters, with no source disabled and no
/// long-context restriction.
fn report_cache_key(
    model: Option<&str>,
    signing_algo: Option<&str>,
    include_tls_fingerprint: bool,
    provider_filter: Option<ProviderFilter>,
    signing_address: Option<&str>,
) -> ReportCacheKey {
    ReportCacheKey {
        model: model.map(str::to_string),
        signing_algo: signing_algo.map(str::to_string),
        include_tls_fingerprint,
        provider_filter,
        signing_address: signing_address.map(str::to_string),
        disabled_sources: Vec::new(),
        long_context_restricted: false,
    }
}

/// Gateway quote algorithm when the caller does not choose one.
const DEFAULT_GATEWAY_SIGNING_ALGO: &str = "ed25519";

/// Validate and lowercase the caller's `signing_algo`. This is the only place
/// the value is normalized: the cache key, the gateway quote and the request
/// forwarded to the model backend all use the result, because model backends
/// accept only the lowercase names. `None` stays `None`, so the model backend
/// keeps applying its own default.
fn normalize_signing_algo(signing_algo: Option<&str>) -> Result<Option<String>, AttestationError> {
    let Some(algo) = signing_algo else {
        return Ok(None);
    };
    let algo = algo.to_lowercase();

    if algo != "ecdsa" && algo != "ed25519" {
        return Err(AttestationError::InvalidParameter(format!(
            "Invalid signing algorithm: {algo}, must be 'ecdsa' or 'ed25519'"
        )));
    }

    Ok(Some(algo))
}

impl AttestationService {
    #[allow(clippy::too_many_arguments)]
    pub(in crate::attestation) async fn get_attestation_report_impl(
        &self,
        model: Option<String>,
        signing_algo: Option<String>,
        nonce: Option<String>,
        signing_address: Option<String>,
        include_tls_fingerprint: bool,
        provider_filter: Option<ProviderFilter>,
        caller: ModelCaller,
    ) -> Result<AttestationReport, AttestationError> {
        let env_tag = format!("{TAG_ENVIRONMENT}:{}", get_environment());
        let signing_algo = normalize_signing_algo(signing_algo.as_deref())?;

        // Resolve the model for the caller before the cache lookup below. A
        // cache hit skips the build, so a check made inside the build would
        // not run when another caller had already cached the report. A model
        // the caller may not use fails here exactly like an unknown one.
        let resolved_canonical = if let Some(ref m) = model {
            let resolved_model = resolve_model_cached(
                &self.model_resolve_cache,
                self.models_repository.as_ref(),
                m,
                caller,
            )
            .await
            .map_err(|e| AttestationError::ProviderError(format!("Failed to resolve model: {e}")))?
            .ok_or_else(|| AttestationError::UnknownModel(m.clone()))?;
            let canonical = resolved_model.model_name;
            if &canonical != m {
                tracing::debug!(
                    requested_model = %m,
                    canonical_model = %canonical,
                    "Resolved alias to canonical model name for attestation report"
                );
            }
            Some(canonical)
        } else {
            None
        };

        // An organization that may not use the model's long-context tier gets
        // a report without it. Decided before the cache lookup, like the
        // model resolution above; only such callers get a separate key, so
        // every other caller keeps sharing cache entries.
        let long_context_allowed = match (&resolved_canonical, caller, &self.long_context_access) {
            (Some(canonical), ModelCaller::Organization(organization_id), Some(access)) => {
                self.inference_provider_pool
                    .long_context_base_capacity(canonical)
                    .is_none()
                    || access.is_allowed(organization_id, canonical).await
            }
            _ => true,
        };

        // Precompute the no-nonce cache key BEFORE the params are moved into the
        // build closure below. The live kill-switch policy applies on this
        // instance immediately, for filtered and unfiltered requests.
        let cache_key = ReportCacheKey {
            disabled_sources: self
                .inference_provider_pool
                .attested_3p_disabled()
                .load()
                .iter()
                .collect(),
            long_context_restricted: !long_context_allowed,
            ..report_cache_key(
                model.as_deref(),
                signing_algo.as_deref(),
                include_tls_fingerprint,
                provider_filter,
                signing_address.as_deref(),
            )
        };

        // The full (expensive) report build. `nonce` is the closure parameter so
        // the body below is unchanged. Called exactly once per request: the
        // bypass arm diverges via `return`, the cache arm via `try_get_with`.
        let build = |nonce: Option<String>| async move {
            // Track whether the caller supplied a nonce. Only caller-supplied nonces are
            // forwarded to inference-proxy: when None, inference-proxy can serve its
            // 5-minute cached report (skipping a fresh GPU-evidence NVIDIA NRAS round-trip
            // that serializes behind a per-backend Mutex and costs ~700 ms per call).
            let user_provided_nonce = nonce.clone();
            let nonce = nonce.unwrap_or_else(|| {
                tracing::debug!(
                    "No nonce provided for attestation report, generated nonce internally"
                );
                generate_nonce_hex()
            });
            let nonce_bytes = decode_nonce_hex(&nonce)?;
            let algo = signing_algo
                .as_deref()
                .unwrap_or(DEFAULT_GATEWAY_SIGNING_ALGO)
                .to_string();

            let signing_address_to_use = self.get_signing_address_hex(&algo)?;
            let signing_address_clean = signing_address_to_use
                .strip_prefix("0x")
                .unwrap_or(&signing_address_to_use);
            let signing_address_bytes = hex::decode(signing_address_clean).map_err(|e| {
                tracing::error!("Failed to decode signing address hex string: {}", e);
                AttestationError::InvalidParameter(format!("Invalid signing address format: {e}"))
            })?;
            let signing_address_for_report = if signing_address_bytes.len() > 32 {
                signing_address_bytes[..32].to_vec()
            } else {
                signing_address_bytes
            };

            let tls_fingerprint = if include_tls_fingerprint {
                Some(self.tls_cert_fingerprint.clone().ok_or_else(|| {
                AttestationError::InternalError(
                    "include_tls_fingerprint=true but TLS_CERT_PATH is not set or fingerprint could not be computed".to_string(),
                )
            })?)
            } else {
                None
            };
            let tls_certificate = if include_tls_fingerprint {
                if let Ok(path) = std::env::var("TLS_CERT_PATH") {
                    tokio::fs::read_to_string(&path).await.ok()
                } else {
                    None
                }
            } else {
                None
            };

            let mut report_data = vec![0u8; 64];
            if let Some(ref fp_hex) = tls_fingerprint {
                use sha2::Digest;
                let fp_bytes = hex::decode(fp_hex).map_err(|e| {
                    AttestationError::InternalError(format!("bad cert fingerprint hex: {e}"))
                })?;
                let mut hasher = sha2::Sha256::new();
                hasher.update(&signing_address_for_report);
                hasher.update(&fp_bytes);
                report_data[..32].copy_from_slice(&hasher.finalize());
            } else {
                report_data[..signing_address_for_report.len()]
                    .copy_from_slice(&signing_address_for_report);
            }
            report_data[32..64].copy_from_slice(&nonce_bytes);

            // Run model-attestation fetch and gateway TDX-quote generation concurrently.
            // These are independent: the model fetch is an outbound HTTP call to the
            // inference backend; the gateway quote is a local dstack Unix-socket call.
            let model_fut = {
                let pool = &self.inference_provider_pool;
                async move {
                    if let Some(canonical) = resolved_canonical {
                        pool.get_attestation_report(
                            canonical,
                            // Normalized above; `None` keeps the backend's default.
                            signing_algo,
                            // Key fix: only forward the nonce when the caller supplied one.
                            // When None, inference-proxy serves its 5-min cached report
                            // instead of forcing a fresh GPU-evidence collection (~700 ms).
                            user_provided_nonce,
                            signing_address,
                            include_tls_fingerprint,
                            provider_filter,
                            long_context_allowed,
                        )
                        .await
                        .map_err(|e| AttestationError::ProviderError(e.to_string()))
                    } else {
                        Ok(vec![])
                    }
                }
            };

            let gateway_fut =
                self.gateway_quote_collector
                    .collect_gateway_quote(GatewayQuoteInput {
                        signing_address: signing_address_to_use,
                        signing_algo: algo,
                        report_data,
                        request_nonce: nonce,
                        vpc: self.vpc_info.clone(),
                        tls_cert_fingerprint: tls_fingerprint.clone(),
                    });

            let (model_attestations, gateway_attestation) =
                tokio::try_join!(model_fut, gateway_fut)?;

            Ok(AttestationReport {
                gateway_attestation,
                model_attestations,
                tls_certificate,
            })
        }; // end `build` closure

        // Nonce-bearing requests (and the disabled-cache case) bypass the cache:
        // the nonce is cryptographically bound into the TDX report_data and the
        // GPU evidence, so serving a cached report for a different nonce would
        // defeat the freshness / replay guarantee. Only nonce-LESS requests —
        // which already opt out of freshness (the gateway auto-generates a random
        // nonce and inference-proxy serves its own cached report) — are cached.
        let cache = match &self.report_cache {
            Some(c) if nonce.is_none() => c.clone(),
            _ => {
                self.metrics_service.record_count(
                    METRIC_ATTESTATION_REPORT_CACHE,
                    1,
                    &[&format!("{TAG_RESULT}:bypass"), &env_tag],
                );
                return build(nonce).await;
            }
        };

        // No-nonce path: short-TTL cache + single-flight. `try_get_with` coalesces
        // concurrent misses on the same key, so a burst of identical no-nonce
        // probes triggers ONE backend build instead of N (collapsing the
        // monitoring thundering-herd that sheds 503s at the inference backend).
        let computed = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let flag = computed.clone();
        let res = cache
            .try_get_with(cache_key, async move {
                flag.store(true, std::sync::atomic::Ordering::Relaxed);
                build(None).await.map(Arc::new)
            })
            .await;
        let result = if computed.load(std::sync::atomic::Ordering::Relaxed) {
            "miss"
        } else {
            "hit"
        };
        self.metrics_service.record_count(
            METRIC_ATTESTATION_REPORT_CACHE,
            1,
            &[&format!("{TAG_RESULT}:{result}"), &env_tag],
        );
        // Clone the report out of the shared `Arc`; map the coalesced
        // `Arc<AttestationError>` back to an owned error so the route still maps
        // it to the correct HTTP status.
        res.map(|arc| (*arc).clone()).map_err(|e| (*e).clone())
    }
}

#[cfg(test)]
mod cache_key_tests {
    use super::{normalize_signing_algo, report_cache_key, ReportCacheKey};
    use crate::attestation::AttestationError;
    use inference_providers::{ProviderFilter, ProviderSource};

    #[test]
    fn report_cache_key_distinguishes_3p_sources() {
        let c = report_cache_key(
            Some("m"),
            None,
            false,
            Some(ProviderFilter::Source(ProviderSource::Chutes)),
            None,
        );
        let t = report_cache_key(
            Some("m"),
            None,
            false,
            Some(ProviderFilter::Source(ProviderSource::Tinfoil)),
            None,
        );
        let n = report_cache_key(Some("m"), None, false, Some(ProviderFilter::Near), None);
        assert_ne!(c, t);
        assert_ne!(c, n);
        assert_ne!(t, n);
    }

    #[test]
    fn key_is_independent_of_nonce_by_construction() {
        // The function has no nonce parameter — this test documents that the
        // safety-critical invariant (a cached no-nonce report is never keyed on,
        // nor served for, a specific nonce) holds at the type level: two requests
        // with identical params map to one key regardless of any nonce the caller
        // did or didn't send.
        let a = report_cache_key(Some("gpt-oss-120b"), Some("ecdsa"), false, None, None);
        let b = report_cache_key(Some("gpt-oss-120b"), Some("ecdsa"), false, None, None);
        assert_eq!(a, b);
    }

    #[test]
    fn normalize_lowercases_and_keeps_omitted_algo_unset() {
        for (input, expected) in [
            ("ECDSA", "ecdsa"),
            ("ecdsa", "ecdsa"),
            ("Ed25519", "ed25519"),
            ("ED25519", "ed25519"),
        ] {
            assert_eq!(
                normalize_signing_algo(Some(input)).unwrap().as_deref(),
                Some(expected)
            );
        }
        assert_eq!(normalize_signing_algo(None).unwrap(), None);
        assert!(matches!(
            normalize_signing_algo(Some("rsa")),
            Err(AttestationError::InvalidParameter(_))
        ));
    }

    #[test]
    fn algo_casings_share_a_key_but_omitted_algo_does_not() {
        let key = |algo: Option<&str>| {
            let algo = normalize_signing_algo(algo).unwrap();
            report_cache_key(Some("m"), algo.as_deref(), false, None, None)
        };
        assert_eq!(key(Some("ECDSA")), key(Some("ecdsa")));
        assert_eq!(key(Some("Ed25519")), key(Some("ed25519")));
        // The gateway defaults to ed25519 but the model backend defaults to
        // ecdsa, so an omitted algo must not be served an explicit-ed25519
        // report (or vice versa).
        assert_ne!(key(None), key(Some("ed25519")));
    }

    #[test]
    fn distinct_params_produce_distinct_keys() {
        let base = report_cache_key(Some("m"), Some("ecdsa"), false, None, None);
        // Each differing dimension must yield a different key (no cross-serving).
        assert_ne!(
            base,
            report_cache_key(Some("m2"), Some("ecdsa"), false, None, None)
        );
        assert_ne!(
            base,
            report_cache_key(Some("m"), Some("ed25519"), false, None, None)
        );
        assert_ne!(
            base,
            report_cache_key(Some("m"), Some("ecdsa"), true, None, None)
        );
        assert_ne!(
            base,
            report_cache_key(
                Some("m"),
                Some("ecdsa"),
                false,
                Some(ProviderFilter::Near),
                None
            )
        );
        assert_ne!(
            base,
            report_cache_key(Some("m"), Some("ecdsa"), false, None, Some("0xabc"))
        );
        // Near vs Attested3p must not collide.
        assert_ne!(
            report_cache_key(
                Some("m"),
                Some("ecdsa"),
                false,
                Some(ProviderFilter::Near),
                None
            ),
            report_cache_key(
                Some("m"),
                Some("ecdsa"),
                false,
                Some(ProviderFilter::Source(ProviderSource::Chutes)),
                None
            ),
        );
    }

    #[test]
    fn none_model_is_distinct_from_wildcard_literal() {
        // Typed fields: a request for all models (None) no longer shares a key
        // with a model literally named "*".
        assert_ne!(
            report_cache_key(None, Some("ecdsa"), false, None, None),
            report_cache_key(Some("*"), Some("ecdsa"), false, None, None),
        );
    }

    #[test]
    fn a_crafted_signing_address_cannot_spell_a_policy_field() {
        let restricted = ReportCacheKey {
            long_context_restricted: true,
            ..report_cache_key(Some("m"), None, false, None, Some("0xabc"))
        };
        // The marker the previous string key appended after the address.
        let crafted = report_cache_key(Some("m"), None, false, None, Some("0xabc|lc=base"));
        assert_ne!(restricted, crafted);

        let disabled = ReportCacheKey {
            disabled_sources: vec![ProviderSource::Chutes],
            ..report_cache_key(Some("m"), None, false, None, Some("0xabc"))
        };
        let crafted = report_cache_key(Some("m"), None, false, None, Some("0xabc|off=chutes"));
        assert_ne!(disabled, crafted);
        assert_ne!(
            restricted,
            report_cache_key(Some("m"), None, false, None, Some("0xabc"))
        );
    }
}
