//! Tinfoil router/model policy: SEV-SNP verification of the router attestation
//! (ATC bundle) against compiled pins, binding the report to the router's TLS key.

use std::collections::HashMap;
use std::io::Read;
use std::sync::{Arc, Mutex};

use arc_swap::ArcSwap;
use base64::Engine;
use inference_providers::attested::tinfoil::verifier_port::{
    AtcBundle, PinSource, PinnedModel, ProxyModelEntry, RouterTcb, TinfoilVerifier,
    TinfoilVerifyError, VerifiedRouter,
};
use sha2::{Digest, Sha256};
use tinfoil_provenance::Provenance;

use super::tinfoil_trust::{verify_admin_pin_bundle, AdminModelPin, TinfoilTrust};
use crate::admin_settings::Versioned;

use super::snp::{self, SnpError, SnpEvidence, SnpPolicy, Tcb, VerifiedSnpReport};
use super::tinfoil_pins::{TinfoilPins, COMPILED_PINS_JSON};

/// The repository whose Sigstore-signed releases may vouch for a router.
const ROUTER_REPO: &str = "tinfoilsh/confidential-model-router";

/// Cap on cached Sigstore results; the cache is cleared when it grows past it.
const SIGSTORE_CACHE_MAX: usize = 64;

const SNP_FORMAT: &str = "https://tinfoil.sh/predicate/sev-snp-guest/v2";

/// Reported TCB observed on the live router (Genoa VCEK): boot 10 / tee 0 /
/// snp 23 / microcode 84. Anything older is rejected.
const MIN_TCB: Tcb = Tcb {
    bootloader: 10,
    tee: 0,
    snp: 23,
    microcode: 84,
};

/// Parse the compiled pins file.
pub fn vetted_tinfoil_pins() -> Result<TinfoilPins, String> {
    serde_json::from_str(COMPILED_PINS_JSON).map_err(|e| format!("tinfoil pins: {e}"))
}

/// The compiled pins, parsed once per process.
pub fn compiled_tinfoil_pins() -> Result<&'static TinfoilPins, &'static str> {
    static PINS: std::sync::LazyLock<Result<TinfoilPins, String>> =
        std::sync::LazyLock::new(vetted_tinfoil_pins);
    PINS.as_ref().map_err(String::as_str)
}

/// SHA-256 of the DER SubjectPublicKeyInfo of a PEM certificate.
pub fn spki_sha256_of_pem_cert(pem: &str) -> Result<[u8; 32], TinfoilVerifyError> {
    let (_, p) = x509_parser::pem::parse_x509_pem(pem.as_bytes())
        .map_err(|_| TinfoilVerifyError::Malformed { stage: "cert_pem" })?;
    let (_, cert) = x509_parser::parse_x509_certificate(&p.contents)
        .map_err(|_| TinfoilVerifyError::Malformed { stage: "cert_x509" })?;
    Ok(Sha256::digest(cert.tbs_certificate.subject_pki.raw).into())
}

/// True when the certificate's SAN lists `name` as a dNSName (exact match).
pub fn pem_cert_has_dns_name(pem: &str, name: &str) -> Result<bool, TinfoilVerifyError> {
    use x509_parser::extensions::{GeneralName, ParsedExtension};
    let (_, p) = x509_parser::pem::parse_x509_pem(pem.as_bytes())
        .map_err(|_| TinfoilVerifyError::Malformed { stage: "cert_pem" })?;
    let (_, cert) = x509_parser::parse_x509_certificate(&p.contents)
        .map_err(|_| TinfoilVerifyError::Malformed { stage: "cert_x509" })?;
    Ok(cert.extensions().iter().any(|ext| {
        matches!(
            ext.parsed_extension(),
            ParsedExtension::SubjectAlternativeName(san)
                if san.general_names.iter().any(|g| matches!(g, GeneralName::DNSName(d) if *d == name))
        )
    }))
}

fn map_snp(e: SnpError) -> TinfoilVerifyError {
    match e {
        SnpError::Chain | SnpError::Signature => TinfoilVerifyError::BadSignature,
        SnpError::Debug | SnpError::MigrateMa => TinfoilVerifyError::DebugPolicy,
        SnpError::Tcb => TinfoilVerifyError::TcbTooLow,
        SnpError::Malformed => TinfoilVerifyError::Malformed {
            stage: "snp_report",
        },
        SnpError::Product => TinfoilVerifyError::Malformed {
            stage: "snp_product",
        },
    }
}

/// Verify the SNP report chain/signature/policy without consulting any pins.
/// This does NOT check that the report is bound to the router's TLS key; use
/// [`observe_bound_router`] for that.
pub fn observe_router(bundle: &AtcBundle) -> Result<VerifiedSnpReport, TinfoilVerifyError> {
    if bundle.report.format != SNP_FORMAT {
        return Err(TinfoilVerifyError::UnsupportedRouterPlatform);
    }
    let b64 = base64::engine::general_purpose::STANDARD;
    let gz = b64
        .decode(bundle.report.body.trim())
        .map_err(|_| TinfoilVerifyError::Malformed {
            stage: "report_base64",
        })?;
    let mut report = Vec::new();
    flate2::read::GzDecoder::new(&gz[..])
        .take(64 * 1024)
        .read_to_end(&mut report)
        .map_err(|_| TinfoilVerifyError::Malformed {
            stage: "report_gzip",
        })?;
    let vcek = b64
        .decode(bundle.vcek.trim())
        .map_err(|_| TinfoilVerifyError::Malformed {
            stage: "vcek_base64",
        })?;
    snp::verify_snp_report(
        &SnpEvidence {
            report: &report,
            vcek_der: &vcek,
        },
        &SnpPolicy { min_tcb: MIN_TCB },
    )
    .map_err(map_snp)
}

