use rand_core::{OsRng, RngCore};

use super::{models::AttestationReport, AttestationError, AttestationService, GatewayQuoteInput};
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
        let mut model_attestations = vec![];
        let user_provided_nonce = nonce.clone();
        let nonce = nonce.unwrap_or_else(|| {
            tracing::debug!("No nonce provided for attestation report, generated nonce internally");
            generate_nonce_hex()
        });
        let nonce_bytes = decode_nonce_hex(&nonce)?;
        let algo = normalize_signing_algo(signing_algo.as_deref())?;

        if let Some(model) = model {
            let resolved_model = self
                .models_repository
                .resolve_and_get_model(&model)
                .await
                .map_err(|e| {
                    AttestationError::ProviderError(format!("Failed to resolve model: {e}"))
                })?
                .ok_or_else(|| {
                    AttestationError::ProviderError(format!(
                        "Model '{model}' not found. It's not a valid model name or alias."
                    ))
                })?;
            let canonical_name = &resolved_model.model_name;
            if canonical_name != &model {
                tracing::debug!(
                    requested_model = %model,
                    canonical_model = %canonical_name,
                    "Resolved alias to canonical model name for attestation report"
                );
            }

            model_attestations = self
                .inference_provider_pool
                .get_attestation_report(
                    canonical_name.clone(),
                    signing_algo.clone(),
                    // Key fix: only forward the nonce when the caller supplied one.
                    // When None, inference-proxy serves its 5-min cached report
                    // instead of forcing a fresh GPU-evidence collection (~700 ms).
                    user_provided_nonce,
                    signing_address,
                    include_tls_fingerprint,
                    provider_filter,
                )
                .await
                .map_err(|e| AttestationError::ProviderError(e.to_string()))?;
        }

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

        let gateway_attestation = self
            .gateway_quote_collector
            .collect_gateway_quote(GatewayQuoteInput {
                signing_address: signing_address_to_use,
                signing_algo: algo,
                report_data,
                request_nonce: nonce,
                vpc: self.vpc_info.clone(),
                tls_cert_fingerprint: tls_fingerprint.clone(),
            })
            .await?;

        Ok(AttestationReport {
            gateway_attestation,
            model_attestations,
            tls_certificate,
        })
    }
}
