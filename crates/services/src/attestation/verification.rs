//! Attestation verification for inference provider backends.
//!
//! Verifies TDX quotes, report_data bindings (signing address + TLS fingerprint),
//! image hashes, and GPU evidence from attestation reports returned by inference-proxy.

use sha2::{Digest as Sha2Digest, Sha256};
use std::collections::HashSet;

const NVIDIA_NRAS_URL: &str = "https://nras.attestation.nvidia.com/v3/attest/gpu";

/// Number of parallel attestation calls per model to discover TLS fingerprints
/// from multiple backends behind L4 load balancing.
///
/// Each cloud-api instance runs its own discovery, so the effective load on a
/// model is `PARALLELISM * cloud-api instance count` per refresh cycle. Keep
/// this modest to avoid piling attestation work on inference backends.
pub const ATTESTATION_DISCOVERY_PARALLELISM: usize = 5;

/// Number of cumulative attestation calls per reused provider on each refresh.
///
/// Each cycle adds a small number of fresh-TCP discovery calls to a reused
/// provider, which accumulates new backend fingerprints into the shared
/// `FingerprintState`. Over several cycles this covers every backend behind
/// the L4 LB, even when the initial discovery only hit one. Kept small so
/// steady-state refresh load stays low.
pub const CUMULATIVE_DISCOVERY_CALLS: usize = 2;

/// Result of verifying an attestation report from an inference backend.
#[derive(Debug, Clone)]
pub struct VerifiedAttestation {
    /// The verified SPKI fingerprint of the backend's TLS certificate.
    pub tls_cert_fingerprint: Option<String>,
    /// The verified signing address from the attestation.
    pub signing_address: String,
    /// TDX TCB status (e.g., "UpToDate", "OutOfDate").
    pub tcb_status: String,
    /// Intel advisory IDs.
    pub advisory_ids: Vec<String>,
    /// OS image hash extracted from the RTMR3-verified event log.
    pub os_image_hash: Option<String>,
    /// Compose hash extracted from the RTMR3-verified event log.
    pub compose_hash: Option<String>,
    /// GPU verification verdict (e.g., "PASS"), if GPU evidence was present.
    pub gpu_verdict: Option<String>,
}

/// Data extracted from the RTMR3-verified event log.
struct EventLogData {
    os_image_hash: Option<String>,
    compose_hash: Option<String>,
}

/// dstack runtime event type constant (0x08000001).
const DSTACK_RUNTIME_EVENT_TYPE: u32 = 0x08000001;

/// An entry from the TDX event log.
#[derive(Debug, Clone, serde::Deserialize)]
struct EventLogEntry {
    /// SHA-384 digest of the event (hex-encoded).
    digest: String,
    /// Event type (0x08000001 for dstack runtime events).
    #[serde(default)]
    event_type: u32,
    /// Event name (e.g., "os-image-hash", "compose-hash", "app-id").
    #[serde(default)]
    event: String,
    /// Event payload (hex-encoded raw bytes).
    #[serde(default)]
    event_payload: String,
    /// Which RTMR this event extends (0-3).
    imr: u32,
}

/// Configuration for attestation verification.
#[derive(Clone)]
pub struct AttestationVerifier {
    /// HTTP client for NVIDIA NRAS calls.
    http_client: reqwest::Client,
    /// Set of allowed OS image hashes (from env var). Empty = skip image hash check.
    allowed_image_hashes: HashSet<String>,
    /// Optional PCCS URL override for Intel collateral fetching.
    pccs_url: Option<String>,
    /// If true, reject attestations where TCB status is not "UpToDate".
    require_tcb_up_to_date: bool,
}