/// [`observe_router`] plus the binding of the report to the router's TLS key:
/// `report_data[0..32]` must equal the SHA-256 of the enclave certificate's
/// SPKI. Pin-free; shared by the live verifier and the sync observer.
pub fn observe_bound_router(
    bundle: &AtcBundle,
) -> Result<(VerifiedSnpReport, [u8; 32]), TinfoilVerifyError> {
    let report = observe_router(bundle)?;
    let spki_sha256 = spki_sha256_of_pem_cert(&bundle.enclave_cert)?;
    if report.report_data[..32] != spki_sha256 {
        return Err(TinfoilVerifyError::ReportDataMismatch);
    }
    Ok((report, spki_sha256))
}

fn registers_eq(a: &[String], b: &[String]) -> bool {
    a.len() == b.len() && a.iter().zip(b).all(|(x, y)| x.eq_ignore_ascii_case(y))
}

type SigstoreCache = HashMap<(String, [u8; 32]), Result<Provenance, TinfoilVerifyError>>;

/// Admin pins whose Sigstore bundle verified here, with the verified provenance.
type VerifiedAdminPins = Arc<Vec<(AdminModelPin, Provenance)>>;

/// Per (repo, bundle hash) Sigstore result for admin pins; the error is the
/// reason, which never contains bundle content.
type AdminSigstoreCache = HashMap<[u8; 32], Result<Provenance, String>>;

/// Cache key of one admin pin's check result: its repo, tag and bundle content.
fn admin_pin_cache_key(pin: &AdminModelPin) -> Option<[u8; 32]> {
    let mut hasher = Sha256::new();
    hasher.update(pin.repo.as_bytes());
    hasher.update([0]);
    hasher.update(pin.tag.as_bytes());
    hasher.update([0]);
    hasher.update(serde_json::to_vec(&pin.sigstore_bundle).ok()?);
    Some(hasher.finalize().into())
}

pub struct TinfoilPolicyVerifier {
    pins: TinfoilPins,
    trust: Arc<ArcSwap<Versioned<TinfoilTrust>>>,
    /// Sigstore verification results per (measurement, bundle hash). Only the
    /// verification itself is cached: the runtime switch and the binding
    /// checks run on every `verify_router`.
    sigstore_cache: Mutex<SigstoreCache>,
    router_repo: String,
    /// The admin pins that verified for the `tinfoil_trust` version they were
    /// computed at (`None`: not computed yet). `check_model` admits only from
    /// this set, so a stored row is never trusted on its own say-so.
    admin_pins: Mutex<(Option<u64>, VerifiedAdminPins)>,
    admin_sigstore_cache: Mutex<AdminSigstoreCache>,
    #[cfg(test)]
    sigstore_calls: std::sync::atomic::AtomicUsize,
    #[cfg(test)]
    admin_sigstore_calls: std::sync::atomic::AtomicUsize,
}

impl TinfoilPolicyVerifier {
    pub fn new(pins: TinfoilPins, trust: Arc<ArcSwap<Versioned<TinfoilTrust>>>) -> Self {
        Self {
            pins,
            trust,
            sigstore_cache: Mutex::new(HashMap::new()),
            router_repo: ROUTER_REPO.to_string(),
            admin_pins: Mutex::new((None, Arc::new(Vec::new()))),
            admin_sigstore_cache: Mutex::new(HashMap::new()),
            #[cfg(test)]
            sigstore_calls: std::sync::atomic::AtomicUsize::new(0),
            #[cfg(test)]
            admin_sigstore_calls: std::sync::atomic::AtomicUsize::new(0),
        }
    }

    /// Verify one stored admin pin with the shared check (cached per repo,
    /// tag and bundle content).
    fn verify_admin_pin(
        &self,
        cache: &mut AdminSigstoreCache,
        pin: &AdminModelPin,
    ) -> Result<Provenance, String> {
        let key = admin_pin_cache_key(pin).ok_or("bundle shape")?;
        if let Some(hit) = cache.get(&key) {
            return hit.clone();
        }
        #[cfg(test)]
        self.admin_sigstore_calls
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let result = verify_admin_pin_bundle(pin);
        if cache.len() >= SIGSTORE_CACHE_MAX {
            cache.clear();
        }
        cache.insert(key, result.clone());
        result
    }

    /// The verified admin pins for the current `tinfoil_trust` version,
    /// recomputed (with a warning per dropped pin) only when the version
    /// changed since the last call.
    fn verified_admin_pins(&self, trust: &Versioned<TinfoilTrust>) -> VerifiedAdminPins {
        let mut state = self.admin_pins.lock().unwrap_or_else(|e| e.into_inner());
        if state.0 == Some(trust.version) {
            return state.1.clone();
        }
        let mut cache = self
            .admin_sigstore_cache
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let mut verified = Vec::new();
        for pin in &trust.value.model_pins {
            match self.verify_admin_pin(&mut cache, pin) {
                Ok(prov) => verified.push((pin.clone(), prov)),
                Err(reason) => tracing::warn!(
                    slug = %pin.slug,
                    repo = %pin.repo,
                    tag = %pin.tag,
                    reason = %reason,
                    "Tinfoil admin model pin failed verification; ignoring it"
                ),
            }
        }
        *state = (Some(trust.version), Arc::new(verified));
        state.1.clone()
    }

