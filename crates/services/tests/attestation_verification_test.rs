//! Integration test: verify attestation from real inference backends.
//!
//! Run with: cargo test -p services --test attestation_verification_test -- --nocapture
//!
//! Requires network access to completions.near.ai backends.

use std::collections::HashSet;

async fn fetch_attestation(
    url: &str,
) -> Result<serde_json::Map<String, serde_json::Value>, String> {
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(15))
        .build()
        .map_err(|e| format!("client build: {e}"))?;
    let resp = client
        .get(url)
        .send()
        .await
        .map_err(|e| format!("request: {e}"))?;
    if !resp.status().is_success() {
        return Err(format!("HTTP {}", resp.status()));
    }
    resp.json().await.map_err(|e| format!("json parse: {e}"))
}

#[tokio::test]
async fn test_verify_glm5_attestation() {
    let report = fetch_attestation(
        "https://glm-5.completions.near.ai/v1/attestation/report?signing_algo=ed25519&include_tls_fingerprint=true",
    )
    .await;

    let report = match report {
        Ok(r) => r,
        Err(e) => {
            eprintln!("Skipping test — cannot reach GLM-5 backend: {e}");
            return;
        }
    };

    let nonce = report
        .get("request_nonce")
        .and_then(|v| v.as_str())
        .expect("missing request_nonce");

    println!("Fetched attestation report:");
    println!(
        "  signing_address: {}",
        report
            .get("signing_address")
            .and_then(|v| v.as_str())
            .unwrap_or("?")
    );
    println!(
        "  tls_cert_fingerprint: {}",
        report
            .get("tls_cert_fingerprint")
            .and_then(|v| v.as_str())
            .unwrap_or("?")
    );
    println!("  request_nonce: {nonce}");

    // Verify with the known-good image hash from production
    let allowed: HashSet<String> =
        ["9b69bb1698bacbb6985409a2c272bcb892e09cdcea63d5399c6768b67d3ff677".to_string()].into();
    let verifier = services::attestation::AttestationVerifier::new(allowed, None, false);

    match verifier.verify_attestation_report(&report, nonce).await {
        Ok(verified) => {
            println!("\nVerification PASSED:");
            println!("  tcb_status: {}", verified.tcb_status);
            println!("  advisory_ids: {:?}", verified.advisory_ids);
            println!("  os_image_hash: {:?}", verified.os_image_hash);
            println!("  compose_hash: {:?}", verified.compose_hash);
            println!(
                "  tls_cert_fingerprint: {:?}",
                verified.tls_cert_fingerprint
            );
            println!("  gpu_verdict: {:?}", verified.gpu_verdict);

            assert!(
                verified.tls_cert_fingerprint.is_some(),
                "should have TLS fingerprint"
            );
            assert_eq!(
                verified.os_image_hash.as_deref(),
                Some("9b69bb1698bacbb6985409a2c272bcb892e09cdcea63d5399c6768b67d3ff677"),
                "os_image_hash should match production"
            );
        }
        Err(e) => {
            panic!("Verification FAILED: {e}");
        }
    }
}

#[tokio::test]
async fn test_verify_qwen35_attestation() {
    let report = fetch_attestation(
        "https://qwen35-122b.completions.near.ai/v1/attestation/report?signing_algo=ed25519&include_tls_fingerprint=true",
    )
    .await;

    let report = match report {
        Ok(r) => r,
        Err(e) => {
            eprintln!("Skipping test — cannot reach Qwen3.5 backend: {e}");
            return;
        }
    };

    let nonce = report
        .get("request_nonce")
        .and_then(|v| v.as_str())
        .expect("missing request_nonce");

    // No image hash allowlist — skip that check
    let verifier = services::attestation::AttestationVerifier::new(HashSet::new(), None, false);

    match verifier.verify_attestation_report(&report, nonce).await {
        Ok(verified) => {
            println!("\nQwen3.5 Verification PASSED:");
            println!("  tcb_status: {}", verified.tcb_status);
            println!("  os_image_hash: {:?}", verified.os_image_hash);
            println!("  compose_hash: {:?}", verified.compose_hash);
            println!(
                "  tls_cert_fingerprint: {:?}",
                verified.tls_cert_fingerprint
            );
            println!("  gpu_verdict: {:?}", verified.gpu_verdict);
        }
        Err(e) => {
            panic!("Qwen3.5 Verification FAILED: {e}");
        }
    }
}

