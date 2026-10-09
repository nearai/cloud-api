//! Tinfoil router/model policy: SEV-SNP verification of the router attestation
//! (ATC bundle) against compiled pins, binding the report to the router's TLS key.

use std::io::Read;

use base64::Engine;
use inference_providers::attested::tinfoil::verifier_port::{
    AtcBundle, PinnedModel, ProxyModelEntry, RouterTcb, TinfoilVerifier, TinfoilVerifyError,
    VerifiedRouter,
};
use sha2::{Digest, Sha256};

use super::snp::{self, SnpError, SnpEvidence, SnpPolicy, Tcb, VerifiedSnpReport};
use super::tinfoil_pins::{TinfoilPins, COMPILED_PINS_JSON};

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

pub struct TinfoilPolicyVerifier {
    pins: TinfoilPins,
}

impl TinfoilPolicyVerifier {
    pub fn new(pins: TinfoilPins) -> Self {
        Self { pins }
    }
}

impl TinfoilVerifier for TinfoilPolicyVerifier {
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
        let pin = self
            .pins
            .router
            .iter()
            .find(|p| p.measurement.eq_ignore_ascii_case(&measurement_hex))
            .ok_or(TinfoilVerifyError::UnknownRouterMeasurement)?;
        Ok(VerifiedRouter {
            spki_sha256,
            measurement_hex,
            tag: pin.tag.clone(),
            tcb: RouterTcb {
                bootloader: report.reported_tcb.bootloader,
                tee: report.reported_tcb.tee,
                snp: report.reported_tcb.snp,
                microcode: report.reported_tcb.microcode,
            },
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
        if !pinned {
            return Err(TinfoilVerifyError::UnknownModelMeasurement);
        }
        Ok(PinnedModel {
            slug: slug.to_string(),
            entry: entry.clone(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use inference_providers::attested::tinfoil::verifier_port::ProxyDoc;

    fn bundle() -> AtcBundle {
        serde_json::from_str(include_str!("testdata/tinfoil/atc_bundle.json")).unwrap()
    }
    fn test_verifier() -> TinfoilPolicyVerifier {
        TinfoilPolicyVerifier::new(
            serde_json::from_str(include_str!("testdata/tinfoil/test_pins.json")).unwrap(),
        )
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
            TinfoilPolicyVerifier::new(TinfoilPins::default())
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
            TinfoilPolicyVerifier::new(pins)
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
            TinfoilPolicyVerifier::new(pins)
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
}