impl AttestationVerifier {
    pub fn new(
        allowed_image_hashes: HashSet<String>,
        pccs_url: Option<String>,
        require_tcb_up_to_date: bool,
    ) -> Self {
        let http_client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(30))
            .build()
            .expect("failed to create HTTP client for attestation verification");
        Self {
            http_client,
            allowed_image_hashes,
            pccs_url,
            require_tcb_up_to_date,
        }
    }

    /// Build an `AttestationVerifier` from environment variables.
    ///
    /// - `ALLOWED_IMAGE_HASHES`: comma-separated list of allowed OS image hashes (hex).
    ///   If unset or empty, image hash verification is skipped.
    /// - `PCCS_URL`: optional Intel PCCS URL for TDX collateral fetching.
    pub fn from_env() -> Self {
        let allowed_image_hashes: HashSet<String> = std::env::var("ALLOWED_IMAGE_HASHES")
            .unwrap_or_default()
            .split(',')
            .map(|s| s.trim().to_lowercase())
            .filter(|s| !s.is_empty())
            .collect();

        if !allowed_image_hashes.is_empty() {
            tracing::info!(
                count = allowed_image_hashes.len(),
                "Loaded allowed image hashes for attestation verification"
            );
        }

        let pccs_url = std::env::var("PCCS_URL").ok().filter(|s| !s.is_empty());

        let require_tcb_up_to_date = std::env::var("REQUIRE_TCB_UP_TO_DATE")
            .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
            .unwrap_or(false);

        Self::new(allowed_image_hashes, pccs_url, require_tcb_up_to_date)
    }

    /// Verify an attestation report from an inference backend.
    ///
    /// Performs:
    /// 1. TDX quote verification via dcap-qvl (Intel signature chain)
    /// 2. Report data binding verification (signing address + TLS fingerprint + nonce)
    /// 3. OS image hash check against allowlist (if configured)
    /// 4. GPU evidence verification via NVIDIA NRAS (if present)
    ///
    /// The `attestation_report` is the JSON map returned by the backend's
    /// `/v1/attestation/report` endpoint.
    pub async fn verify_attestation_report(
        &self,
        attestation_report: &serde_json::Map<String, serde_json::Value>,
        request_nonce: &str,
    ) -> Result<VerifiedAttestation, AttestationVerificationError> {
        // 1. Extract and verify TDX quote
        let intel_quote_hex = attestation_report
            .get("intel_quote")
            .and_then(|v| v.as_str())
            .ok_or_else(|| AttestationVerificationError::MissingField("intel_quote".to_string()))?;

        let quote_hex = intel_quote_hex
            .strip_prefix("0x")
            .unwrap_or(intel_quote_hex);
        let quote_bytes = hex::decode(quote_hex).map_err(|e| {
            AttestationVerificationError::InvalidFormat(format!("intel_quote hex decode: {e}"))
        })?;

        let pccs_url = self
            .pccs_url
            .clone()
            .unwrap_or_else(|| dcap_qvl::collateral::PHALA_PCCS_URL.to_string());
        let collateral_client = dcap_qvl::collateral::CollateralClient::with_default_http(pccs_url)
            .map_err(|e| {
                AttestationVerificationError::TdxVerificationFailed(format!(
                    "failed to build collateral client: {e:#}"
                ))
            })?;
        let verified_report = collateral_client
            .fetch_and_verify(&quote_bytes)
            .await
            .map_err(|e| AttestationVerificationError::TdxVerificationFailed(format!("{e:#}")))?;

        // Check TCB status
        let tcb_status = &verified_report.status;
        if self.require_tcb_up_to_date && tcb_status != "UpToDate" {
            return Err(AttestationVerificationError::TdxVerificationFailed(format!(
                "TCB status is '{tcb_status}' but REQUIRE_TCB_UP_TO_DATE is set (advisory_ids: {:?})",
                verified_report.advisory_ids
            )));
        }
        if tcb_status != "UpToDate" {
            tracing::warn!(
                tcb_status = %tcb_status,
                advisory_ids = ?verified_report.advisory_ids,
                "TDX TCB status is not UpToDate — microcode may need updating"
            );
        }

        // Check debug mode is disabled
        let td_report = verified_report.report.as_td10().ok_or_else(|| {
            AttestationVerificationError::TdxVerificationFailed(
                "no TD10 report in verified quote".to_string(),
            )
        })?;

        let is_debug = td_report.td_attributes[0] & 0x01 != 0;
        if is_debug {
            return Err(AttestationVerificationError::TdxVerificationFailed(
                "TDX debug mode is enabled — rejecting".to_string(),
            ));
        }

        let tcb_status = verified_report.status.clone();
        let advisory_ids = verified_report.advisory_ids.clone();

        // 2. Verify report_data binding
        let report_data = &td_report.report_data;
        let signing_address = attestation_report
            .get("signing_address")
            .and_then(|v| v.as_str())
            .ok_or_else(|| {
                AttestationVerificationError::MissingField("signing_address".to_string())
            })?;

        let tls_cert_fingerprint = attestation_report
            .get("tls_cert_fingerprint")
            .and_then(|v| v.as_str());

        self.verify_report_data(
            report_data,
            signing_address,
            tls_cert_fingerprint,
            request_nonce,
            attestation_report
                .get("signing_algo")
                .and_then(|v| v.as_str())
                .unwrap_or("ed25519"),
        )?;

        // 3. Replay RTMR3 from event log and verify against the TDX quote.
        //    If successful, extract os_image_hash and compose_hash from the verified log.
        let event_log_data =
            self.verify_rtmr3_and_extract(attestation_report, &td_report.rt_mr3)?;

        // 4. Check OS image hash from verified event log against allowlist
        if let Some(ref hash) = event_log_data.os_image_hash {
            if !self.allowed_image_hashes.is_empty()
                && !self.allowed_image_hashes.contains(&hash.to_lowercase())
            {
                return Err(AttestationVerificationError::ImageHashMismatch(format!(
                    "os_image_hash '{}' from RTMR3-verified event log not in allowed list",
                    hash
                )));
            }
        } else if !self.allowed_image_hashes.is_empty() {
            return Err(AttestationVerificationError::ImageHashMismatch(
                "ALLOWED_IMAGE_HASHES configured but no os-image-hash in event log".to_string(),
            ));
        }

        // 5. Verify GPU evidence (best-effort — skip if not present)
        let gpu_verdict = self
            .verify_gpu_evidence(attestation_report, request_nonce)
            .await?;

        Ok(VerifiedAttestation {
            tls_cert_fingerprint: tls_cert_fingerprint.map(|s| s.to_string()),
            signing_address: signing_address.to_string(),
            tcb_status,
            advisory_ids,
            os_image_hash: event_log_data.os_image_hash,
            compose_hash: event_log_data.compose_hash,
            gpu_verdict,
        })
    }

    /// Verify that `report_data` correctly binds the signing address, TLS fingerprint, and nonce.
    ///
    /// When `tls_cert_fingerprint` is present:
    ///   `report_data[0:32] = SHA256(signing_address_bytes || fingerprint_bytes)`
    /// Otherwise:
    ///   `report_data[0:32] = signing_address_bytes padded to 32`
    ///
    /// Always: `report_data[32:64] = nonce_bytes`
    fn verify_report_data(
        &self,
        report_data: &[u8; 64],
        signing_address: &str,
        tls_cert_fingerprint: Option<&str>,
        nonce: &str,
        _signing_algo: &str,
    ) -> Result<(), AttestationVerificationError> {
        // Verify nonce (second 32 bytes)
        let nonce_bytes = hex::decode(nonce.strip_prefix("0x").unwrap_or(nonce)).map_err(|e| {
            AttestationVerificationError::InvalidFormat(format!("nonce hex decode: {e}"))
        })?;
        if nonce_bytes.len() != 32 {
            return Err(AttestationVerificationError::ReportDataMismatch(format!(
                "nonce must be 32 bytes, got {}",
                nonce_bytes.len()
            )));
        }
        if report_data[32..64] != nonce_bytes[..] {
            return Err(AttestationVerificationError::ReportDataMismatch(
                "nonce mismatch in report_data[32:64]".to_string(),
            ));
        }

        // Verify first 32 bytes
        let addr_hex = signing_address
            .strip_prefix("0x")
            .unwrap_or(signing_address);
        let addr_bytes = hex::decode(addr_hex).map_err(|e| {
            AttestationVerificationError::InvalidFormat(format!("signing_address hex decode: {e}"))
        })?;

        if let Some(fp_hex) = tls_cert_fingerprint {
            // TLS fingerprint binding: SHA256(signing_address_bytes || fingerprint_bytes)
            let fp_bytes =
                hex::decode(fp_hex.strip_prefix("0x").unwrap_or(fp_hex)).map_err(|e| {
                    AttestationVerificationError::InvalidFormat(format!(
                        "tls_cert_fingerprint hex decode: {e}"
                    ))
                })?;
            let mut hasher = Sha256::new();
            hasher.update(&addr_bytes);
            hasher.update(&fp_bytes);
            let expected = hasher.finalize();

            if report_data[..32] != expected[..] {
                return Err(AttestationVerificationError::ReportDataMismatch(format!(
                    "report_data[0:32] does not match SHA256(signing_address || tls_fingerprint). \
                     Expected: {}, got: {}",
                    hex::encode(expected),
                    hex::encode(&report_data[..32])
                )));
            }
        } else {
            // No TLS fingerprint: first 32 bytes = signing_address padded to 32
            let mut expected = [0u8; 32];
            let copy_len = addr_bytes.len().min(32);
            expected[..copy_len].copy_from_slice(&addr_bytes[..copy_len]);
            if report_data[..32] != expected[..] {
                return Err(AttestationVerificationError::ReportDataMismatch(format!(
                    "report_data[0:32] does not match padded signing_address. \
                     Expected: {}, got: {}",
                    hex::encode(expected),
                    hex::encode(&report_data[..32])
                )));
            }
        }

        Ok(())
    }

    /// Replay RTMR3 from the event log and verify it matches the TDX quote.
    ///
    /// RTMR3 contains runtime measurements: app_id, compose_hash, os-image-hash,
    /// instance_id, key-provider, etc. By replaying the SHA-384 hash chain from
    /// the event log and comparing against the quote's `rt_mr3`, we cryptographically
    /// verify the event log is authentic. Then we extract os_image_hash and compose_hash
    /// from the verified events.
    fn verify_rtmr3_and_extract(
        &self,
        attestation_report: &serde_json::Map<String, serde_json::Value>,
        quoted_rtmr3: &[u8; 48],
    ) -> Result<EventLogData, AttestationVerificationError> {
        // Parse event log from attestation response
        let event_log = attestation_report
            .get("event_log")
            .ok_or_else(|| AttestationVerificationError::MissingField("event_log".to_string()))?;

        // Event log may be a JSON array directly or a JSON string containing the array
        let events: Vec<EventLogEntry> = if let Some(s) = event_log.as_str() {
            serde_json::from_str(s).map_err(|e| {
                AttestationVerificationError::InvalidFormat(format!(
                    "failed to parse event_log string: {e}"
                ))
            })?
        } else {
            serde_json::from_value(event_log.clone()).map_err(|e| {
                AttestationVerificationError::InvalidFormat(format!(
                    "failed to parse event_log value: {e}"
                ))
            })?
        };

        // Replay RTMR3: SHA-384 chain of ALL events with imr == 3.
        // For runtime events (event_type == 0x08000001), we first validate that the
        // digest matches SHA384(event_type_le || ":" || event_name || ":" || payload_bytes).
        // This prevents an attacker from keeping valid digests while swapping payloads.
        use sha2::Sha384;
        let mut rtmr3 = [0u8; 48];
        let mut rtmr3_event_count = 0u32;
        for event in &events {
            if event.imr != 3 {
                continue;
            }
            rtmr3_event_count += 1;

            // For runtime events, validate digest matches payload
            let digest_bytes = if event.event_type == DSTACK_RUNTIME_EVENT_TYPE {
                let payload_bytes = hex::decode(&event.event_payload).map_err(|e| {
                    AttestationVerificationError::InvalidFormat(format!(
                        "runtime event '{}' payload hex decode: {e}",
                        event.event
                    ))
                })?;
                let mut hasher = Sha384::new();
                sha2::Digest::update(&mut hasher, DSTACK_RUNTIME_EVENT_TYPE.to_ne_bytes());
                sha2::Digest::update(&mut hasher, b":");
                sha2::Digest::update(&mut hasher, event.event.as_bytes());
                sha2::Digest::update(&mut hasher, b":");
                sha2::Digest::update(&mut hasher, &payload_bytes);
                let computed: [u8; 48] = sha2::Digest::finalize(hasher).into();

                // If the event has a stored digest, verify it matches
                if !event.digest.is_empty() {
                    let stored = hex::decode(&event.digest).map_err(|e| {
                        AttestationVerificationError::InvalidFormat(format!(
                            "event '{}' digest hex decode: {e}",
                            event.event
                        ))
                    })?;
                    if computed[..] != stored[..] {
                        return Err(AttestationVerificationError::RtmrMismatch(format!(
                            "runtime event '{}' digest does not match payload: computed={}, stored={}",
                            event.event,
                            hex::encode(computed),
                            hex::encode(&stored)
                        )));
                    }
                }
                computed.to_vec()
            } else {
                // Non-runtime events: use stored digest directly
                hex::decode(&event.digest).map_err(|e| {
                    AttestationVerificationError::InvalidFormat(format!(
                        "event digest hex decode: {e}"
                    ))
                })?
            };

            // RTMR extension: RTMR = SHA-384(RTMR || digest)
            let mut hasher = Sha384::new();
            sha2::Digest::update(&mut hasher, rtmr3);
            sha2::Digest::update(&mut hasher, &digest_bytes);
            let result = sha2::Digest::finalize(hasher);
            rtmr3.copy_from_slice(&result);
        }

        // Reject if no RTMR3 events found
        let has_rtmr3_events = rtmr3_event_count > 0;
        if !has_rtmr3_events {
            return Err(AttestationVerificationError::RtmrMismatch(
                "no RTMR3 events in event log — cannot verify runtime measurements".to_string(),
            ));
        }

        if rtmr3 != *quoted_rtmr3 {
            return Err(AttestationVerificationError::RtmrMismatch(format!(
                "RTMR3 replay mismatch: replayed={}, quoted={}",
                hex::encode(rtmr3),
                hex::encode(quoted_rtmr3)
            )));
        }

        // Event log is verified — extract os-image-hash and compose-hash
        let mut os_image_hash = None;
        let mut compose_hash = None;
        for event in &events {
            if event.imr != 3 {
                continue;
            }
            match event.event.as_str() {
                "os-image-hash" => {
                    os_image_hash = Some(event.event_payload.clone());
                }
                "compose-hash" => {
                    compose_hash = Some(event.event_payload.clone());
                }
                _ => {}
            }
        }

        Ok(EventLogData {
            os_image_hash,
            compose_hash,
        })
    }

    /// Verify GPU evidence via NVIDIA NRAS.
    ///
    /// Returns `Some(verdict)` if GPU evidence was present and verified,
    /// `None` if no GPU evidence was included (e.g., gateway without GPU).
    async fn verify_gpu_evidence(
        &self,
        attestation_report: &serde_json::Map<String, serde_json::Value>,
        request_nonce: &str,
    ) -> Result<Option<String>, AttestationVerificationError> {
        let nvidia_payload_str = match attestation_report
            .get("nvidia_payload")
            .and_then(|v| v.as_str())
        {
            Some(s) if !s.is_empty() => s,
            _ => return Ok(None), // No GPU evidence — acceptable for non-GPU CVMs
        };

        let payload: serde_json::Value = serde_json::from_str(nvidia_payload_str).map_err(|e| {
            AttestationVerificationError::GpuVerificationFailed(format!(
                "failed to parse nvidia_payload JSON: {e}"
            ))
        })?;

        // Verify nonce matches
        let payload_nonce = payload
            .get("nonce")
            .and_then(|v| v.as_str())
            .unwrap_or_default();
        if payload_nonce.to_lowercase() != request_nonce.to_lowercase() {
            return Err(AttestationVerificationError::GpuVerificationFailed(
                format!(
                    "GPU payload nonce mismatch: expected {}, got {}",
                    request_nonce, payload_nonce
                ),
            ));
        }

        // Submit to NVIDIA NRAS
        let response = self
            .http_client
            .post(NVIDIA_NRAS_URL)
            .json(&payload)
            .send()
            .await
            .map_err(|e| {
                AttestationVerificationError::GpuVerificationFailed(format!(
                    "NVIDIA NRAS request failed: {e}"
                ))
            })?;

        if !response.status().is_success() {
            let status = response.status();
            let body = response.text().await.unwrap_or_default();
            return Err(AttestationVerificationError::GpuVerificationFailed(
                format!("NVIDIA NRAS returned HTTP {status}: {body}"),
            ));
        }

        // Response is an array of [category, jwt_token] pairs
        let body: serde_json::Value = response.json().await.map_err(|e| {
            AttestationVerificationError::GpuVerificationFailed(format!(
                "failed to parse NRAS response: {e}"
            ))
        })?;

        let jwt_token = body
            .as_array()
            .and_then(|arr| arr.first())
            .and_then(|entry| entry.as_array())
            .and_then(|pair| pair.get(1))
            .and_then(|v| v.as_str())
            .ok_or_else(|| {
                AttestationVerificationError::GpuVerificationFailed(
                    "unexpected NRAS response format".to_string(),
                )
            })?;

        // Decode JWT payload (second segment, base64url-encoded)
        let parsed = parse_nras_jwt(jwt_token)?;

        if parsed.verdict != "PASS" {
            let failed_checks = nras_failed_checks(&parsed.claims);
            let gpu_arch = nras_str_claim(&parsed.claims, "x-nvidia-gpu-arch");
            let driver_version = nras_str_claim(&parsed.claims, "x-nvidia-gpu-driver-version");
            let vbios_version = nras_str_claim(&parsed.claims, "x-nvidia-gpu-vbios-version");
            let detailed = parsed
                .claims
                .get("x-nvidia-attestation-detailed-result")
                .map(|v| v.to_string())
                .unwrap_or_default();

            // Log full diagnostic context once. Caller already wraps the
            // returned error in a higher-level "Failed to create verified
            // client" message, so the structured fields here are the only
            // place the per-claim breakdown surfaces.
            tracing::warn!(
                verdict = %parsed.verdict,
                failed_checks = ?failed_checks,
                gpu_arch = %gpu_arch.as_deref().unwrap_or("?"),
                driver_version = %driver_version.as_deref().unwrap_or("?"),
                vbios_version = %vbios_version.as_deref().unwrap_or("?"),
                detailed_result = %detailed,
                "NVIDIA NRAS attestation failed"
            );

            let summary = if failed_checks.is_empty() {
                format!(
                    "NVIDIA attestation verdict: {} (expected PASS)",
                    parsed.verdict
                )
            } else {
                format!(
                    "NVIDIA attestation verdict: {} (expected PASS); failed checks: [{}]",
                    parsed.verdict,
                    failed_checks.join(", ")
                )
            };
            return Err(AttestationVerificationError::GpuVerificationFailed(summary));
        }

        Ok(Some(parsed.verdict))
    }
}

