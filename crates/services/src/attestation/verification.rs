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

/// Inter-model stagger for cumulative discovery on each refresh cycle (milliseconds).
///
/// When the provider pool refreshes, it runs cumulative attestation discovery
/// for every reused model. Without staggering, all models fire their first
/// discovery call at t=0, creating a burst that saturates the GPU evidence
/// worker on dense hosts (e.g. gpu04 runs 8+ model instances).
///
/// With this stagger, model i starts its discovery after `i * MODEL_DISCOVERY_STAGGER_MS`
/// delay. At 2 s/model the burst is spread across tens of seconds rather than
/// a single wall-clock instant, while still completing well within the 5-minute
/// refresh interval even for large pools.
pub const MODEL_DISCOVERY_STAGGER_MS: u64 = 2_000;

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

        // NRAS response shape (verified against a real production response):
        //   [
        //     ["JWT", "<overall_eat>"],            // index 0: overall verdict
        //     {"GPU-0": "<jwt>", ..., "GPU-N": "<jwt>"}  // index 1: per-GPU detail
        //   ]
        // The overall EAT contains only `x-nvidia-overall-att-result`,
        // `x-nvidia-ver`, and submod digests — *no* per-check booleans or
        // version metadata. All the diagnostic claims we want for failure
        // logging (`x-nvidia-gpu-driver-rim-fetched`, driver/vbios version,
        // `hwmodel`, etc.) live in the per-GPU EATs at index 1.
        let body: serde_json::Value = response.json().await.map_err(|e| {
            AttestationVerificationError::GpuVerificationFailed(format!(
                "failed to parse NRAS response: {e}"
            ))
        })?;

        let overall_jwt = body
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
        let verdict = parse_nras_overall(overall_jwt)?;

        if verdict != "PASS" {
            // Walk the per-GPU EATs (body[1]) for diagnostic detail.
            let gpus = parse_per_gpu_diagnostics(&body);
            let agg = aggregate_gpu_diagnostics(&gpus);

            // Single structured warning with everything an operator needs to
            // tell apart "NVIDIA hasn't published a RIM for this driver yet"
            // from "our cert chain broke" from "GPU is in debug mode". Caller
            // wraps the returned error string in a higher-level "Failed to
            // create verified client" message, so this `warn!` is the only
            // place the per-claim breakdown surfaces.
            tracing::warn!(
                verdict = %verdict,
                total_gpus = agg.total_gpus,
                failed_gpus = agg.failed_gpus,
                hwmodels = ?agg.hwmodels,
                driver_versions = ?agg.driver_versions,
                vbios_versions = ?agg.vbios_versions,
                failed_checks = ?agg.failed_checks,
                gpu_errors = ?agg.gpu_errors,
                attestation_warnings = ?agg.warnings,
                "NVIDIA NRAS attestation failed"
            );

            let summary = format_failure_summary(&verdict, &agg);
            return Err(AttestationVerificationError::GpuVerificationFailed(summary));
        }

        Ok(Some(verdict))
    }
}

/// Decode a NRAS JWT payload into its claims map.
///
/// Used for both the overall EAT and per-GPU EATs — the two have *different*
/// claim schemas (overall has `x-nvidia-overall-att-result`; per-GPU has the
/// per-check booleans and version metadata) so this stays schema-agnostic.
/// Schema-specific extraction lives in `parse_nras_overall` and
/// `parse_per_gpu_diagnostics`.
///
/// The JWT is unsigned by us (NRAS signs the response envelope, not the JWT
/// body for our purposes — we trust the TLS connection to NRAS), so we just
/// base64url-decode the payload segment and parse it as JSON.
fn decode_nras_jwt_claims(
    jwt: &str,
) -> Result<serde_json::Map<String, serde_json::Value>, AttestationVerificationError> {
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

    // Pattern-match to take ownership of the inner Map without cloning, and
    // enumerate the other variants explicitly so a future serde_json change
    // forces us to reconsider the classification.
    match payload {
        serde_json::Value::Object(map) => Ok(map),
        serde_json::Value::Null
        | serde_json::Value::Bool(_)
        | serde_json::Value::Number(_)
        | serde_json::Value::String(_)
        | serde_json::Value::Array(_) => Err(AttestationVerificationError::GpuVerificationFailed(
            "NRAS JWT payload is not a JSON object".to_string(),
        )),
    }
}

/// Decode the overall NRAS EAT and return the verdict string.
///
/// Only valid for `body[0][1]` — per-GPU EATs do *not* contain
/// `x-nvidia-overall-att-result` (they have the per-check booleans instead),
/// so calling this on a per-GPU JWT will return a "verdict not found" error.
fn parse_nras_overall(jwt: &str) -> Result<String, AttestationVerificationError> {
    let claims = decode_nras_jwt_claims(jwt)?;

    let result = claims.get("x-nvidia-overall-att-result").ok_or_else(|| {
        AttestationVerificationError::GpuVerificationFailed(
            "x-nvidia-overall-att-result not found in NRAS JWT".to_string(),
        )
    })?;

    // NRAS returns either boolean true/false or string "PASS"/"FAIL"
    let verdict = match result {
        serde_json::Value::Bool(b) => if *b { "PASS" } else { "FAIL" }.to_string(),
        serde_json::Value::String(s) => s.clone(),
        serde_json::Value::Null
        | serde_json::Value::Number(_)
        | serde_json::Value::Array(_)
        | serde_json::Value::Object(_) => result.to_string(),
    };

    Ok(verdict)
}

