use std::sync::Arc;

use rand_core::{OsRng, RngCore};

use super::{models::AttestationReport, AttestationError, AttestationService, GatewayQuoteInput};
use crate::metrics::consts::{
    get_environment, METRIC_ATTESTATION_REPORT_CACHE, TAG_ENVIRONMENT, TAG_RESULT,
};
use inference_providers::ProviderTier;

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

/// Build the cache key for the no-nonce attestation-report cache.
///
/// SAFETY-CRITICAL: the nonce is deliberately NOT an input — this cache is only
/// ever consulted for nonce-less requests (the caller checks `nonce.is_none()`
/// before using the returned key). Every other parameter that changes the report
/// contents IS part of the key, so a request can never receive a report built
/// for a different model / algo / tls-fingerprint / provider tier / signing
/// address. `signing_algo` is lowercased and defaulted to match the service's
/// own algo normalization.
fn report_cache_key(
    model: Option<&str>,
    signing_algo: Option<&str>,
    include_tls_fingerprint: bool,
    provider_filter: Option<ProviderTier>,
    signing_address: Option<&str>,
) -> String {
    format!(
        "m={}|a={}|tls={}|pf={}|sa={}",
        model.unwrap_or("*"),
        signing_algo.unwrap_or("ed25519").to_ascii_lowercase(),
        include_tls_fingerprint,
        provider_filter.map(|t| t.as_str()).unwrap_or("-"),
        signing_address.unwrap_or("-"),
    )
}

fn normalize_signing_algo(signing_algo: Option<&str>) -> Result<String, AttestationError> {
    let algo = signing_algo
        .map(str::to_lowercase)
        .unwrap_or_else(|| "ed25519".to_string());

    if algo != "ecdsa" && algo != "ed25519" {
        return Err(AttestationError::InvalidParameter(format!(
            "Invalid signing algorithm: {algo}, must be 'ecdsa' or 'ed25519'"
        )));
    }

    Ok(algo)
}

impl AttestationService {
    pub(in crate::attestation) async fn get_attestation_report_impl(
        &self,
        model: Option<String>,
        signing_algo: Option<String>,
        nonce: Option<String>,
        signing_address: Option<String>,
        include_tls_fingerprint: bool,
        provider_filter: Option<ProviderTier>,
    ) -> Result<AttestationReport, AttestationError> {
        let env_tag = format!("{TAG_ENVIRONMENT}:{}", get_environment());

        // Precompute the no-nonce cache key BEFORE the params are moved into the
        // build closure below.
        let cache_key = report_cache_key(
            model.as_deref(),
            signing_algo.as_deref(),
            include_tls_fingerprint,
            provider_filter,
            signing_address.as_deref(),
        );

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
            let algo = normalize_signing_algo(signing_algo.as_deref())?;

            // Resolve model alias synchronously (fast DB lookup ~10 ms) before
            // spawning the parallel futures below.
            let resolved_canonical = if let Some(ref m) = model {
                let resolved_model = self
                    .models_repository
                    .resolve_and_get_model(m)
                    .await
                    .map_err(|e| {
                        AttestationError::ProviderError(format!("Failed to resolve model: {e}"))
                    })?
                    .ok_or_else(|| {
                        AttestationError::ProviderError(format!(
                            "Model '{m}' not found. It's not a valid model name or alias."
                        ))
                    })?;
                let canonical = resolved_model.model_name.clone();
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
                            signing_algo,
                            // Key fix: only forward the nonce when the caller supplied one.
                            // When None, inference-proxy serves its 5-min cached report
                            // instead of forcing a fresh GPU-evidence collection (~700 ms).
                            user_provided_nonce,
                            signing_address,
                            include_tls_fingerprint,
                            provider_filter,
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
    use super::report_cache_key;
    use inference_providers::ProviderTier;

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
    fn algo_defaults_and_lowercases() {
        // None defaults to ed25519; case is normalized so ECDSA and ecdsa collide.
        assert_eq!(
            report_cache_key(Some("m"), None, false, None, None),
            report_cache_key(Some("m"), Some("ed25519"), false, None, None),
        );
        assert_eq!(
            report_cache_key(Some("m"), Some("ECDSA"), false, None, None),
            report_cache_key(Some("m"), Some("ecdsa"), false, None, None),
        );
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
                Some(ProviderTier::Near),
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
                Some(ProviderTier::Near),
                None
            ),
            report_cache_key(
                Some("m"),
                Some("ecdsa"),
                false,
                Some(ProviderTier::Attested3p),
                None
            ),
        );
    }

    #[test]
    fn none_model_is_distinct_from_wildcard_literal() {
        // A request for all models (None) keys on "*"; a model literally named
        // "*" is not a realistic catalog entry, so this is acceptable and just
        // documents the sentinel.
        assert_eq!(
            report_cache_key(None, Some("ecdsa"), false, None, None),
            report_cache_key(Some("*"), Some("ecdsa"), false, None, None),
        );
    }
}