/// Parsed NRAS JWT: the overall verdict plus the full top-level claims map.
#[derive(Debug)]
struct NrasJwtClaims {
    verdict: String,
    claims: serde_json::Map<String, serde_json::Value>,
}

/// Decode a NRAS JWT payload and extract the overall verdict and full claims.
///
/// The JWT is unsigned (NRAS signs the response envelope, not the JWT body
/// for our purposes — we trust the TLS connection to NRAS), so we just
/// base64url-decode the payload segment and parse it as JSON.
fn parse_nras_jwt(jwt: &str) -> Result<NrasJwtClaims, AttestationVerificationError> {
    let parts: Vec<&str> = jwt.split('.').collect();
    if parts.len() < 2 {
        return Err(AttestationVerificationError::GpuVerificationFailed(
            "invalid JWT format from NRAS".to_string(),
        ));
    }

    use base64::Engine;
    let payload_bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(parts[1])
        .map_err(|e| {
            AttestationVerificationError::GpuVerificationFailed(format!(
                "failed to decode JWT payload: {e}"
            ))
        })?;

    let payload: serde_json::Value = serde_json::from_slice(&payload_bytes).map_err(|e| {
        AttestationVerificationError::GpuVerificationFailed(format!(
            "failed to parse JWT payload JSON: {e}"
        ))
    })?;

    let claims = payload
        .as_object()
        .ok_or_else(|| {
            AttestationVerificationError::GpuVerificationFailed(
                "NRAS JWT payload is not a JSON object".to_string(),
            )
        })?
        .clone();

    let result = claims.get("x-nvidia-overall-att-result").ok_or_else(|| {
        AttestationVerificationError::GpuVerificationFailed(
            "x-nvidia-overall-att-result not found in NRAS JWT".to_string(),
        )
    })?;

    // NRAS returns either boolean true/false or string "PASS"/"FAIL"
    let verdict = match result {
        serde_json::Value::Bool(b) => if *b { "PASS" } else { "FAIL" }.to_string(),
        serde_json::Value::String(s) => s.clone(),
        other => other.to_string(),
    };

    Ok(NrasJwtClaims { verdict, claims })
}