    /// Verify the ATC bundle's Sigstore release attestation (cached per
    /// measurement and bundle content).
    fn sigstore_provenance(
        &self,
        bundle: &AtcBundle,
        measurement_hex: &str,
    ) -> Result<Provenance, TinfoilVerifyError> {
        let bundle_hash: [u8; 32] = Sha256::digest(
            serde_json::to_vec(&bundle.sigstore_bundle)
                .map_err(|_| TinfoilVerifyError::RouterSigstoreRejected { stage: "verify" })?,
        )
        .into();
        let key = (measurement_hex.to_ascii_lowercase(), bundle_hash);
        let mut cache = self
            .sigstore_cache
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        if let Some(hit) = cache.get(&key) {
            return hit.clone();
        }
        #[cfg(test)]
        self.sigstore_calls
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let result = tinfoil_provenance::verify_release(&bundle.sigstore_bundle, &self.router_repo)
            .map_err(|e| {
                tracing::warn!(reason = %e, "Tinfoil router Sigstore attestation rejected");
                TinfoilVerifyError::RouterSigstoreRejected { stage: "verify" }
            });
        if cache.len() >= SIGSTORE_CACHE_MAX {
            cache.clear();
        }
        cache.insert(key, result.clone());
        result
    }
}

/// A verified release attestation vouches for this router only if it names the
/// live SNP measurement and is a statement about the ATC document's digest.
fn bind_router_provenance(
    prov: &Provenance,
    measurement_hex: &str,
    digest: &str,
) -> Result<(), TinfoilVerifyError> {
    if !prov.snp_measurement.eq_ignore_ascii_case(measurement_hex) {
        return Err(TinfoilVerifyError::RouterSigstoreMeasurementMismatch);
    }
    if !prov.subject_sha256.eq_ignore_ascii_case(digest) {
        return Err(TinfoilVerifyError::RouterSigstoreRejected { stage: "digest" });
    }
    Ok(())
}

impl TinfoilVerifier for TinfoilPolicyVerifier {
    fn trust_generation(&self) -> u64 {
        self.trust.load().version
    }

    fn verify_router(&self, bundle: &AtcBundle) -> Result<VerifiedRouter, TinfoilVerifyError> {
        let (report, spki_sha256) = observe_bound_router(bundle)?;
        let measurement_hex = hex::encode(report.measurement);
        // The domain decides where requests go: it must be a bare tinfoil.sh
        // host that the attested certificate itself names.
        inference_providers::attested::tinfoil::verifier_port::validate_router_domain(
            &bundle.domain,
        )?;
        if !pem_cert_has_dns_name(&bundle.enclave_cert, &bundle.domain)? {
            return Err(TinfoilVerifyError::Malformed {
                stage: "domain_not_in_san",
            });
        }
        let tcb = RouterTcb {
            bootloader: report.reported_tcb.bootloader,
            tee: report.reported_tcb.tee,
            snp: report.reported_tcb.snp,
            microcode: report.reported_tcb.microcode,
        };
        if let Some(pin) = self
            .pins
            .router
            .iter()
            .find(|p| p.measurement.eq_ignore_ascii_case(&measurement_hex))
        {
            return Ok(VerifiedRouter {
                spki_sha256,
                measurement_hex,
                tag: pin.tag.clone(),
                source: PinSource::Compiled,
                rekor_log_index: None,
                tcb,
            });
        }
        // Not compiled: admit it on Tinfoil's signed release attestation, if
        // that path is enabled and the ATC served a bundle.
        let bundle_absent = match &bundle.sigstore_bundle {
            serde_json::Value::Null => true,
            serde_json::Value::Object(o) => o.is_empty(),
            _ => false,
        };
        if !self.trust.load().value.router_sigstore || bundle_absent {
            return Err(TinfoilVerifyError::UnknownRouterMeasurement);
        }
        let prov = self.sigstore_provenance(bundle, &measurement_hex)?;
        bind_router_provenance(&prov, &measurement_hex, &bundle.digest)?;
        Ok(VerifiedRouter {
            spki_sha256,
            measurement_hex,
            tag: prov.tag,
            source: PinSource::SigstoreAtc,
            rekor_log_index: Some(prov.rekor_log_index),
            tcb,
        })
    }