/// Collect the names of `x-nvidia-gpu-*` boolean checks that returned `false`
/// in a single per-GPU claims map. Sorted for stable log output.
///
/// We only consider keys with the `x-nvidia-gpu-` prefix because NRAS also
/// emits non-check booleans (e.g. `secboot`) whose `false` value is a piece
/// of state, not a check failure.
fn nras_failed_checks(claims: &serde_json::Map<String, serde_json::Value>) -> Vec<String> {
    let mut out: Vec<String> = claims
        .iter()
        .filter_map(|(key, value)| match value {
            serde_json::Value::Bool(false) if key.starts_with("x-nvidia-gpu-") => Some(key.clone()),
            // Listed explicitly so a future serde_json variant addition forces
            // a compile-time decision rather than silently slipping through.
            serde_json::Value::Null
            | serde_json::Value::Bool(_)
            | serde_json::Value::Number(_)
            | serde_json::Value::String(_)
            | serde_json::Value::Array(_)
            | serde_json::Value::Object(_) => None,
        })
        .collect();
    out.sort();
    out
}

/// Read a string-typed NRAS claim, if present. Returns a borrow into the
/// caller-owned claims map — no allocation on the failure path.
fn nras_str_claim<'a>(
    claims: &'a serde_json::Map<String, serde_json::Value>,
    key: &str,
) -> Option<&'a str> {
    claims.get(key).and_then(|v| v.as_str())
}

/// `x-nvidia-error-details` payload that appears on per-GPU JWTs when
/// NRAS rejected that GPU's evidence (e.g. signature mismatch, nonce
/// mismatch). Only emitted when there's a hard rejection — happy-path
/// JWTs use the boolean `x-nvidia-gpu-*` claims instead.
#[derive(Debug, Default, Clone)]
struct NrasGpuError {
    /// Numeric code (e.g. 4010 for `NONCE_NOT_MATCHING`, 4013 for
    /// `INVALID_EVIDENCE_SIGNATURE`).
    code: Option<i64>,
    /// Short machine-readable identifier (e.g. `NONCE_NOT_MATCHING`).
    message: Option<String>,
    /// Long human-readable description.
    description: Option<String>,
}

impl NrasGpuError {
    /// One-line label for log lists: prefer the short message, fall back to
    /// description, fall back to the code.
    fn short_label(&self) -> String {
        if let Some(m) = &self.message {
            return m.clone();
        }
        if let Some(d) = &self.description {
            return d.clone();
        }
        if let Some(c) = self.code {
            return format!("code:{c}");
        }
        "unknown_error".to_string()
    }

    fn is_present(&self) -> bool {
        self.code.is_some() || self.message.is_some() || self.description.is_some()
    }
}

fn parse_nras_gpu_error(claims: &serde_json::Map<String, serde_json::Value>) -> NrasGpuError {
    let Some(obj) = claims
        .get("x-nvidia-error-details")
        .and_then(|v| v.as_object())
    else {
        return NrasGpuError::default();
    };
    NrasGpuError {
        code: obj.get("code").and_then(|v| v.as_i64()),
        message: obj
            .get("message")
            .and_then(|v| v.as_str())
            .map(String::from),
        description: obj
            .get("description")
            .and_then(|v| v.as_str())
            .map(String::from),
    }
}

/// Diagnostic claims extracted from one GPU's EAT JWT.
///
/// NRAS uses one of two per-GPU JWT shapes depending on whether the GPU's
/// evidence verified successfully:
///
/// - **Pass shape:** `hwmodel`, `x-nvidia-gpu-driver-version`,
///   `x-nvidia-gpu-vbios-version`, and the 17 `x-nvidia-gpu-*` boolean
///   checks (some of which may be `false` even on an overall-PASS verdict).
/// - **Fail shape:** none of the above — just the JWT envelope plus an
///   `x-nvidia-error-details` object carrying a code, short message, and
///   description (e.g. `4013 / INVALID_EVIDENCE_SIGNATURE`).
///
/// We extract whichever is present; both are valuable for triage.
#[derive(Debug, Default, Clone)]
struct GpuDiagnostic {
    gpu_id: String,
    hwmodel: Option<String>,
    driver_version: Option<String>,
    vbios_version: Option<String>,
    warning: Option<String>,
    failed_checks: Vec<String>,
    error: NrasGpuError,
}

impl GpuDiagnostic {
    /// A GPU "failed" if it has either a hard error from NRAS or any
    /// `x-nvidia-gpu-*: false` boolean check.
    fn is_failed(&self) -> bool {
        self.error.is_present() || !self.failed_checks.is_empty()
    }
}