/// Collect the names of any NRAS sub-checks that returned `false`.
///
/// NRAS exposes per-check booleans both as flat top-level claims
/// (e.g. `x-nvidia-gpu-driver-rim-fetched`) and as a nested
/// `x-nvidia-attestation-detailed-result` object — different NRAS versions
/// emit one or the other, so we walk both. Returns sorted, deduped names so
/// log lines are stable across runs.
fn nras_failed_checks(claims: &serde_json::Map<String, serde_json::Value>) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for (key, value) in claims {
        if key == "x-nvidia-overall-att-result" {
            continue;
        }
        match value {
            serde_json::Value::Bool(false) if key.starts_with("x-nvidia-") => {
                out.push(key.clone());
            }
            serde_json::Value::Object(nested) if key.contains("detailed-result") => {
                for (nested_key, nested_value) in nested {
                    if matches!(nested_value, serde_json::Value::Bool(false)) {
                        out.push(format!("{key}.{nested_key}"));
                    }
                }
            }
            _ => {}
        }
    }
    out.sort();
    out.dedup();
    out
}

/// Read a string-typed NRAS claim, if present.
fn nras_str_claim(
    claims: &serde_json::Map<String, serde_json::Value>,
    key: &str,
) -> Option<String> {
    claims
        .get(key)
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
}