    fn check_model(
        &self,
        slug: &str,
        entry: &ProxyModelEntry,
    ) -> Result<PinnedModel, TinfoilVerifyError> {
        // Registers, repo and tag must all equal one pinned row: a pin proves
        // a specific release, so reused registers under other provenance are
        // not accepted.
        let pinned = self.pins.models.get(slug).is_some_and(|ps| {
            ps.iter().any(|p| {
                registers_eq(&p.registers, &entry.measurement.registers)
                    && p.repo == entry.repo
                    && p.tag == entry.tag
            })
        });
        if pinned {
            return Ok(PinnedModel {
                slug: slug.to_string(),
                entry: entry.clone(),
                source: PinSource::Compiled,
                rekor_log_index: None,
            });
        }
        // Not compiled: an admin pin whose Sigstore bundle verified here for
        // exactly this slug, repo, tag and registers.
        let trust = self.trust.load();
        if !trust.value.admin_model_pins {
            return Err(TinfoilVerifyError::UnknownModelMeasurement);
        }
        let admin = self.verified_admin_pins(&trust);
        admin
            .iter()
            .find(|(pin, prov)| {
                pin.slug == slug
                    && pin.repo == entry.repo
                    && pin.tag == entry.tag
                    && prov
                        .registers()
                        .is_some_and(|r| registers_eq(&r, &entry.measurement.registers))
            })
            .map(|(_, prov)| PinnedModel {
                slug: slug.to_string(),
                entry: entry.clone(),
                source: PinSource::Admin,
                rekor_log_index: Some(prov.rekor_log_index),
            })
            .ok_or(TinfoilVerifyError::UnknownModelMeasurement)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use inference_providers::attested::tinfoil::verifier_port::ProxyDoc;

    fn bundle() -> AtcBundle {
        serde_json::from_str(include_str!("testdata/tinfoil/atc_bundle.json")).unwrap()
    }
    fn trust_handle(router_sigstore: bool) -> Arc<ArcSwap<Versioned<TinfoilTrust>>> {
        Arc::new(ArcSwap::from_pointee(Versioned {
            version: 0,
            value: TinfoilTrust {
                router_sigstore,
                ..Default::default()
            },
        }))
    }
    fn test_verifier() -> TinfoilPolicyVerifier {
        TinfoilPolicyVerifier::new(
            serde_json::from_str(include_str!("testdata/tinfoil/test_pins.json")).unwrap(),
            trust_handle(true),
        )
    }
    /// No compiled router row: the router can only be admitted via Sigstore.
    fn unpinned_verifier(router_sigstore: bool) -> TinfoilPolicyVerifier {
        TinfoilPolicyVerifier::new(TinfoilPins::default(), trust_handle(router_sigstore))
    }
    fn hand_built_provenance(snp: &str, digest: &str) -> Provenance {
        Provenance {
            repo: ROUTER_REPO.into(),
            tag: "v1.2.3".into(),
            predicate_type: "p".into(),
            subject_sha256: digest.into(),
            snp_measurement: snp.into(),
            rtmr1: None,
            rtmr2: None,
            rekor_log_index: 7,
            integrated_time: chrono::DateTime::from_timestamp(0, 0).unwrap(),
        }
    }

    /// The compiled pins come only from the sync bot's reviewed PRs, so their
    /// content changes over time; this checks they always parse and that no
    /// row is missing its measurement.
    #[test]
    fn compiled_pins_parse() {
        let p = vetted_tinfoil_pins().unwrap();
        // SEV-SNP measurements and model registers are 48-byte digests (96 hex).
        let hex96 = |s: &str| s.len() == 96 && s.chars().all(|c| c.is_ascii_hexdigit());
        for r in &p.router {
            assert!(
                hex96(&r.measurement) && !r.repo.is_empty() && !r.tag.is_empty(),
                "bad router pin: {r:?}"
            );
        }
        for (slug, pins) in &p.models {
            for m in pins {
                assert!(
                    !m.registers.is_empty()
                        && m.registers.iter().all(|reg| hex96(reg))
                        && !m.repo.is_empty()
                        && !m.tag.is_empty(),
                    "bad model pin for {slug}: {m:?}"
                );
            }
        }
    }

    #[test]
    fn router_verifies_against_test_pins_and_binds_spki() {
        let v = test_verifier().verify_router(&bundle()).unwrap();
        assert_eq!(
            hex::encode(v.spki_sha256),
            "2ac79995464edfb139b34e4ee6269f38d0ab63da92b1a431170dec3bdd0c7c84"
        );
        assert_eq!(v.tag, "v0.0.0-test");
    }

    #[test]
    fn router_tcb_comes_from_the_verified_report() {
        let v = test_verifier().verify_router(&bundle()).unwrap();
        let r = observe_router(&bundle()).unwrap();
        assert_eq!(v.tcb.bootloader, r.reported_tcb.bootloader);
        assert_eq!(v.tcb.tee, r.reported_tcb.tee);
        assert_eq!(v.tcb.snp, r.reported_tcb.snp);
        assert_eq!(v.tcb.microcode, r.reported_tcb.microcode);
        assert!(r.reported_tcb.meets(&MIN_TCB));
    }

    #[test]
    fn domain_must_be_named_by_the_cert_san() {
        let mut b = bundle();
        assert!(pem_cert_has_dns_name(&b.enclave_cert, &b.domain).unwrap());
        // Syntactically valid, but not a name the attested certificate carries.
        b.domain = "router-0.tinfoil.sh".into();
        assert_eq!(
            test_verifier().verify_router(&b).unwrap_err(),
            TinfoilVerifyError::Malformed {
                stage: "domain_not_in_san"
            }
        );
    }

    #[test]
    fn invalid_domains_are_malformed() {
        for bad in [
            "https://inference.tinfoil.sh",
            "inference.tinfoil.sh:443",
            "inference.tinfoil.sh/x",
            "u@inference.tinfoil.sh",
            "inference.example.com",
            "tinfoil.sh",
            ".tinfoil.sh",
            "Inference.tinfoil.sh",
        ] {
            let mut b = bundle();
            b.domain = bad.into();
            assert_eq!(
                test_verifier().verify_router(&b).unwrap_err(),
                TinfoilVerifyError::Malformed {
                    stage: "router_domain"
                },
                "{bad}"
            );
        }
    }

    #[test]
    fn observe_router_needs_no_pins() {
        let r = observe_router(&bundle()).unwrap();
        assert_eq!(
            hex::encode(&r.report_data[..32]),
            "2ac79995464edfb139b34e4ee6269f38d0ab63da92b1a431170dec3bdd0c7c84"
        );
    }

    #[test]
    fn empty_router_pins_fail_closed() {
        assert_eq!(
            TinfoilPolicyVerifier::new(TinfoilPins::default(), trust_handle(false))
                .verify_router(&bundle())
                .unwrap_err(),
            TinfoilVerifyError::UnknownRouterMeasurement
        );
    }

    #[test]
    fn non_snp_router_format_is_unsupported() {
        let mut b = bundle();
        b.report.format = "https://tinfoil.sh/predicate/tdx-guest/v2".into();
        assert_eq!(
            test_verifier().verify_router(&b).unwrap_err(),
            TinfoilVerifyError::UnsupportedRouterPlatform
        );
    }

    #[test]
    fn swapped_cert_is_report_data_mismatch() {
        use base64::Engine;
        let mut b = bundle();
        // The VCEK is a valid cert whose SPKI differs from the router's TLS key.
        let der = base64::engine::general_purpose::STANDARD
            .decode(&b.vcek)
            .unwrap();
        b.enclave_cert = format!(
            "-----BEGIN CERTIFICATE-----\n{}\n-----END CERTIFICATE-----\n",
            base64::engine::general_purpose::STANDARD.encode(der)
        );
        assert_eq!(
            test_verifier().verify_router(&b).unwrap_err(),
            TinfoilVerifyError::ReportDataMismatch
        );
    }

    #[test]
    fn tampered_report_is_bad_signature() {
        use base64::Engine;
        use std::io::{Read, Write};
        let mut b = bundle();
        let gz = base64::engine::general_purpose::STANDARD
            .decode(&b.report.body)
            .unwrap();
        let mut raw = Vec::new();
        flate2::read::GzDecoder::new(&gz[..])
            .read_to_end(&mut raw)
            .unwrap();
        raw[0x90] ^= 1; // flip a measurement byte
        let mut enc = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        enc.write_all(&raw).unwrap();
        b.report.body = base64::engine::general_purpose::STANDARD.encode(enc.finish().unwrap());
        assert_eq!(
            test_verifier().verify_router(&b).unwrap_err(),
            TinfoilVerifyError::BadSignature
        );
    }

    #[test]
    fn model_registers_must_be_pinned() {
        let proxy: ProxyDoc =
            serde_json::from_str(include_str!("testdata/tinfoil/proxy.json")).unwrap();
        assert!(test_verifier()
            .check_model("glm-5-3", &proxy.models["glm-5-3"])
            .is_ok());
        assert_eq!(
            test_verifier()
                .check_model("kimi-k3", &proxy.models["kimi-k3"])
                .unwrap_err(),
            TinfoilVerifyError::UnknownModelMeasurement
        );
    }

    fn with_report_body(b: &mut AtcBundle, body: String) {
        b.report.body = body;
    }

    #[test]
    fn oversized_or_garbage_report_body_is_malformed() {
        use base64::Engine;
        use std::io::Write;
        let b64 = base64::engine::general_purpose::STANDARD;
        let gzip = |raw: &[u8]| {
            let mut enc = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
            enc.write_all(raw).unwrap();
            enc.finish().unwrap()
        };

        // Not base64.
        let mut b = bundle();
        with_report_body(&mut b, "!!! not base64 !!!".into());
        assert_eq!(
            observe_router(&b).unwrap_err(),
            TinfoilVerifyError::Malformed {
                stage: "report_base64"
            }
        );

        // Base64 but not gzip.
        with_report_body(&mut b, b64.encode(b"plain bytes, not gzip"));
        assert_eq!(
            observe_router(&b).unwrap_err(),
            TinfoilVerifyError::Malformed {
                stage: "report_gzip"
            }
        );

        // Valid gzip that inflates past the 64 KiB cap: truncated to the cap,
        // which is not a report length, so it is rejected as malformed.
        with_report_body(&mut b, b64.encode(gzip(&vec![0u8; 1024 * 1024])));
        assert_eq!(
            observe_router(&b).unwrap_err(),
            TinfoilVerifyError::Malformed {
                stage: "snp_report"
            }
        );
    }

    #[test]
    fn malformed_error_reason_is_not_a_fetch_error() {
        assert_eq!(
            TinfoilVerifyError::Malformed { stage: "x" }.reason(),
            "malformed_evidence"
        );
    }

    #[test]
    fn map_snp_maps_every_error() {
        use TinfoilVerifyError as E;
        for (from, to) in [
            (SnpError::Tcb, E::TcbTooLow),
            (SnpError::Debug, E::DebugPolicy),
            (SnpError::MigrateMa, E::DebugPolicy),
            (SnpError::Chain, E::BadSignature),
            (SnpError::Signature, E::BadSignature),
            (
                SnpError::Malformed,
                E::Malformed {
                    stage: "snp_report",
                },
            ),
            (
                SnpError::Product,
                E::Malformed {
                    stage: "snp_product",
                },
            ),
        ] {
            assert_eq!(map_snp(from), to);
        }
    }

    #[test]
    fn non_matching_router_pins_are_unknown_measurement() {
        use super::super::tinfoil_pins::RouterPin;
        let pins = TinfoilPins {
            router: vec![RouterPin {
                measurement: "00".repeat(48),
                repo: "r".into(),
                tag: "t".into(),
            }],
            models: Default::default(),
        };
        assert_eq!(
            TinfoilPolicyVerifier::new(pins, trust_handle(false))
                .verify_router(&bundle())
                .unwrap_err(),
            TinfoilVerifyError::UnknownRouterMeasurement
        );
    }

    #[test]
    fn pinned_slug_with_different_registers_is_unknown_model() {
        use super::super::tinfoil_pins::ModelPin;
        let proxy: ProxyDoc =
            serde_json::from_str(include_str!("testdata/tinfoil/proxy.json")).unwrap();
        let mut pins = TinfoilPins::default();
        pins.models.insert(
            "glm-5-3".into(),
            vec![ModelPin {
                registers: vec!["aa".into(), "bb".into(), "cc".into()],
                repo: "r".into(),
                tag: "t".into(),
            }],
        );
        assert_eq!(
            TinfoilPolicyVerifier::new(pins, trust_handle(true))
                .check_model("glm-5-3", &proxy.models["glm-5-3"])
                .unwrap_err(),
            TinfoilVerifyError::UnknownModelMeasurement
        );
    }

    #[test]
    fn model_registers_compare_case_insensitively() {
        let mut proxy: ProxyDoc =
            serde_json::from_str(include_str!("testdata/tinfoil/proxy.json")).unwrap();
        let entry = proxy.models.get_mut("glm-5-3").unwrap();
        for r in &mut entry.measurement.registers {
            *r = r.to_uppercase();
        }
        assert!(test_verifier().check_model("glm-5-3", entry).is_ok());
    }

    #[test]
    fn pinned_registers_with_other_tag_or_repo_are_unknown_model() {
        let proxy: ProxyDoc =
            serde_json::from_str(include_str!("testdata/tinfoil/proxy.json")).unwrap();
        let pinned = test_verifier()
            .check_model("glm-5-3", &proxy.models["glm-5-3"])
            .unwrap();
        assert_eq!(pinned.entry.repo, "tinfoilsh/confidential-glm5-3-nvfp4");
        assert_eq!(pinned.entry.tag, "v0.0.3");

        let mut other_tag = proxy.models["glm-5-3"].clone();
        other_tag.tag = "v9.9.9".into();
        let mut other_repo = proxy.models["glm-5-3"].clone();
        other_repo.repo = "tinfoilsh/confidential-other".into();
        for entry in [other_tag, other_repo] {
            assert_eq!(
                test_verifier().check_model("glm-5-3", &entry).unwrap_err(),
                TinfoilVerifyError::UnknownModelMeasurement
            );
        }
    }

    // --- Sigstore router admission (Tasks 6) ---

    #[test]
    fn compiled_router_pin_is_admitted_as_compiled() {
        let v = test_verifier().verify_router(&bundle()).unwrap();
        assert_eq!(v.source, PinSource::Compiled);
        assert_eq!(v.rekor_log_index, None);
        assert_eq!(v.tag, "v0.0.0-test");
    }

    #[test]
    fn unpinned_router_with_valid_atc_bundle_is_admitted_via_sigstore() {
        let v = unpinned_verifier(true).verify_router(&bundle()).unwrap();
        assert_eq!(v.source, PinSource::SigstoreAtc);
        assert!(v.rekor_log_index.is_some());
        assert!(!v.tag.is_empty());
        assert_eq!(
            hex::encode(v.spki_sha256),
            "2ac79995464edfb139b34e4ee6269f38d0ab63da92b1a431170dec3bdd0c7c84"
        );
    }

    #[test]
    fn sigstore_bundle_for_other_release_is_rejected() {
        // The signed statement names a different SNP measurement than the live
        // report (the bundle itself verifies).
        let live = "aa".repeat(48);
        let prov = hand_built_provenance(&"bb".repeat(48), "d");
        assert_eq!(
            bind_router_provenance(&prov, &live, "d").unwrap_err(),
            TinfoilVerifyError::RouterSigstoreMeasurementMismatch
        );
        // Case differences are not a mismatch.
        let prov = hand_built_provenance(&live.to_uppercase(), "d");
        assert!(bind_router_provenance(&prov, &live, "d").is_ok());
    }

    #[test]
    fn digest_mismatch_is_rejected() {
        let m = "aa".repeat(48);
        let prov = hand_built_provenance(&m, "d1");
        assert_eq!(
            bind_router_provenance(&prov, &m, "d2").unwrap_err(),
            TinfoilVerifyError::RouterSigstoreRejected { stage: "digest" }
        );
        // Through the verifier: a different ATC digest breaks the binding.
        let mut b = bundle();
        b.digest = "00".repeat(32);
        assert_eq!(
            unpinned_verifier(true).verify_router(&b).unwrap_err(),
            TinfoilVerifyError::RouterSigstoreRejected { stage: "digest" }
        );
    }

    #[test]
    fn other_repo_bundle_is_rejected() {
        // A release signed for another repo fails the signer-identity check.
        let mut v = unpinned_verifier(true);
        v.router_repo = "tinfoilsh/confidential-other".into();
        assert_eq!(
            v.verify_router(&bundle()).unwrap_err(),
            TinfoilVerifyError::RouterSigstoreRejected { stage: "verify" }
        );
    }

    #[test]
    fn missing_bundle_falls_back_to_unknown_router_measurement() {
        for missing in [serde_json::Value::Null, serde_json::json!({})] {
            let mut b = bundle();
            b.sigstore_bundle = missing;
            assert_eq!(
                unpinned_verifier(true).verify_router(&b).unwrap_err(),
                TinfoilVerifyError::UnknownRouterMeasurement
            );
        }
        // A key absent from the ATC JSON deserializes to the same thing.
        let mut json: serde_json::Value =
            serde_json::from_str(include_str!("testdata/tinfoil/atc_bundle.json")).unwrap();
        json.as_object_mut().unwrap().remove("sigstoreBundle");
        let b: AtcBundle = serde_json::from_value(json).unwrap();
        assert!(b.sigstore_bundle.is_null());
    }

    #[test]
    fn router_sigstore_off_rejects_unpinned_router() {
        assert_eq!(
            unpinned_verifier(false)
                .verify_router(&bundle())
                .unwrap_err(),
            TinfoilVerifyError::UnknownRouterMeasurement
        );
        // A compiled pin is unaffected by the switch.
        let v = TinfoilPolicyVerifier::new(
            serde_json::from_str(include_str!("testdata/tinfoil/test_pins.json")).unwrap(),
            trust_handle(false),
        );
        assert_eq!(
            v.verify_router(&bundle()).unwrap().source,
            PinSource::Compiled
        );
    }

    #[test]
    fn switch_is_read_on_every_verify_and_version_is_the_generation() {
        let trust = trust_handle(true);
        let v = TinfoilPolicyVerifier::new(TinfoilPins::default(), trust.clone());
        assert_eq!(v.trust_generation(), 0);
        assert!(v.verify_router(&bundle()).is_ok());
        trust.store(Arc::new(Versioned {
            version: 3,
            value: TinfoilTrust {
                router_sigstore: false,
                ..Default::default()
            },
        }));
        assert_eq!(v.trust_generation(), 3);
        // The cached provenance does not bypass the switch.
        assert_eq!(
            v.verify_router(&bundle()).unwrap_err(),
            TinfoilVerifyError::UnknownRouterMeasurement
        );
    }

    #[test]
    fn sigstore_result_is_cached_per_bundle() {
        use std::sync::atomic::Ordering;
        let v = unpinned_verifier(true);
        v.verify_router(&bundle()).unwrap();
        v.verify_router(&bundle()).unwrap();
        assert_eq!(v.sigstore_calls.load(Ordering::SeqCst), 1);
        // A different bundle body is a different cache entry.
        let mut b = bundle();
        b.sigstore_bundle["extra"] = serde_json::json!(1);
        let _ = v.verify_router(&b);
        assert_eq!(v.sigstore_calls.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn snp_and_san_checks_still_gate_sigstore_path() {
        use std::sync::atomic::Ordering;
        let v = unpinned_verifier(true);
        // Wrong SAN.
        let mut b = bundle();
        b.domain = "router-0.tinfoil.sh".into();
        assert_eq!(
            v.verify_router(&b).unwrap_err(),
            TinfoilVerifyError::Malformed {
                stage: "domain_not_in_san"
            }
        );
        // Non-SNP platform.
        let mut b = bundle();
        b.report.format = "https://tinfoil.sh/predicate/tdx-guest/v2".into();
        assert_eq!(
            v.verify_router(&b).unwrap_err(),
            TinfoilVerifyError::UnsupportedRouterPlatform
        );
        // Swapped certificate (TLS binding).
        let mut b = bundle();
        let der = base64::engine::general_purpose::STANDARD
            .decode(&b.vcek)
            .unwrap();
        b.enclave_cert = format!(
            "-----BEGIN CERTIFICATE-----\n{}\n-----END CERTIFICATE-----\n",
            base64::engine::general_purpose::STANDARD.encode(der)
        );
        assert_eq!(
            v.verify_router(&b).unwrap_err(),
            TinfoilVerifyError::ReportDataMismatch
        );
        // Tampered report.
        let mut b = bundle();
        let gz = base64::engine::general_purpose::STANDARD
            .decode(&b.report.body)
            .unwrap();
        let mut raw = Vec::new();
        flate2::read::GzDecoder::new(&gz[..])
            .read_to_end(&mut raw)
            .unwrap();
        raw[0x90] ^= 1;
        let mut enc = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        std::io::Write::write_all(&mut enc, &raw).unwrap();
        b.report.body = base64::engine::general_purpose::STANDARD.encode(enc.finish().unwrap());
        assert_eq!(
            v.verify_router(&b).unwrap_err(),
            TinfoilVerifyError::BadSignature
        );
        assert_eq!(
            v.sigstore_calls.load(Ordering::SeqCst),
            0,
            "Sigstore is never consulted before the SNP, TLS and SAN checks pass"
        );
    }

    #[test]
    fn pin_source_strings() {
        assert_eq!(PinSource::Compiled.as_str(), "compiled");
        assert_eq!(PinSource::SigstoreAtc.as_str(), "sigstore_atc");
        assert_eq!(PinSource::Admin.as_str(), "admin");
    }

    // --- Admin model pins (Task 10) ---

    use super::super::tinfoil_trust::recorded_model_pin;
    use inference_providers::attested::tinfoil::verifier_port::{
        ProxyMeasurement, ProxyModelEntry,
    };

    const ADMIN_SLUG: &str = "deepseek-v4-1-flash";

    fn trust_with(
        version: u64,
        admin_model_pins: bool,
        pins: Vec<AdminModelPin>,
    ) -> Arc<Versioned<TinfoilTrust>> {
        Arc::new(Versioned {
            version,
            value: TinfoilTrust {
                admin_model_pins,
                model_pins: pins,
                ..Default::default()
            },
        })
    }

    fn admin_verifier(
        admin_model_pins: bool,
        pins: Vec<AdminModelPin>,
    ) -> (TinfoilPolicyVerifier, Arc<ArcSwap<Versioned<TinfoilTrust>>>) {
        let handle = Arc::new(ArcSwap::new(trust_with(1, admin_model_pins, pins)));
        (
            TinfoilPolicyVerifier::new(TinfoilPins::default(), handle.clone()),
            handle,
        )
    }

    /// The proxy entry Tinfoil would publish for the recorded release.
    fn recorded_entry() -> ProxyModelEntry {
        let pin = recorded_model_pin(ADMIN_SLUG);
        let prov = tinfoil_provenance::verify_release(&pin.sigstore_bundle, &pin.repo).unwrap();
        ProxyModelEntry {
            repo: pin.repo,
            tag: pin.tag,
            measurement: ProxyMeasurement {
                kind: "snp-tdx-multiplatform".into(),
                registers: prov.registers().unwrap(),
            },
            enclaves: Default::default(),
        }
    }

    #[test]
    fn admin_pin_matching_proxy_entry_is_admitted() {
        let (v, _) = admin_verifier(true, vec![recorded_model_pin(ADMIN_SLUG)]);
        let pinned = v.check_model(ADMIN_SLUG, &recorded_entry()).unwrap();
        assert_eq!(pinned.source, PinSource::Admin);
        assert!(pinned.rekor_log_index.is_some());
        assert_eq!(pinned.slug, ADMIN_SLUG);
        // Case-insensitive registers; a different slug is not admitted.
        let mut upper = recorded_entry();
        for r in &mut upper.measurement.registers {
            *r = r.to_uppercase();
        }
        assert!(v.check_model(ADMIN_SLUG, &upper).is_ok());
        assert_eq!(
            v.check_model("other-slug", &recorded_entry()).unwrap_err(),
            TinfoilVerifyError::UnknownModelMeasurement
        );
    }

    #[test]
    fn admin_pin_tag_mismatch_rejected() {
        let (v, _) = admin_verifier(true, vec![recorded_model_pin(ADMIN_SLUG)]);
        let mut e = recorded_entry();
        e.tag = "v9.9.9".into();
        assert_eq!(
            v.check_model(ADMIN_SLUG, &e).unwrap_err(),
            TinfoilVerifyError::UnknownModelMeasurement
        );
    }

    #[test]
    fn admin_pin_registers_mismatch_rejected() {
        let (v, _) = admin_verifier(true, vec![recorded_model_pin(ADMIN_SLUG)]);
        let mut e = recorded_entry();
        e.measurement.registers[1] = "00".repeat(48);
        assert_eq!(
            v.check_model(ADMIN_SLUG, &e).unwrap_err(),
            TinfoilVerifyError::UnknownModelMeasurement
        );
        e.measurement.registers.pop();
        assert_eq!(
            v.check_model(ADMIN_SLUG, &e).unwrap_err(),
            TinfoilVerifyError::UnknownModelMeasurement
        );
    }

    #[test]
    fn admin_pin_repo_mismatch_rejected() {
        let (v, _) = admin_verifier(true, vec![recorded_model_pin(ADMIN_SLUG)]);
        let mut e = recorded_entry();
        e.repo = "tinfoilsh/confidential-other".into();
        assert_eq!(
            v.check_model(ADMIN_SLUG, &e).unwrap_err(),
            TinfoilVerifyError::UnknownModelMeasurement
        );
    }

    #[test]
    fn compiled_wins_over_admin() {
        use super::super::tinfoil_pins::ModelPin;
        let e = recorded_entry();
        let mut pins = TinfoilPins::default();
        pins.models.insert(
            ADMIN_SLUG.into(),
            vec![ModelPin {
                registers: e.measurement.registers.clone(),
                repo: e.repo.clone(),
                tag: e.tag.clone(),
            }],
        );
        let handle = Arc::new(ArcSwap::new(trust_with(
            1,
            true,
            vec![recorded_model_pin(ADMIN_SLUG)],
        )));
        let v = TinfoilPolicyVerifier::new(pins, handle);
        let pinned = v.check_model(ADMIN_SLUG, &e).unwrap();
        assert_eq!(pinned.source, PinSource::Compiled);
        assert_eq!(pinned.rekor_log_index, None);
        assert_eq!(
            v.admin_sigstore_calls
                .load(std::sync::atomic::Ordering::SeqCst),
            0,
            "a compiled match never consults the admin pins"
        );
    }

    #[test]
    fn admin_model_pins_off_blocks_admission() {
        let (v, handle) = admin_verifier(false, vec![recorded_model_pin(ADMIN_SLUG)]);
        assert_eq!(
            v.check_model(ADMIN_SLUG, &recorded_entry()).unwrap_err(),
            TinfoilVerifyError::UnknownModelMeasurement
        );
        // Turning it back on (a new version) admits again.
        handle.store(trust_with(2, true, vec![recorded_model_pin(ADMIN_SLUG)]));
        assert_eq!(
            v.check_model(ADMIN_SLUG, &recorded_entry()).unwrap().source,
            PinSource::Admin
        );
    }

    #[test]
    fn directly_edited_row_reverified_and_dropped() {
        let entry = recorded_entry();
        // Bundle verifies, but the stored tag was edited to something else.
        let mut wrong_tag = recorded_model_pin(ADMIN_SLUG);
        wrong_tag.tag = "v9.9.9".into();
        let mut e = entry.clone();
        e.tag = "v9.9.9".into();
        let (v, _) = admin_verifier(true, vec![wrong_tag]);
        assert_eq!(
            v.check_model(ADMIN_SLUG, &e).unwrap_err(),
            TinfoilVerifyError::UnknownModelMeasurement
        );
        // Garbage bundle: rejected, no panic.
        for bundle in [
            serde_json::json!({"garbage": true}),
            serde_json::json!("x"),
            serde_json::Value::Null,
        ] {
            let mut bad = recorded_model_pin(ADMIN_SLUG);
            bad.sigstore_bundle = bundle;
            let (v, _) = admin_verifier(true, vec![bad]);
            assert_eq!(
                v.check_model(ADMIN_SLUG, &entry).unwrap_err(),
                TinfoilVerifyError::UnknownModelMeasurement
            );
        }
        // A non-tinfoilsh repo is dropped even when it names the entry.
        let mut other_repo = recorded_model_pin(ADMIN_SLUG);
        other_repo.repo = "evil/x".into();
        let mut e = entry.clone();
        e.repo = "evil/x".into();
        let (v, _) = admin_verifier(true, vec![other_repo]);
        assert_eq!(
            v.check_model(ADMIN_SLUG, &e).unwrap_err(),
            TinfoilVerifyError::UnknownModelMeasurement
        );
        // A bad pin does not stop a good one beside it.
        let mut bad = recorded_model_pin("bad");
        bad.sigstore_bundle = serde_json::json!({});
        let (v, _) = admin_verifier(true, vec![bad, recorded_model_pin(ADMIN_SLUG)]);
        assert!(v.check_model(ADMIN_SLUG, &entry).is_ok());
    }

    #[test]
    fn reverify_once_per_version() {
        use std::sync::atomic::Ordering;
        let (v, handle) = admin_verifier(true, vec![recorded_model_pin(ADMIN_SLUG)]);
        let e = recorded_entry();
        for _ in 0..3 {
            v.check_model(ADMIN_SLUG, &e).unwrap();
            let _ = v.check_model("other", &e);
        }
        assert_eq!(v.admin_sigstore_calls.load(Ordering::SeqCst), 1);
        // A new version recomputes the set, but the bundle result is cached.
        handle.store(trust_with(2, true, vec![recorded_model_pin(ADMIN_SLUG)]));
        v.check_model(ADMIN_SLUG, &e).unwrap();
        v.check_model(ADMIN_SLUG, &e).unwrap();
        assert_eq!(v.admin_sigstore_calls.load(Ordering::SeqCst), 1);
        // A removed pin takes effect with the next version.
        handle.store(trust_with(3, true, vec![]));
        assert_eq!(
            v.check_model(ADMIN_SLUG, &e).unwrap_err(),
            TinfoilVerifyError::UnknownModelMeasurement
        );
    }
}
