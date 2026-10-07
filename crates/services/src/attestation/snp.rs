//! Provider-neutral AMD SEV-SNP attestation report verifier.
//!
//! Verifies an `ATTESTATION_REPORT` (AMD SEV-SNP ABI spec, doc 56860) against
//! its VCEK certificate and the AMD ARK/ASK chain embedded in this binary.
//! No network access: the VCEK is supplied by the caller as evidence.
//!
//! AMD root chains (`amd_roots/*_cert_chain.pem`, ASK followed by ARK) were
//! fetched once on 2026-10-07 from
//! `https://kdsintf.amd.com/vcek/v1/{Milan,Genoa,Turin}/cert_chain`.
//!
//! Order of checks: length, parse, VCEK chain, report signature, VCEK<->report
//! binding (chip id and TCB), policy (debug), TCB floor.
//!
//! Products: Milan and Genoa are supported. Turin chains are embedded so a
//! Turin VCEK is recognised, but it is rejected with [`SnpError::Product`]
//! because its `reported_tcb` encoding has not been validated against a real
//! report; we refuse rather than guess.

use p384::ecdsa::{signature::Verifier, Signature, VerifyingKey};
use rsa::pkcs1::DecodeRsaPublicKey;
use rsa::pss::{Signature as PssSignature, VerifyingKey as PssVerifyingKey};
use rsa::RsaPublicKey;
use sha2_010::Sha384;
use x509_cert::der::{Decode, Encode};
use x509_cert::Certificate;

const REPORT_LEN: usize = 0x4A0;
const SIGNED_LEN: usize = 0x2A0;
const OFF_VERSION: usize = 0x000;
const OFF_POLICY: usize = 0x008;
const OFF_TCB: usize = 0x038;
const OFF_REPORT_DATA: usize = 0x050;
const OFF_MEASUREMENT: usize = 0x090;
const OFF_CHIP_ID: usize = 0x1A0;
const OFF_SIG_ALGO: usize = 0x034;
const SIG_ALGO_ECDSA_P384_SHA384: u32 = 1;

const POLICY_DEBUG_BIT: u64 = 1 << 19;

// VCEK X.509 extension OIDs (AMD KDS interface spec 57230).
const OID_BOOTLOADER: &str = "1.3.6.1.4.1.3704.1.3.1";
const OID_TEE: &str = "1.3.6.1.4.1.3704.1.3.2";
const OID_SNP: &str = "1.3.6.1.4.1.3704.1.3.3";
const OID_HWID: &str = "1.3.6.1.4.1.3704.1.4";
const OID_UCODE: &str = "1.3.6.1.4.1.3704.1.3.8";

const MILAN_CHAIN: &[u8] = include_bytes!("amd_roots/milan_cert_chain.pem");
const GENOA_CHAIN: &[u8] = include_bytes!("amd_roots/genoa_cert_chain.pem");
const TURIN_CHAIN: &[u8] = include_bytes!("amd_roots/turin_cert_chain.pem");

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Product {
    Milan,
    Genoa,
    Turin,
}

pub struct SnpEvidence<'a> {
    pub report: &'a [u8],
    pub vcek_der: &'a [u8],
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct Tcb {
    pub bootloader: u8,
    pub tee: u8,
    pub snp: u8,
    pub microcode: u8,
}

pub struct SnpPolicy {
    pub min_tcb: Tcb,
}