#[derive(Debug, thiserror::Error)]
pub enum AttestationVerificationError {
    #[error("missing field in attestation report: {0}")]
    MissingField(String),

    #[error("invalid format: {0}")]
    InvalidFormat(String),

    #[error("TDX quote verification failed: {0}")]
    TdxVerificationFailed(String),

    #[error("report data binding mismatch: {0}")]
    ReportDataMismatch(String),

    #[error("RTMR3 replay mismatch: {0}")]
    RtmrMismatch(String),

    #[error("OS image hash mismatch: {0}")]
    ImageHashMismatch(String),

    #[error("GPU evidence verification failed: {0}")]
    GpuVerificationFailed(String),
}

#[cfg(test)]
mod tests {
    use super::*;
    use base64::Engine;
    use serde_json::json;

    fn make_jwt(payload: serde_json::Value) -> String {
        let header = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .encode(br#"{"alg":"none","typ":"JWT"}"#);
        let body = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .encode(serde_json::to_vec(&payload).unwrap());
        format!("{header}.{body}.")
    }

    #[test]
    fn parse_nras_jwt_pass_boolean() {
        let jwt = make_jwt(json!({
            "x-nvidia-overall-att-result": true,
            "x-nvidia-gpu-arch": "HOPPER",
        }));
        let parsed = parse_nras_jwt(&jwt).unwrap();
        assert_eq!(parsed.verdict, "PASS");
        assert_eq!(
            parsed
                .claims
                .get("x-nvidia-gpu-arch")
                .and_then(|v| v.as_str()),
            Some("HOPPER")
        );
    }

    #[test]
    fn parse_nras_jwt_fail_string() {
        let jwt = make_jwt(json!({"x-nvidia-overall-att-result": "FAIL"}));
        let parsed = parse_nras_jwt(&jwt).unwrap();
        assert_eq!(parsed.verdict, "FAIL");
    }

    #[test]
    fn parse_nras_jwt_missing_verdict() {
        let jwt = make_jwt(json!({"x-nvidia-gpu-arch": "HOPPER"}));
        let err = parse_nras_jwt(&jwt).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("x-nvidia-overall-att-result not found"),
            "{msg}"
        );
    }

