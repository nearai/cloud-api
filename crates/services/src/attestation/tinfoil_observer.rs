//! Keyless observation of the Tinfoil router for the daily measurement sync
//! (`crates/tinfoil_sync`). It runs the same SEV-SNP checks as
//! [`super::tinfoil::TinfoilPolicyVerifier::verify_router`] but consults no pins.

use inference_providers::attested::tinfoil::verifier_port::{AtcBundle, TinfoilVerifyError};

use super::snp::Tcb;
use super::tinfoil::{observe_router, spki_sha256_of_pem_cert};

/// What a genuine router attestation says about the running router.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ObservedRouter {
    pub measurement_hex: String,
    pub spki_sha256_hex: String,
    pub tcb: Tcb,
    pub format: String,
}

/// Verify the router bundle (VCEK chain, report signature, TCB floor, debug
/// bit, and that the report is bound to the router's TLS key) and describe it.
pub fn observe(bundle: &AtcBundle) -> Result<ObservedRouter, TinfoilVerifyError> {
    let report = observe_router(bundle)?;
    let spki = spki_sha256_of_pem_cert(&bundle.enclave_cert)?;
    if report.report_data[..32] != spki {
        return Err(TinfoilVerifyError::ReportDataMismatch);
    }
    Ok(ObservedRouter {
        measurement_hex: hex::encode(report.measurement),
        spki_sha256_hex: hex::encode(spki),
        tcb: report.reported_tcb,
        format: bundle.report.format.clone(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bundle() -> AtcBundle {
        serde_json::from_str(include_str!("testdata/tinfoil/atc_bundle.json")).unwrap()
    }

    #[test]
    fn observes_without_pins() {
        let o = observe(&bundle()).unwrap();
        assert_eq!(o.measurement_hex.len(), 96);
        assert_eq!(
            o.spki_sha256_hex,
            "2ac79995464edfb139b34e4ee6269f38d0ab63da92b1a431170dec3bdd0c7c84"
        );
        assert!(o.format.ends_with("sev-snp-guest/v2"));
        assert!(
            o.tcb
                >= Tcb {
                    bootloader: 10,
                    tee: 0,
                    snp: 23,
                    microcode: 84
                }
        );
    }

    #[test]
    fn swapped_cert_is_report_data_mismatch() {
        use base64::Engine;
        let mut b = bundle();
        let der = base64::engine::general_purpose::STANDARD
            .decode(&b.vcek)
            .unwrap();
        b.enclave_cert = format!(
            "-----BEGIN CERTIFICATE-----\n{}\n-----END CERTIFICATE-----\n",
            base64::engine::general_purpose::STANDARD.encode(der)
        );
        assert_eq!(
            observe(&b).unwrap_err(),
            TinfoilVerifyError::ReportDataMismatch
        );
    }

    #[test]
    fn non_snp_format_is_rejected() {
        let mut b = bundle();
        b.report.format = "https://tinfoil.sh/predicate/tdx-guest/v2".into();
        assert_eq!(
            observe(&b).unwrap_err(),
            TinfoilVerifyError::UnsupportedRouterPlatform
        );
    }
}