/// Parse the per-GPU EAT JWTs from `body[1]` of an NRAS response.
///
/// Returns one `GpuDiagnostic` per successfully parsed GPU JWT, sorted by
/// `gpu_id` so log output is stable. JWTs that fail to parse are skipped —
/// we already know the overall verdict at this point and the goal here is
/// best-effort diagnostic, not validation.
fn parse_per_gpu_diagnostics(body: &serde_json::Value) -> Vec<GpuDiagnostic> {
    let Some(arr) = body.as_array() else {
        return Vec::new();
    };
    let Some(map) = arr.get(1).and_then(|v| v.as_object()) else {
        return Vec::new();
    };

    let mut out: Vec<GpuDiagnostic> = map
        .iter()
        .filter_map(|(gpu_id, jwt_value)| {
            let jwt = jwt_value.as_str()?;
            // Per-GPU JWTs do NOT contain `x-nvidia-overall-att-result`, so
            // decode claims directly instead of going through
            // `parse_nras_overall` (which would error on every GPU).
            let claims = decode_nras_jwt_claims(jwt).ok()?;
            Some(GpuDiagnostic {
                gpu_id: gpu_id.clone(),
                hwmodel: nras_str_claim(&claims, "hwmodel").map(String::from),
                driver_version: nras_str_claim(&claims, "x-nvidia-gpu-driver-version")
                    .map(String::from),
                vbios_version: nras_str_claim(&claims, "x-nvidia-gpu-vbios-version")
                    .map(String::from),
                warning: nras_str_claim(&claims, "x-nvidia-attestation-warning").map(String::from),
                failed_checks: nras_failed_checks(&claims),
                error: parse_nras_gpu_error(&claims),
            })
        })
        .collect();
    out.sort_by(|a, b| a.gpu_id.cmp(&b.gpu_id));
    out
}

/// Aggregated view across every GPU in an NRAS response, formatted for
/// inclusion in a single log line. Distinct sets are used for metadata so
/// heterogeneous-GPU hosts surface the variance instead of hiding it behind
/// "the first GPU's value".
#[derive(Debug, Default)]
struct AggregatedGpuDiagnostics {
    total_gpus: usize,
    failed_gpus: usize,
    hwmodels: Vec<String>,
    driver_versions: Vec<String>,
    vbios_versions: Vec<String>,
    warnings: Vec<String>,
    /// Union of `x-nvidia-gpu-*: false` check names across all GPUs.
    failed_checks: Vec<String>,
    /// Union of NRAS error labels (e.g. `NONCE_NOT_MATCHING`,
    /// `INVALID_EVIDENCE_SIGNATURE`) extracted from per-GPU
    /// `x-nvidia-error-details` objects. Distinct from `failed_checks`
    /// because the two come from different (mutually exclusive) JWT shapes.
    gpu_errors: Vec<String>,
}

fn aggregate_gpu_diagnostics(gpus: &[GpuDiagnostic]) -> AggregatedGpuDiagnostics {
    use std::collections::BTreeSet;
    let mut hwmodels = BTreeSet::new();
    let mut drivers = BTreeSet::new();
    let mut vbios = BTreeSet::new();
    let mut warnings = BTreeSet::new();
    let mut checks = BTreeSet::new();
    let mut errors = BTreeSet::new();
    let mut failed_gpu_count = 0;

    for g in gpus {
        if g.is_failed() {
            failed_gpu_count += 1;
        }
        if let Some(s) = &g.hwmodel {
            hwmodels.insert(s.clone());
        }
        if let Some(s) = &g.driver_version {
            drivers.insert(s.clone());
        }
        if let Some(s) = &g.vbios_version {
            vbios.insert(s.clone());
        }
        if let Some(s) = &g.warning {
            warnings.insert(s.clone());
        }
        for c in &g.failed_checks {
            checks.insert(c.clone());
        }
        if g.error.is_present() {
            errors.insert(g.error.short_label());
        }
    }

    AggregatedGpuDiagnostics {
        total_gpus: gpus.len(),
        failed_gpus: failed_gpu_count,
        hwmodels: hwmodels.into_iter().collect(),
        driver_versions: drivers.into_iter().collect(),
        vbios_versions: vbios.into_iter().collect(),
        warnings: warnings.into_iter().collect(),
        failed_checks: checks.into_iter().collect(),
        gpu_errors: errors.into_iter().collect(),
    }
}