#[derive(Debug, Clone)]
pub struct VerifiedSnpReport {
    pub measurement: [u8; 48],
    pub report_data: [u8; 64],
    pub reported_tcb: Tcb,
    pub chip_id: [u8; 64],
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum SnpError {
    #[error("malformed report")]
    Malformed,
    #[error("bad vcek chain")]
    Chain,
    #[error("bad signature")]
    Signature,
    #[error("debug policy")]
    Debug,
    #[error("tcb too low")]
    Tcb,
    #[error("unknown product")]
    Product,
}

/// Rejects reports whose guest policy permits debugging.
fn check_policy(policy: u64) -> Result<(), SnpError> {
    if policy & POLICY_DEBUG_BIT != 0 {
        return Err(SnpError::Debug);
    }
    Ok(())
}

fn le_u32(b: &[u8], off: usize) -> u32 {
    u32::from_le_bytes(b[off..off + 4].try_into().expect("bounds checked"))
}

fn le_u64(b: &[u8], off: usize) -> u64 {
    u64::from_le_bytes(b[off..off + 8].try_into().expect("bounds checked"))
}

/// Milan/Genoa `TCB_VERSION` layout: byte 0 boot loader, 1 TEE, 6 SNP, 7 microcode.
fn parse_tcb(raw: u64) -> Tcb {
    let b = raw.to_le_bytes();
    Tcb {
        bootloader: b[0],
        tee: b[1],
        snp: b[6],
        microcode: b[7],
    }
}

/// Parses the ASK and ARK (in that order) of an embedded chain and checks the
/// chain is internally consistent: ARK self-signed, ASK signed by ARK.
fn load_chain(product: Product) -> Result<(Certificate, Certificate), SnpError> {
    let pem = match product {
        Product::Milan => MILAN_CHAIN,
        Product::Genoa => GENOA_CHAIN,
        Product::Turin => TURIN_CHAIN,
    };
    let certs = Certificate::load_pem_chain(pem).map_err(|_| SnpError::Chain)?;
    let [ask, ark]: [Certificate; 2] = certs.try_into().map_err(|_| SnpError::Chain)?;
    verify_cert_sig(&ark, &ark)?;
    verify_cert_sig(&ask, &ark)?;
    Ok((ask, ark))
}

/// Verifies `cert`'s RSASSA-PSS (SHA-384, MGF1-SHA-384) signature under `issuer`'s key.
fn verify_cert_sig(cert: &Certificate, issuer: &Certificate) -> Result<(), SnpError> {
    let spki = &issuer.tbs_certificate.subject_public_key_info;
    let key_bytes = spki.subject_public_key.as_bytes().ok_or(SnpError::Chain)?;
    let pubkey = RsaPublicKey::from_pkcs1_der(key_bytes).map_err(|_| SnpError::Chain)?;
    let tbs = cert.tbs_certificate.to_der().map_err(|_| SnpError::Chain)?;
    let sig_bytes = cert.signature.as_bytes().ok_or(SnpError::Chain)?;
    let sig = PssSignature::try_from(sig_bytes).map_err(|_| SnpError::Chain)?;
    PssVerifyingKey::<Sha384>::new(pubkey)
        .verify(&tbs, &sig)
        .map_err(|_| SnpError::Chain)
}

/// Reads a VCEK extension value (the content of its extnValue OCTET STRING).
fn ext_value<'a>(cert: &'a Certificate, oid: &str) -> Result<&'a [u8], SnpError> {
    let exts = cert
        .tbs_certificate
        .extensions
        .as_ref()
        .ok_or(SnpError::Chain)?;
    exts.iter()
        .find(|e| e.extn_id.to_string() == oid)
        .map(|e| e.extn_value.as_bytes())
        .ok_or(SnpError::Chain)
}

/// DER INTEGER holding a small non-negative value (TCB component).
fn ext_u8(cert: &Certificate, oid: &str) -> Result<u8, SnpError> {
    match ext_value(cert, oid)? {
        [0x02, 0x01, v] if *v < 0x80 => Ok(*v),
        [0x02, 0x02, 0x00, v] => Ok(*v),
        _ => Err(SnpError::Chain),
    }
}

/// The HWID extension's extnValue is the raw 64-byte chip id (no inner DER wrapper).
fn ext_hwid(cert: &Certificate) -> Result<[u8; 64], SnpError> {
    ext_value(cert, OID_HWID)?
        .try_into()
        .map_err(|_| SnpError::Chain)
}

/// Verifies the VCEK against the embedded chains and returns the product whose ASK signed it.
fn verify_vcek_chain(vcek: &Certificate) -> Result<Product, SnpError> {
    for product in [Product::Milan, Product::Genoa, Product::Turin] {
        let (ask, _ark) = load_chain(product)?;
        if vcek.tbs_certificate.issuer == ask.tbs_certificate.subject
            && verify_cert_sig(vcek, &ask).is_ok()
        {
            return Ok(product);
        }
    }
    Err(SnpError::Chain)
}