#[tokio::test]
async fn test_image_hash_rejection() {
    let report = fetch_attestation(
        "https://glm-5.completions.near.ai/v1/attestation/report?signing_algo=ed25519&include_tls_fingerprint=true",
    )
    .await;

    let report = match report {
        Ok(r) => r,
        Err(e) => {
            eprintln!("Skipping test — cannot reach GLM-5 backend: {e}");
            return;
        }
    };

    let nonce = report
        .get("request_nonce")
        .and_then(|v| v.as_str())
        .expect("missing request_nonce");

    // Use a wrong image hash — should reject
    let wrong_hash: HashSet<String> = ["deadbeef00000000".to_string()].into();
    let verifier = services::attestation::AttestationVerifier::new(wrong_hash, None, false);

    let result = verifier.verify_attestation_report(&report, nonce).await;
    assert!(result.is_err(), "should reject wrong image hash");
    let err = result.unwrap_err().to_string();
    assert!(
        err.contains("not in allowed list"),
        "error should mention allowlist: {err}"
    );
    println!("Image hash rejection test PASSED: {err}");
}

#[tokio::test]
async fn test_spki_fingerprint_verifier() {
    use inference_providers::spki_verifier::{self, FingerprintState};
    use std::sync::{Arc, RwLock};

    // Test bootstrap mode (accepts any valid cert)
    let state = Arc::new(RwLock::new(FingerprintState::Bootstrap));
    let config = spki_verifier::build_rustls_config_with_verifier(state.clone());
    let client = reqwest::Client::builder()
        .use_preconfigured_tls(config)
        .build()
        .expect("client build");

    let resp = client
        .get("https://glm-5.completions.near.ai/v1/models")
        .timeout(std::time::Duration::from_secs(10))
        .send()
        .await;

    match resp {
        Ok(r) => println!(
            "Bootstrap mode: TLS connection succeeded (HTTP {})",
            r.status()
        ),
        Err(e) => {
            if e.to_string().contains("timed out") {
                eprintln!("Skipping — timeout reaching backend");
                return;
            }
            panic!("Bootstrap mode should accept any valid cert: {e}");
        }
    }

    // Now pin a wrong fingerprint — should reject
    state.write().unwrap().add_fingerprint(
        "0000000000000000000000000000000000000000000000000000000000000000".to_string(),
    );
    let config2 = spki_verifier::build_rustls_config_with_verifier(state.clone());
    let client2 = reqwest::Client::builder()
        .use_preconfigured_tls(config2)
        .build()
        .expect("client build");

    let resp2 = client2
        .get("https://glm-5.completions.near.ai/v1/models")
        .timeout(std::time::Duration::from_secs(10))
        .send()
        .await;

    assert!(
        resp2.is_err(),
        "wrong fingerprint should reject TLS connection"
    );
    let err = resp2.unwrap_err().to_string();
    println!("Wrong fingerprint rejection: {err}");
}

/// PR2 fail-closed guard (deterministic, no network): an attested third-party
/// `MeasurementPolicy` with an empty OS-image-hash allowlist must reject *before*
/// any quote work — `assert_enforceable()` is the very first step of
/// `verify_attestation_report`. This is the check that stops a Chutes-tier
/// misconfiguration from silently accepting arbitrary software.
#[tokio::test]
async fn attested3p_empty_allowlist_fails_closed_before_verification() {
    use services::attestation::{AttestationVerifier, MeasurementPolicy};

    let verifier =
        AttestationVerifier::with_policy(MeasurementPolicy::attested3p(HashSet::new()), None);

    // Empty report: if the policy guard did NOT fire first we'd get a
    // MissingField("intel_quote") error instead of the fail-closed policy error.
    let empty_report = serde_json::Map::new();
    let err = verifier
        .verify_attestation_report(&empty_report, "00")
        .await
        .expect_err("attested 3p with empty allowlist must fail closed");
    let msg = err.to_string().to_lowercase();
    assert!(
        msg.contains("fail-closed") || msg.contains("allowlist"),
        "expected fail-closed policy error, got: {err}"
    );
}

/// NEAR's own fleet with an empty allowlist must NOT fail closed at the policy
/// guard — it falls through to real verification (here surfacing the expected
/// missing-field error on an empty report), proving the historical
/// fail-open-on-empty behavior is preserved for `ProviderTier::Near`.
#[tokio::test]
async fn near_empty_allowlist_does_not_fail_closed() {
    let verifier = services::attestation::AttestationVerifier::new(HashSet::new(), None, false);
    let empty_report = serde_json::Map::new();
    let err = verifier
        .verify_attestation_report(&empty_report, "00")
        .await
        .expect_err("empty report still fails on missing quote");
    let msg = err.to_string().to_lowercase();
    assert!(
        msg.contains("intel_quote") || msg.contains("missing"),
        "NEAR empty allowlist should reach quote extraction, not a fail-closed policy error; got: {err}"
    );
}