fn format_failure_summary(verdict: &str, agg: &AggregatedGpuDiagnostics) -> String {
    let base = format!("NVIDIA attestation verdict: {verdict} (expected PASS)");
    if agg.total_gpus == 0 {
        // No per-GPU detail to add.
        return base;
    }
    let mut s = format!(
        "{base}; {}/{} GPU(s) failed",
        agg.failed_gpus, agg.total_gpus
    );
    if !agg.failed_checks.is_empty() {
        s.push_str(&format!(
            "; failed checks: [{}]",
            agg.failed_checks.join(", ")
        ));
    }
    if !agg.gpu_errors.is_empty() {
        s.push_str(&format!("; gpu errors: [{}]", agg.gpu_errors.join(", ")));
    }
    s
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

    /// Build a fake NRAS response in the wire format observed in production:
    ///   [["JWT", overall_eat], {"GPU-0": jwt, ...}]
    fn make_nras_response(
        overall: serde_json::Value,
        per_gpu: Vec<(&str, serde_json::Value)>,
    ) -> serde_json::Value {
        let gpus: serde_json::Map<String, serde_json::Value> = per_gpu
            .into_iter()
            .map(|(id, claims)| (id.to_string(), serde_json::Value::String(make_jwt(claims))))
            .collect();
        json!([["JWT", make_jwt(overall)], serde_json::Value::Object(gpus),])
    }

    /// Realistic per-GPU claims, modeled on a captured production NRAS
    /// response. `failed` flips a subset of the boolean checks to `false`.
    ///
    /// Important: real per-GPU EATs do **not** contain
    /// `x-nvidia-overall-att-result` — that claim only appears in the
    /// overall EAT at `body[0][1]`. Including it here would mask the bug
    /// class where code accidentally reuses an "overall verdict"-style
    /// parser on per-GPU JWTs (which is exactly the miss this PR fixes).
    fn realistic_gpu_claims(
        driver: &str,
        vbios: &str,
        hwmodel: &str,
        failed: &[&str],
    ) -> serde_json::Value {
        let mut obj = json!({
            "iss": "https://nras.attestation.nvidia.com",
            "eat_nonce": "deadbeef",
            "iat": 1_777_893_228_i64,
            "exp": 1_777_896_828_i64,
            "nbf": 1_777_893_228_i64,
            "ueid": "1",
            "jti": "2144daf6-da5e-454c-a7ee-ae7694d07bf4",
            "hwmodel": hwmodel,
            "oemid": "5703",
            "dbgstat": "disabled",
            "secboot": true,
            "measres": "success",
            "x-nvidia-gpu-driver-version": driver,
            "x-nvidia-gpu-vbios-version": vbios,
            "x-nvidia-attestation-warning": null,
            // The 17 boolean checks observed in the captured response,
            // all PASS by default.
            "x-nvidia-gpu-arch-check": true,
            "x-nvidia-gpu-attestation-report-cert-chain-validated": true,
            "x-nvidia-gpu-attestation-report-nonce-match": true,
            "x-nvidia-gpu-attestation-report-parsed": true,
            "x-nvidia-gpu-attestation-report-signature-verified": true,
            "x-nvidia-gpu-driver-rim-cert-validated": true,
            "x-nvidia-gpu-driver-rim-fetched": true,
            "x-nvidia-gpu-driver-rim-measurements-available": true,
            "x-nvidia-gpu-driver-rim-schema-validated": true,
            "x-nvidia-gpu-driver-rim-signature-verified": true,
            "x-nvidia-gpu-vbios-index-no-conflict": true,
            "x-nvidia-gpu-vbios-rim-cert-validated": true,
            "x-nvidia-gpu-vbios-rim-fetched": true,
            "x-nvidia-gpu-vbios-rim-measurements-available": true,
            "x-nvidia-gpu-vbios-rim-schema-validated": true,
            "x-nvidia-gpu-vbios-rim-signature-verified": true,
        });
        let map = obj.as_object_mut().expect("object");
        for f in failed {
            map.insert((*f).to_string(), json!(false));
        }
        obj
    }

    #[test]
    fn parse_nras_overall_pass_boolean() {
        let jwt = make_jwt(json!({"x-nvidia-overall-att-result": true}));
        assert_eq!(parse_nras_overall(&jwt).unwrap(), "PASS");
    }

    #[test]
    fn parse_nras_overall_fail_string() {
        let jwt = make_jwt(json!({"x-nvidia-overall-att-result": "FAIL"}));
        assert_eq!(parse_nras_overall(&jwt).unwrap(), "FAIL");
    }

    #[test]
    fn parse_nras_overall_fail_boolean() {
        let jwt = make_jwt(json!({"x-nvidia-overall-att-result": false}));
        assert_eq!(parse_nras_overall(&jwt).unwrap(), "FAIL");
    }

    #[test]
    fn parse_nras_overall_missing_verdict_errors() {
        // A per-GPU EAT lacks `x-nvidia-overall-att-result` — feeding one
        // to the overall parser must fail loudly so callers don't silently
        // dispatch the wrong parser. (The previous version of this PR did
        // exactly that for per-GPU JWTs.)
        let jwt = make_jwt(realistic_gpu_claims(
            "570.172.08",
            "96.00.CF.00.02",
            "GH100",
            &[],
        ));
        let err = parse_nras_overall(&jwt).unwrap_err();
        assert!(
            err.to_string()
                .contains("x-nvidia-overall-att-result not found"),
            "{err}"
        );
    }

    #[test]
    fn decode_nras_jwt_claims_works_for_per_gpu_eat() {
        // Per-GPU EAT — no `x-nvidia-overall-att-result`. The claims
        // decoder must accept it.
        let jwt = make_jwt(realistic_gpu_claims(
            "570.172.08",
            "96.00.CF.00.02",
            "GH100",
            &["x-nvidia-gpu-driver-rim-fetched"],
        ));
        let claims = decode_nras_jwt_claims(&jwt).expect("decode");
        assert_eq!(
            claims
                .get("x-nvidia-gpu-driver-rim-fetched")
                .and_then(|v| v.as_bool()),
            Some(false)
        );
        assert_eq!(
            claims.get("hwmodel").and_then(|v| v.as_str()),
            Some("GH100")
        );
    }

    #[test]
    fn nras_failed_checks_only_collects_x_nvidia_gpu_booleans() {
        // Mix of real-shape claims: x-nvidia-gpu-* booleans (the only ones
        // we should pick up), other booleans like `secboot`, string
        // metadata, and a non-x-nvidia-gpu boolean like `measres` (which
        // is actually a string in real NRAS but the test pins behavior
        // even if the schema drifts).
        let claims = json!({
            "x-nvidia-overall-att-result": false,
            "x-nvidia-gpu-driver-rim-fetched": false,
            "x-nvidia-gpu-vbios-rim-cert-validated": false,
            "x-nvidia-gpu-arch-check": true,
            "secboot": false,
            "x-nvidia-gpu-driver-version": "570.172.08",
            "x-nvidia-attestation-warning": null,
        })
        .as_object()
        .unwrap()
        .clone();
        assert_eq!(
            nras_failed_checks(&claims),
            vec![
                "x-nvidia-gpu-driver-rim-fetched".to_string(),
                "x-nvidia-gpu-vbios-rim-cert-validated".to_string(),
            ]
        );
    }

    #[test]
    fn nras_failed_checks_empty_when_all_pass() {
        let claims = realistic_gpu_claims("570.172.08", "96.00.CF.00.02", "GH100", &[])
            .as_object()
            .unwrap()
            .clone();
        assert!(nras_failed_checks(&claims).is_empty());
    }

    #[test]
    fn parse_per_gpu_diagnostics_extracts_versions_and_failed_checks() {
        // Two GPUs: one healthy, one with two failed checks. Same driver
        // and hwmodel, different VBIOS to exercise the distinct-set
        // aggregation path.
        let body = make_nras_response(
            json!({"x-nvidia-overall-att-result": false}),
            vec![
                (
                    "GPU-0",
                    realistic_gpu_claims("570.172.08", "96.00.CF.00.02", "GH100", &[]),
                ),
                (
                    "GPU-1",
                    realistic_gpu_claims(
                        "570.172.08",
                        "96.00.CF.00.99",
                        "GH100",
                        &[
                            "x-nvidia-gpu-driver-rim-fetched",
                            "x-nvidia-gpu-vbios-rim-fetched",
                        ],
                    ),
                ),
            ],
        );
        let gpus = parse_per_gpu_diagnostics(&body);
        assert_eq!(gpus.len(), 2);
        // Sorted by gpu_id.
        assert_eq!(gpus[0].gpu_id, "GPU-0");
        assert_eq!(gpus[1].gpu_id, "GPU-1");
        assert_eq!(gpus[0].failed_checks.len(), 0);
        assert_eq!(
            gpus[1].failed_checks,
            vec![
                "x-nvidia-gpu-driver-rim-fetched".to_string(),
                "x-nvidia-gpu-vbios-rim-fetched".to_string(),
            ]
        );
        assert_eq!(gpus[0].driver_version.as_deref(), Some("570.172.08"));
        assert_eq!(gpus[1].vbios_version.as_deref(), Some("96.00.CF.00.99"));
        assert_eq!(gpus[0].hwmodel.as_deref(), Some("GH100"));
    }

    #[test]
    fn parse_per_gpu_diagnostics_returns_empty_when_body_shape_unexpected() {
        // body missing index 1
        assert!(parse_per_gpu_diagnostics(&json!([["JWT", "x.y.z"]])).is_empty());
        // body[1] not an object
        assert!(parse_per_gpu_diagnostics(&json!([["JWT", "x.y.z"], "wat"])).is_empty());
        // body not an array
        assert!(parse_per_gpu_diagnostics(&json!({"foo": "bar"})).is_empty());
    }

    #[test]
    fn parse_per_gpu_diagnostics_skips_unparseable_jwt_entries() {
        // GPU-0's JWT is malformed; GPU-1 is valid. We should still get
        // GPU-1 back (best-effort diagnostic).
        let body = json!([
            ["JWT", "x.y.z"],
            {
                "GPU-0": "not-a-jwt",
                "GPU-1": make_jwt(realistic_gpu_claims("570.172.08", "96.00.CF.00.02", "GH100", &["x-nvidia-gpu-driver-rim-fetched"])),
            }
        ]);
        let gpus = parse_per_gpu_diagnostics(&body);
        assert_eq!(gpus.len(), 1);
        assert_eq!(gpus[0].gpu_id, "GPU-1");
        assert_eq!(gpus[0].failed_checks.len(), 1);
    }

    #[test]
    fn aggregate_gpu_diagnostics_dedupes_metadata_and_unions_failed_checks() {
        let gpus = vec![
            GpuDiagnostic {
                gpu_id: "GPU-0".into(),
                hwmodel: Some("GH100".into()),
                driver_version: Some("570.172.08".into()),
                vbios_version: Some("96.00.CF.00.02".into()),
                warning: None,
                failed_checks: vec!["x-nvidia-gpu-driver-rim-fetched".into()],
                error: NrasGpuError::default(),
            },
            GpuDiagnostic {
                gpu_id: "GPU-1".into(),
                hwmodel: Some("GH100".into()), // same hwmodel — should dedupe
                driver_version: Some("570.172.08".into()), // same driver
                vbios_version: Some("96.00.CF.00.99".into()), // distinct vbios
                warning: Some("clock skew".into()),
                failed_checks: vec![
                    "x-nvidia-gpu-driver-rim-fetched".into(), // overlap with GPU-0
                    "x-nvidia-gpu-vbios-rim-fetched".into(),
                ],
                error: NrasGpuError::default(),
            },
            GpuDiagnostic {
                gpu_id: "GPU-2".into(),
                hwmodel: Some("GH100".into()),
                driver_version: Some("570.172.08".into()),
                vbios_version: Some("96.00.CF.00.02".into()),
                warning: None,
                failed_checks: vec![], // not failed
                error: NrasGpuError::default(),
            },
        ];
        let agg = aggregate_gpu_diagnostics(&gpus);
        assert_eq!(agg.total_gpus, 3);
        assert_eq!(agg.failed_gpus, 2);
        assert_eq!(agg.hwmodels, vec!["GH100".to_string()]);
        assert_eq!(agg.driver_versions, vec!["570.172.08".to_string()]);
        assert_eq!(
            agg.vbios_versions,
            vec!["96.00.CF.00.02".to_string(), "96.00.CF.00.99".to_string()]
        );
        assert_eq!(agg.warnings, vec!["clock skew".to_string()]);
        assert_eq!(
            agg.failed_checks,
            vec![
                "x-nvidia-gpu-driver-rim-fetched".to_string(),
                "x-nvidia-gpu-vbios-rim-fetched".to_string(),
            ]
        );
        assert!(agg.gpu_errors.is_empty());
    }

    #[test]
    fn aggregate_gpu_diagnostics_counts_error_only_gpus_as_failed_and_dedupes_errors() {
        // Mirrors the real-world FAIL shape: failing GPUs have ONLY the
        // x-nvidia-error-details payload — no version metadata, no
        // boolean checks. Same error code on multiple GPUs (e.g. the
        // captured nonce-mismatch case where every GPU got 4010) should
        // collapse to one entry in `gpu_errors`.
        let make_err = |id: &str, msg: &str| GpuDiagnostic {
            gpu_id: id.into(),
            error: NrasGpuError {
                code: Some(4010),
                message: Some(msg.into()),
                description: None,
            },
            ..GpuDiagnostic::default()
        };
        let gpus = vec![
            make_err("GPU-0", "NONCE_NOT_MATCHING"),
            make_err("GPU-1", "NONCE_NOT_MATCHING"),
            make_err("GPU-2", "NONCE_NOT_MATCHING"),
        ];
        let agg = aggregate_gpu_diagnostics(&gpus);
        assert_eq!(agg.total_gpus, 3);
        assert_eq!(agg.failed_gpus, 3);
        assert!(agg.failed_checks.is_empty());
        assert_eq!(agg.gpu_errors, vec!["NONCE_NOT_MATCHING".to_string()]);
        // No version metadata extracted from error-only GPUs — that's
        // expected and documented.
        assert!(agg.hwmodels.is_empty());
        assert!(agg.driver_versions.is_empty());
    }

    #[test]
    fn format_failure_summary_includes_gpu_count_and_checks() {
        let agg = AggregatedGpuDiagnostics {
            total_gpus: 8,
            failed_gpus: 2,
            failed_checks: vec![
                "x-nvidia-gpu-driver-rim-fetched".into(),
                "x-nvidia-gpu-vbios-rim-fetched".into(),
            ],
            ..Default::default()
        };
        let s = format_failure_summary("FAIL", &agg);
        assert_eq!(
            s,
            "NVIDIA attestation verdict: FAIL (expected PASS); 2/8 GPU(s) failed; \
             failed checks: [x-nvidia-gpu-driver-rim-fetched, x-nvidia-gpu-vbios-rim-fetched]"
        );
    }

    #[test]
    fn format_failure_summary_handles_no_per_gpu_detail() {
        // body[1] missing entirely → no per-GPU diagnostic available.
        let agg = AggregatedGpuDiagnostics::default();
        assert_eq!(
            format_failure_summary("FAIL", &agg),
            "NVIDIA attestation verdict: FAIL (expected PASS)"
        );
    }

    /// Realistic per-GPU FAIL claims, modeled on a captured production
    /// NRAS response from a tampered-evidence rejection. NRAS strips
    /// every diagnostic claim and replaces them with a single
    /// `x-nvidia-error-details` object — no `hwmodel`, no driver/vbios
    /// version, no per-check booleans.
    fn realistic_failed_gpu_claims(
        code: i64,
        message: &str,
        description: &str,
    ) -> serde_json::Value {
        json!({
            "iss": "https://nras.attestation.nvidia.com",
            "iat": 1_777_896_875_i64,
            "exp": 1_777_900_475_i64,
            "nbf": 1_777_896_875_i64,
            "jti": "deadbeef",
            "x-nvidia-error-details": {
                "code": code,
                "fieldName": null,
                "httpStatus": "400 BAD_REQUEST",
                "description": description,
                "message": message,
            }
        })
    }

    #[test]
    fn parse_per_gpu_diagnostics_extracts_error_details() {
        // 8 GPUs, all rejected with NONCE_NOT_MATCHING — the captured
        // shape from the production-style nonce mismatch case.
        let per_gpu: Vec<(&str, serde_json::Value)> = (0..8)
            .map(|i| {
                let id = [
                    "GPU-0", "GPU-1", "GPU-2", "GPU-3", "GPU-4", "GPU-5", "GPU-6", "GPU-7",
                ][i];
                (
                    id,
                    realistic_failed_gpu_claims(
                        4010,
                        "NONCE_NOT_MATCHING",
                        "Nonce from request is not matching with evidence nonce ",
                    ),
                )
            })
            .collect();
        let body = make_nras_response(json!({"x-nvidia-overall-att-result": false}), per_gpu);
        let gpus = parse_per_gpu_diagnostics(&body);
        assert_eq!(gpus.len(), 8);
        for g in &gpus {
            assert!(g.is_failed(), "{}", g.gpu_id);
            assert_eq!(g.error.code, Some(4010));
            assert_eq!(g.error.message.as_deref(), Some("NONCE_NOT_MATCHING"));
            assert!(g.failed_checks.is_empty());
            assert!(g.hwmodel.is_none());
        }
        let agg = aggregate_gpu_diagnostics(&gpus);
        assert_eq!(agg.failed_gpus, 8);
        assert_eq!(agg.gpu_errors, vec!["NONCE_NOT_MATCHING".to_string()]);
    }

    #[test]
    fn parse_per_gpu_diagnostics_handles_mixed_pass_and_fail_gpus() {
        // The captured tampered-evidence case: GPU-0 fails with a
        // signature error, GPU-1 through GPU-7 pass. Aggregate should
        // report 1/8 failed plus the error label, while still picking up
        // metadata from the passing GPUs.
        let mut per_gpu: Vec<(&str, serde_json::Value)> = vec![(
            "GPU-0",
            realistic_failed_gpu_claims(
                4013,
                "INVALID_EVIDENCE_SIGNATURE",
                "Attestation Report Signature is Invalid",
            ),
        )];
        for id in [
            "GPU-1", "GPU-2", "GPU-3", "GPU-4", "GPU-5", "GPU-6", "GPU-7",
        ] {
            per_gpu.push((
                id,
                realistic_gpu_claims("570.172.08", "96.00.CF.00.02", "GH100", &[]),
            ));
        }
        let body = make_nras_response(json!({"x-nvidia-overall-att-result": false}), per_gpu);
        let agg = aggregate_gpu_diagnostics(&parse_per_gpu_diagnostics(&body));
        assert_eq!(agg.total_gpus, 8);
        assert_eq!(agg.failed_gpus, 1);
        assert_eq!(
            agg.gpu_errors,
            vec!["INVALID_EVIDENCE_SIGNATURE".to_string()]
        );
        assert_eq!(agg.hwmodels, vec!["GH100".to_string()]);
        assert_eq!(agg.driver_versions, vec!["570.172.08".to_string()]);
    }

    #[test]
    fn nras_gpu_error_short_label_prefers_message_then_description_then_code() {
        let with_msg = NrasGpuError {
            code: Some(4010),
            message: Some("NONCE_NOT_MATCHING".into()),
            description: Some("Nonce mismatch".into()),
        };
        assert_eq!(with_msg.short_label(), "NONCE_NOT_MATCHING");

        let without_msg = NrasGpuError {
            code: Some(9999),
            message: None,
            description: Some("Some new error NRAS hasn't given us a short name for".into()),
        };
        assert_eq!(
            without_msg.short_label(),
            "Some new error NRAS hasn't given us a short name for"
        );

        let bare = NrasGpuError {
            code: Some(1234),
            message: None,
            description: None,
        };
        assert_eq!(bare.short_label(), "code:1234");

        let empty = NrasGpuError::default();
        assert_eq!(empty.short_label(), "unknown_error");
        assert!(!empty.is_present());
    }

    #[test]
    fn format_failure_summary_includes_gpu_errors() {
        let agg = AggregatedGpuDiagnostics {
            total_gpus: 8,
            failed_gpus: 8,
            gpu_errors: vec!["NONCE_NOT_MATCHING".into()],
            ..Default::default()
        };
        let s = format_failure_summary("FAIL", &agg);
        assert_eq!(
            s,
            "NVIDIA attestation verdict: FAIL (expected PASS); 8/8 GPU(s) failed; \
             gpu errors: [NONCE_NOT_MATCHING]"
        );
    }

    #[test]
    fn end_to_end_fail_pipeline_produces_actionable_summary() {
        // Mimic a realistic FAIL scenario: 8 GPUs, 2 of them with the same
        // RIM-fetch failure (the most common production pattern when NVIDIA
        // hasn't published a RIM for a new driver version yet).
        let mut per_gpu = Vec::new();
        for i in 0..8 {
            let id = format!("GPU-{i}");
            let claims = if i < 2 {
                realistic_gpu_claims(
                    "570.999.00",
                    "96.00.CF.00.02",
                    "GH100",
                    &["x-nvidia-gpu-driver-rim-fetched"],
                )
            } else {
                realistic_gpu_claims("570.999.00", "96.00.CF.00.02", "GH100", &[])
            };
            per_gpu.push((id, claims));
        }
        let body = make_nras_response(
            json!({"x-nvidia-overall-att-result": false}),
            per_gpu
                .iter()
                .map(|(s, c)| (s.as_str(), c.clone()))
                .collect(),
        );
        let gpus = parse_per_gpu_diagnostics(&body);
        let agg = aggregate_gpu_diagnostics(&gpus);
        assert_eq!(agg.total_gpus, 8);
        assert_eq!(agg.failed_gpus, 2);
        assert_eq!(agg.driver_versions, vec!["570.999.00".to_string()]);
        assert_eq!(agg.hwmodels, vec!["GH100".to_string()]);
        let summary = format_failure_summary("FAIL", &agg);
        assert_eq!(
            summary,
            "NVIDIA attestation verdict: FAIL (expected PASS); 2/8 GPU(s) failed; \
             failed checks: [x-nvidia-gpu-driver-rim-fetched]"
        );
    }

    /// Local-only sanity check against a captured production NRAS response.
    /// Skipped in CI — set `NRAS_FIXTURE` to a JSON file containing the raw
    /// NRAS response body to run.
    ///
    /// Catches the bug class where the parser compiles, runs, but silently
    /// drops every real field (the empty-fields outcome that #561 and #569
    /// each produced before reaching staging). Asserts that *something*
    /// from each useful claim type is extracted: either pass-shape metadata
    /// or fail-shape error details, depending on what the real response
    /// contains. An empty result for both is the bug.
    #[test]
    #[ignore]
    fn parse_per_gpu_diagnostics_against_captured_response() {
        let path = match std::env::var("NRAS_FIXTURE") {
            Ok(v) => v,
            Err(_) => {
                eprintln!("skipped: NRAS_FIXTURE env var not set");
                return;
            }
        };
        let body: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        let gpus = parse_per_gpu_diagnostics(&body);
        let agg = aggregate_gpu_diagnostics(&gpus);
        eprintln!("captured: total_gpus={}", agg.total_gpus);
        eprintln!("captured: failed_gpus={}", agg.failed_gpus);
        eprintln!("captured: hwmodels={:?}", agg.hwmodels);
        eprintln!("captured: driver_versions={:?}", agg.driver_versions);
        eprintln!("captured: vbios_versions={:?}", agg.vbios_versions);
        eprintln!("captured: failed_checks={:?}", agg.failed_checks);
        eprintln!("captured: gpu_errors={:?}", agg.gpu_errors);

        assert!(agg.total_gpus > 0, "expected ≥1 GPU in real response");

        // At least one diagnostic surface must be populated. Pass-shape
        // GPUs give us metadata; fail-shape GPUs give us errors. Both
        // empty is the silent-drop bug we're guarding against.
        let has_pass_metadata = !agg.hwmodels.is_empty()
            && !agg.driver_versions.is_empty()
            && !agg.vbios_versions.is_empty();
        let has_fail_errors = !agg.gpu_errors.is_empty();
        assert!(
            has_pass_metadata || has_fail_errors,
            "captured response yielded neither pass-shape metadata nor fail-shape errors — \
             parser is silently dropping real fields"
        );

        // Cross-checks based on what's present.
        if agg.failed_gpus > 0 && !has_fail_errors {
            // Some failures use the boolean-check shape instead of
            // error-details. That's fine — failed_checks should be
            // non-empty in that case.
            assert!(
                !agg.failed_checks.is_empty(),
                "failed_gpus={} but neither gpu_errors nor failed_checks populated",
                agg.failed_gpus
            );
        }
    }

    #[test]
    fn format_failure_summary_omits_check_list_when_unknown_but_keeps_count() {
        // Some failure modes (e.g. missing claims) yield 0 failed checks
        // but we still know the GPU count from body[1].
        let agg = AggregatedGpuDiagnostics {
            total_gpus: 8,
            failed_gpus: 0,
            ..Default::default()
        };
        assert_eq!(
            format_failure_summary("FAIL", &agg),
            "NVIDIA attestation verdict: FAIL (expected PASS); 0/8 GPU(s) failed"
        );
    }
}