    #[test]
    fn nras_failed_checks_collects_flat_booleans() {
        let claims = json!({
            "x-nvidia-overall-att-result": false,
            "x-nvidia-gpu-driver-rim-fetched": false,
            "x-nvidia-gpu-attestation-report-cert-chain-validated": true,
            "x-nvidia-gpu-vbios-rim-version-match": false,
            "x-nvidia-gpu-arch": "HOPPER",
            "iat": 123,
        })
        .as_object()
        .unwrap()
        .clone();
        let failed = nras_failed_checks(&claims);
        assert_eq!(
            failed,
            vec![
                "x-nvidia-gpu-driver-rim-fetched".to_string(),
                "x-nvidia-gpu-vbios-rim-version-match".to_string(),
            ]
        );
    }

    #[test]
    fn nras_failed_checks_walks_nested_detailed_result() {
        let claims = json!({
            "x-nvidia-overall-att-result": false,
            "x-nvidia-attestation-detailed-result": {
                "x-nvidia-gpu-driver-rim-fetched": false,
                "x-nvidia-gpu-vbios-rim-cert-validated": true,
                "x-nvidia-gpu-arch-check": false,
            },
        })
        .as_object()
        .unwrap()
        .clone();
        let failed = nras_failed_checks(&claims);
        assert_eq!(
            failed,
            vec![
                "x-nvidia-attestation-detailed-result.x-nvidia-gpu-arch-check".to_string(),
                "x-nvidia-attestation-detailed-result.x-nvidia-gpu-driver-rim-fetched".to_string(),
            ]
        );
    }

    #[test]
    fn nras_failed_checks_empty_when_all_pass() {
        let claims = json!({
            "x-nvidia-overall-att-result": true,
            "x-nvidia-gpu-driver-rim-fetched": true,
        })
        .as_object()
        .unwrap()
        .clone();
        assert!(nras_failed_checks(&claims).is_empty());
    }
}