/// Verifies `ev.report` end to end. See the module docs for the order of checks.
pub fn verify_snp_report(
    ev: &SnpEvidence<'_>,
    policy: &SnpPolicy,
) -> Result<VerifiedSnpReport, SnpError> {
    // 1. Length and header.
    let r = ev.report;
    if r.len() != REPORT_LEN {
        return Err(SnpError::Malformed);
    }
    if !(2..=5).contains(&le_u32(r, OFF_VERSION)) {
        return Err(SnpError::Malformed);
    }
    if le_u32(r, OFF_SIG_ALGO) != SIG_ALGO_ECDSA_P384_SHA384 {
        return Err(SnpError::Malformed);
    }

    // 2. Parse the VCEK.
    let vcek = Certificate::from_der(ev.vcek_der).map_err(|_| SnpError::Chain)?;

    // 3. VCEK -> ASK -> ARK (embedded trust anchors).
    let product = verify_vcek_chain(&vcek)?;
    if product == Product::Turin {
        // Turin's reported_tcb / VCEK TCB encoding is not validated here.
        return Err(SnpError::Product);
    }

    // 4. Report signature: ECDSA P-384 / SHA-384 over [0, 0x2A0), R and S little-endian.
    let spki = &vcek.tbs_certificate.subject_public_key_info;
    let point = spki.subject_public_key.as_bytes().ok_or(SnpError::Chain)?;
    let key = VerifyingKey::from_sec1_bytes(point).map_err(|_| SnpError::Chain)?;
    let sig = &r[SIGNED_LEN..SIGNED_LEN + 144];
    // Each 72-byte component is zero-padded LE; only the low 48 bytes may be non-zero.
    if sig[48..72].iter().any(|b| *b != 0) || sig[72 + 48..144].iter().any(|b| *b != 0) {
        return Err(SnpError::Signature);
    }
    let mut rs = [0u8; 96];
    for i in 0..48 {
        rs[i] = sig[47 - i];
        rs[48 + i] = sig[72 + 47 - i];
    }
    let signature = Signature::from_slice(&rs).map_err(|_| SnpError::Signature)?;
    key.verify(&r[..SIGNED_LEN], &signature)
        .map_err(|_| SnpError::Signature)?;

    // The report is now authentic. Bind it to the VCEK that signed it: the
    // certificate is issued for one chip at one TCB.
    let reported_tcb = parse_tcb(le_u64(r, OFF_TCB));
    let mut chip_id = [0u8; 64];
    chip_id.copy_from_slice(&r[OFF_CHIP_ID..OFF_CHIP_ID + 64]);
    let cert_tcb = Tcb {
        bootloader: ext_u8(&vcek, OID_BOOTLOADER)?,
        tee: ext_u8(&vcek, OID_TEE)?,
        snp: ext_u8(&vcek, OID_SNP)?,
        microcode: ext_u8(&vcek, OID_UCODE)?,
    };
    if ext_hwid(&vcek)? != chip_id || cert_tcb != reported_tcb {
        return Err(SnpError::Chain);
    }

    // 5. Guest policy.
    check_policy(le_u64(r, OFF_POLICY))?;

    // 6. TCB floor: every component must meet the minimum.
    let min = policy.min_tcb;
    if reported_tcb.bootloader < min.bootloader
        || reported_tcb.tee < min.tee
        || reported_tcb.snp < min.snp
        || reported_tcb.microcode < min.microcode
    {
        return Err(SnpError::Tcb);
    }

    let mut measurement = [0u8; 48];
    measurement.copy_from_slice(&r[OFF_MEASUREMENT..OFF_MEASUREMENT + 48]);
    let mut report_data = [0u8; 64];
    report_data.copy_from_slice(&r[OFF_REPORT_DATA..OFF_REPORT_DATA + 64]);
    Ok(VerifiedSnpReport {
        measurement,
        report_data,
        reported_tcb,
        chip_id,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture() -> (Vec<u8>, Vec<u8>) {
        (
            include_bytes!("testdata/tinfoil/router_report.bin").to_vec(),
            include_bytes!("testdata/tinfoil/router_vcek.der").to_vec(),
        )
    }
    fn lax() -> SnpPolicy {
        SnpPolicy {
            min_tcb: Tcb {
                bootloader: 0,
                tee: 0,
                snp: 0,
                microcode: 0,
            },
        }
    }

    #[test]
    fn verifies_captured_router_report() {
        let (r, v) = fixture();
        let ok = verify_snp_report(
            &SnpEvidence {
                report: &r,
                vcek_der: &v,
            },
            &lax(),
        )
        .unwrap();
        assert_eq!(hex::encode(ok.measurement), "b3be62c7199d8e4d24f130e5651bdc8a62a2532f72c7e87c986bec54bf5f90bab703ad4dbfc5e45bfd385f8972dfc66c");
        assert_eq!(
            hex::encode(&ok.report_data[..32]),
            "2ac79995464edfb139b34e4ee6269f38d0ab63da92b1a431170dec3bdd0c7c84"
        );
        assert_eq!(
            ok.reported_tcb,
            Tcb {
                bootloader: 10,
                tee: 0,
                snp: 23,
                microcode: 84
            }
        );
    }
    #[test]
    fn rejects_flipped_signature_byte() {
        let (mut r, v) = fixture();
        r[0x2A0] ^= 1;
        assert_eq!(
            verify_snp_report(
                &SnpEvidence {
                    report: &r,
                    vcek_der: &v
                },
                &lax()
            )
            .unwrap_err(),
            SnpError::Signature
        );
    }
    #[test]
    fn rejects_flipped_measurement_byte() {
        let (mut r, v) = fixture();
        r[0x90] ^= 1;
        assert_eq!(
            verify_snp_report(
                &SnpEvidence {
                    report: &r,
                    vcek_der: &v
                },
                &lax()
            )
            .unwrap_err(),
            SnpError::Signature
        );
    }
    #[test]
    fn rejects_tcb_below_floor() {
        let (r, v) = fixture();
        let p = SnpPolicy {
            min_tcb: Tcb {
                bootloader: 255,
                tee: 255,
                snp: 255,
                microcode: 255,
            },
        };
        assert_eq!(
            verify_snp_report(
                &SnpEvidence {
                    report: &r,
                    vcek_der: &v
                },
                &p
            )
            .unwrap_err(),
            SnpError::Tcb
        );
    }
    #[test]
    fn rejects_tcb_below_floor_in_single_component() {
        let (r, v) = fixture();
        let p = SnpPolicy {
            min_tcb: Tcb {
                bootloader: 0,
                tee: 0,
                snp: 0,
                microcode: 85,
            },
        };
        assert_eq!(
            verify_snp_report(
                &SnpEvidence {
                    report: &r,
                    vcek_der: &v
                },
                &p
            )
            .unwrap_err(),
            SnpError::Tcb
        );
    }
    #[test]
    fn rejects_broken_vcek() {
        let (r, mut v) = fixture();
        let n = v.len();
        v[n - 10] ^= 1;
        assert!(matches!(
            verify_snp_report(
                &SnpEvidence {
                    report: &r,
                    vcek_der: &v
                },
                &lax()
            )
            .unwrap_err(),
            SnpError::Chain | SnpError::Signature
        ));
    }
    #[test]
    fn rejects_truncated_report() {
        let (r, v) = fixture();
        assert_eq!(
            verify_snp_report(
                &SnpEvidence {
                    report: &r[..100],
                    vcek_der: &v
                },
                &lax()
            )
            .unwrap_err(),
            SnpError::Malformed
        );
    }
    #[test]
    fn rejects_garbage_vcek() {
        let (r, _) = fixture();
        assert_eq!(
            verify_snp_report(
                &SnpEvidence {
                    report: &r,
                    vcek_der: &[1, 2, 3]
                },
                &lax()
            )
            .unwrap_err(),
            SnpError::Chain
        );
    }
    #[test]
    fn rejects_unsupported_report_version() {
        let (mut r, v) = fixture();
        r[0] = 1;
        assert_eq!(
            verify_snp_report(
                &SnpEvidence {
                    report: &r,
                    vcek_der: &v
                },
                &lax()
            )
            .unwrap_err(),
            SnpError::Malformed
        );
    }
    #[test]
    fn debug_bit_is_checked_before_signature_is_trusted() {
        assert_eq!(check_policy(1 << 19).unwrap_err(), SnpError::Debug);
        assert!(check_policy(0).is_ok());
    }
    #[test]
    fn embedded_chains_are_self_consistent() {
        for p in [Product::Milan, Product::Genoa, Product::Turin] {
            assert!(load_chain(p).is_ok(), "{p:?}");
        }
    }
}
