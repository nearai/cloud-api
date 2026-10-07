//! Provider-neutral AMD SEV-SNP attestation report verifier.
//!
//! Verifies an `ATTESTATION_REPORT` (AMD SEV-SNP ABI spec, doc 56860) against
//! its VCEK certificate and the AMD ARK/ASK chain embedded in this binary.
//! No network access: the VCEK is supplied by the caller as evidence.
//!
//! AMD root chains (`amd_roots/*_cert_chain.pem`, ASK followed by ARK) were
//! fetched once on 2026-10-07 from
//! `https://kdsintf.amd.com/vcek/v1/{Milan,Genoa,Turin}/cert_chain`. They are
//! parsed and signature-checked once per process.
//!
//! Order of checks: length, parse, VCEK chain (issuer match, signatures,
//! validity periods of VCEK/ASK/ARK), report signature, VCEK<->report binding
//! (chip id and TCB), policy (debug, migration agent), TCB floor.
//!
//! Out of scope: certificate revocation. AMD publishes a CRL per product on
//! the KDS; consulting it needs network access, which this verifier never
//! performs, so a revoked-but-unexpired VCEK is not detected here.
//!
//! Products: Milan and Genoa are supported. Turin chains are embedded so a
//! Turin VCEK is recognised, but it is rejected with [`SnpError::Product`]
//! because its `reported_tcb` encoding has not been validated against a real
//! report; we refuse rather than guess.

use std::sync::LazyLock;
use std::time::{SystemTime, UNIX_EPOCH};

use ring::signature::{UnparsedPublicKey, ECDSA_P384_SHA384_FIXED};
use x509_parser::certificate::X509Certificate;
use x509_parser::oid_registry::{OID_PKCS1_RSASSAPSS, OID_SIG_ECDSA_WITH_SHA384};
use x509_parser::prelude::FromDer;
use x509_parser::time::ASN1Time;

const REPORT_LEN: usize = 0x4A0;
const SIGNED_LEN: usize = 0x2A0;
const OFF_VERSION: usize = 0x000;
const OFF_POLICY: usize = 0x008;
// 0x038 is CURRENT_TCB (unused); the VCEK is bound to REPORTED_TCB at 0x180.
const OFF_REPORTED_TCB: usize = 0x180;
const OFF_REPORT_DATA: usize = 0x050;
const OFF_MEASUREMENT: usize = 0x090;
const OFF_CHIP_ID: usize = 0x1A0;
const OFF_SIG_ALGO: usize = 0x034;
const SIG_ALGO_ECDSA_P384_SHA384: u32 = 1;

const POLICY_MIGRATE_MA_BIT: u64 = 1 << 18;
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Tcb {
    pub bootloader: u8,
    pub tee: u8,
    pub snp: u8,
    pub microcode: u8,
}

impl Tcb {
    /// True when every component is at least the corresponding component of `min`.
    pub fn meets(&self, min: &Tcb) -> bool {
        self.bootloader >= min.bootloader
            && self.tee >= min.tee
            && self.snp >= min.snp
            && self.microcode >= min.microcode
    }
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

#[derive(Debug, Clone, Copy, thiserror::Error, PartialEq, Eq)]
pub enum SnpError {
    #[error("malformed report")]
    Malformed,
    #[error("bad vcek chain")]
    Chain,
    #[error("bad signature")]
    Signature,
    #[error("debug policy")]
    Debug,
    #[error("migration agent allowed")]
    MigrateMa,
    #[error("tcb too low")]
    Tcb,
    #[error("unknown product")]
    Product,
}

/// Rejects reports whose guest policy permits debugging or a migration agent.
fn check_policy(policy: u64) -> Result<(), SnpError> {
    if policy & POLICY_DEBUG_BIT != 0 {
        return Err(SnpError::Debug);
    }
    if policy & POLICY_MIGRATE_MA_BIT != 0 {
        return Err(SnpError::MigrateMa);
    }
    Ok(())
}

fn le_u32(b: &[u8], off: usize) -> Result<u32, SnpError> {
    let s = b.get(off..off + 4).ok_or(SnpError::Malformed)?;
    Ok(u32::from_le_bytes(
        s.try_into().map_err(|_| SnpError::Malformed)?,
    ))
}

fn le_u64(b: &[u8], off: usize) -> Result<u64, SnpError> {
    let s = b.get(off..off + 8).ok_or(SnpError::Malformed)?;
    Ok(u64::from_le_bytes(
        s.try_into().map_err(|_| SnpError::Malformed)?,
    ))
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

/// Turin's reported_tcb / VCEK TCB encoding is not validated here, so refuse it.
fn check_product_supported(product: Product) -> Result<(), SnpError> {
    match product {
        Product::Milan | Product::Genoa => Ok(()),
        Product::Turin => Err(SnpError::Product),
    }
}

/// REPORTED_TCB (0x180) of a full-length report, Milan/Genoa layout.
fn reported_tcb(report: &[u8]) -> Result<Tcb, SnpError> {
    Ok(parse_tcb(le_u64(report, OFF_REPORTED_TCB)?))
}

/// Parses one DER certificate, rejecting trailing bytes.
fn parse_cert(der: &[u8]) -> Result<X509Certificate<'_>, SnpError> {
    let (rest, cert) = X509Certificate::from_der(der).map_err(|_| SnpError::Chain)?;
    if !rest.is_empty() {
        return Err(SnpError::Chain);
    }
    Ok(cert)
}

/// Verifies `cert`'s signature under `issuer`'s key (`None`: self-signed).
/// Only RSASSA-PSS (AMD chains) and ECDSA-SHA384 are accepted; the outer and
/// signed algorithm identifiers must agree.
fn verify_cert_sig(
    cert: &X509Certificate<'_>,
    issuer: Option<&X509Certificate<'_>>,
) -> Result<(), SnpError> {
    let alg = &cert.signature_algorithm;
    if *alg != cert.tbs_certificate.signature {
        return Err(SnpError::Chain);
    }
    if alg.algorithm != OID_PKCS1_RSASSAPSS && alg.algorithm != OID_SIG_ECDSA_WITH_SHA384 {
        return Err(SnpError::Chain);
    }
    let key = issuer.map(|i| &i.tbs_certificate.subject_pki);
    cert.verify_signature(key).map_err(|_| SnpError::Chain)
}

fn check_valid_at(cert: &X509Certificate<'_>, now: ASN1Time) -> Result<(), SnpError> {
    if cert.validity().is_valid_at(now) {
        Ok(())
    } else {
        Err(SnpError::Chain)
    }
}

/// Reads a VCEK extension value (the content of its extnValue OCTET STRING).
fn ext_value<'a>(cert: &'a X509Certificate<'_>, oid: &str) -> Result<&'a [u8], SnpError> {
    cert.extensions()
        .iter()
        .find(|e| e.oid.to_id_string() == oid)
        .map(|e| e.value)
        .ok_or(SnpError::Chain)
}

/// DER INTEGER holding a small non-negative value (TCB component).
fn der_u8(value: &[u8]) -> Result<u8, SnpError> {
    match value {
        [0x02, 0x01, v] if *v < 0x80 => Ok(*v),
        [0x02, 0x02, 0x00, v] => Ok(*v),
        _ => Err(SnpError::Chain),
    }
}

fn ext_u8(cert: &X509Certificate<'_>, oid: &str) -> Result<u8, SnpError> {
    der_u8(ext_value(cert, oid)?)
}

/// The HWID extension's extnValue is the raw 64-byte chip id (no inner DER wrapper).
fn ext_hwid(cert: &X509Certificate<'_>) -> Result<[u8; 64], SnpError> {
    ext_value(cert, OID_HWID)?
        .try_into()
        .map_err(|_| SnpError::Chain)
}

/// A verified ARK/ASK pair for one product. The pair is checked for internal
/// consistency (ARK self-signed, ASK signed by ARK) when constructed.
struct Anchor {
    product: Product,
    ask_der: Vec<u8>,
    ark_der: Vec<u8>,
}

impl Anchor {
    fn new(product: Product, ask_der: Vec<u8>, ark_der: Vec<u8>) -> Result<Self, SnpError> {
        {
            let ark = parse_cert(&ark_der)?;
            let ask = parse_cert(&ask_der)?;
            verify_cert_sig(&ark, None)?;
            verify_cert_sig(&ask, Some(&ark))?;
        }
        Ok(Self {
            product,
            ask_der,
            ark_der,
        })
    }

    /// Builds an anchor from a PEM bundle holding the ASK followed by the ARK.
    fn from_pem_chain(product: Product, pem: &[u8]) -> Result<Self, SnpError> {
        let mut ders = Vec::new();
        for p in x509_parser::pem::Pem::iter_from_buffer(pem) {
            ders.push(p.map_err(|_| SnpError::Chain)?.contents);
        }
        let [ask, ark]: [Vec<u8>; 2] = ders.try_into().map_err(|_| SnpError::Chain)?;
        Self::new(product, ask, ark)
    }
}

/// The AMD chains embedded in this binary, verified once per process.
static EMBEDDED_ANCHORS: LazyLock<Result<Vec<Anchor>, SnpError>> = LazyLock::new(|| {
    [
        (Product::Milan, MILAN_CHAIN),
        (Product::Genoa, GENOA_CHAIN),
        (Product::Turin, TURIN_CHAIN),
    ]
    .into_iter()
    .map(|(p, pem)| Anchor::from_pem_chain(p, pem))
    .collect()
});

fn embedded_anchors() -> Result<&'static [Anchor], SnpError> {
    EMBEDDED_ANCHORS
        .as_ref()
        .map(|v| v.as_slice())
        .map_err(|e| *e)
}

/// Verifies the VCEK against `anchors` (selecting the ASK by issuer name before
/// any signature work) and returns the product whose ASK signed it.
fn verify_vcek_chain(
    vcek: &X509Certificate<'_>,
    anchors: &[Anchor],
    now: ASN1Time,
) -> Result<Product, SnpError> {
    for anchor in anchors {
        let ask = parse_cert(&anchor.ask_der)?;
        if vcek.tbs_certificate.issuer.as_raw() != ask.tbs_certificate.subject.as_raw() {
            continue;
        }
        if verify_cert_sig(vcek, Some(&ask)).is_err() {
            continue;
        }
        let ark = parse_cert(&anchor.ark_der)?;
        check_valid_at(vcek, now)?;
        check_valid_at(&ask, now)?;
        check_valid_at(&ark, now)?;
        return Ok(anchor.product);
    }
    Err(SnpError::Chain)
}

/// Verifies `ev.report` end to end against the embedded AMD roots at the
/// current time. See the module docs for the order of checks.
pub fn verify_snp_report(
    ev: &SnpEvidence<'_>,
    policy: &SnpPolicy,
) -> Result<VerifiedSnpReport, SnpError> {
    verify_with_roots(ev, policy, embedded_anchors()?, SystemTime::now())
}

/// The verifier core. Production callers reach it only through
/// [`verify_snp_report`], which supplies the embedded AMD roots; tests inject a
/// synthetic chain and a fixed clock.
fn verify_with_roots(
    ev: &SnpEvidence<'_>,
    policy: &SnpPolicy,
    anchors: &[Anchor],
    now: SystemTime,
) -> Result<VerifiedSnpReport, SnpError> {
    // 1. Length and header.
    let r = ev.report;
    if r.len() != REPORT_LEN {
        return Err(SnpError::Malformed);
    }
    if !(2..=5).contains(&le_u32(r, OFF_VERSION)?) {
        return Err(SnpError::Malformed);
    }
    if le_u32(r, OFF_SIG_ALGO)? != SIG_ALGO_ECDSA_P384_SHA384 {
        return Err(SnpError::Malformed);
    }

    // 2. Parse the VCEK.
    let vcek = parse_cert(ev.vcek_der)?;
    let secs = now
        .duration_since(UNIX_EPOCH)
        .map_err(|_| SnpError::Chain)?
        .as_secs();
    let now = i64::try_from(secs)
        .ok()
        .and_then(|s| ASN1Time::from_timestamp(s).ok())
        .ok_or(SnpError::Chain)?;

    // 3. VCEK -> ASK -> ARK (trust anchors) and validity periods.
    let product = verify_vcek_chain(&vcek, anchors, now)?;
    check_product_supported(product)?;

    // 4. Report signature: ECDSA P-384 / SHA-384 over [0, 0x2A0), R and S little-endian.
    let point = vcek.tbs_certificate.subject_pki.subject_public_key.as_ref();
    if point.len() != 97 || point[0] != 0x04 {
        return Err(SnpError::Chain);
    }
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
    UnparsedPublicKey::new(&ECDSA_P384_SHA384_FIXED, point)
        .verify(&r[..SIGNED_LEN], &rs)
        .map_err(|_| SnpError::Signature)?;

    // The report is now authentic. Bind it to the VCEK that signed it: the
    // certificate is issued for one chip at one TCB.
    let reported_tcb = reported_tcb(r)?;
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
    check_policy(le_u64(r, OFF_POLICY)?)?;

    // 6. TCB floor: every component must meet the minimum.
    if !reported_tcb.meets(&policy.min_tcb) {
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
    fn verify_fixture(
        ev: &SnpEvidence<'_>,
        policy: &SnpPolicy,
    ) -> Result<VerifiedSnpReport, SnpError> {
        verify_with_roots(ev, policy, embedded_anchors()?, fixed_now())
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
            verify_fixture(
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
            verify_fixture(
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
            verify_fixture(
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
            verify_fixture(
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
        assert_eq!(
            verify_fixture(
                &SnpEvidence {
                    report: &r,
                    vcek_der: &v
                },
                &lax()
            )
            .unwrap_err(),
            SnpError::Chain
        );
    }
    #[test]
    fn rejects_truncated_report() {
        let (r, v) = fixture();
        assert_eq!(
            verify_fixture(
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
            verify_fixture(
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
            verify_fixture(
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
    fn check_policy_rejects_debug_bit() {
        assert_eq!(check_policy(1 << 19).unwrap_err(), SnpError::Debug);
        assert!(check_policy(0).is_ok());
    }
    #[test]
    fn migrate_ma_bit_is_rejected() {
        assert_eq!(check_policy(1 << 18).unwrap_err(), SnpError::MigrateMa);
        assert_eq!(
            check_policy((1 << 18) | (1 << 19)).unwrap_err(),
            SnpError::Debug
        );
        // The captured router policy (0x30000) has neither bit set.
        assert!(check_policy(0x30000).is_ok());
    }
    #[test]
    fn reported_tcb_is_read_from_0x180_not_0x38() {
        let mut r = vec![0u8; REPORT_LEN];
        r[0x38..0x40].copy_from_slice(&[1, 2, 0, 0, 0, 0, 3, 4]);
        r[0x180..0x188].copy_from_slice(&[10, 20, 0, 0, 0, 0, 30, 40]);
        assert_eq!(
            reported_tcb(&r).unwrap(),
            Tcb {
                bootloader: 10,
                tee: 20,
                snp: 30,
                microcode: 40
            }
        );
    }
    #[test]
    fn turin_product_is_rejected() {
        assert_eq!(
            check_product_supported(Product::Turin).unwrap_err(),
            SnpError::Product
        );
        assert!(check_product_supported(Product::Milan).is_ok());
        assert!(check_product_supported(Product::Genoa).is_ok());
    }
    #[test]
    fn rejects_cert_not_chaining_to_any_embedded_ask() {
        // A Milan ASK is signed by the Milan ARK, not by any ASK: it cannot be a VCEK.
        let (r, _) = fixture();
        let der = embedded_anchors()
            .unwrap()
            .iter()
            .find(|a| a.product == Product::Milan)
            .unwrap()
            .ask_der
            .clone();
        assert_eq!(
            verify_fixture(
                &SnpEvidence {
                    report: &r,
                    vcek_der: &der
                },
                &lax()
            )
            .unwrap_err(),
            SnpError::Chain
        );
    }
    #[test]
    fn embedded_chains_are_self_consistent() {
        let products: Vec<Product> = embedded_anchors()
            .unwrap()
            .iter()
            .map(|a| a.product)
            .collect();
        assert_eq!(products, [Product::Milan, Product::Genoa, Product::Turin]);
    }

    // ---- synthetic chain: exercises everything behind the signature check ----

    use rcgen::{
        BasicConstraints, CertificateParams, CustomExtension, DistinguishedName, DnType, IsCa,
        Issuer, KeyPair, KeyUsagePurpose, PKCS_ECDSA_P384_SHA384,
    };
    use ring::rand::SystemRandom;
    use ring::signature::{EcdsaKeyPair, ECDSA_P384_SHA384_FIXED_SIGNING};

    /// 2026-10-07T00:00:00Z.
    fn fixed_now() -> SystemTime {
        UNIX_EPOCH + std::time::Duration::from_secs(1_791_331_200)
    }

    fn at_year(year: u64) -> SystemTime {
        UNIX_EPOCH + std::time::Duration::from_secs((year - 1970) * 365 * 86_400)
    }

    fn p384_key() -> KeyPair {
        KeyPair::generate_for(&PKCS_ECDSA_P384_SHA384).unwrap()
    }

    fn ca_params(cn: &str) -> CertificateParams {
        let mut p = CertificateParams::new(Vec::<String>::new()).unwrap();
        let mut dn = DistinguishedName::new();
        dn.push(DnType::CommonName, cn);
        p.distinguished_name = dn;
        p.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
        p.key_usages = vec![KeyUsagePurpose::KeyCertSign];
        p.not_before = rcgen::date_time_ymd(2020, 1, 1);
        p.not_after = rcgen::date_time_ymd(2050, 1, 1);
        p
    }

    struct SynthChain {
        anchors: Vec<Anchor>,
        ask_issuer: Issuer<'static, KeyPair>,
    }

    /// A synthetic ARK -> ASK pair (ECDSA P-384) labelled as `product`.
    fn synth_chain(product: Product, ask_not_after: (i32, u8, u8)) -> SynthChain {
        let ark_key = p384_key();
        let ark_params = ca_params("SYNTH-ARK");
        let ark = ark_params.self_signed(&ark_key).unwrap();
        let ark_issuer = Issuer::new(ark_params, ark_key);
        let ask_key = p384_key();
        let mut ask_params = ca_params("SYNTH-ASK");
        ask_params.not_after =
            rcgen::date_time_ymd(ask_not_after.0, ask_not_after.1, ask_not_after.2);
        let ask = ask_params.signed_by(&ask_key, &ark_issuer).unwrap();
        let anchor = Anchor::new(product, ask.der().to_vec(), ark.der().to_vec()).unwrap();
        SynthChain {
            anchors: vec![anchor],
            ask_issuer: Issuer::new(ask_params, ask_key),
        }
    }

    #[derive(Clone)]
    struct ReportSpec {
        policy: u64,
        tcb: Tcb,
        chip_id: [u8; 64],
    }

    impl Default for ReportSpec {
        fn default() -> Self {
            Self {
                policy: 0x30000,
                tcb: Tcb {
                    bootloader: 10,
                    tee: 0,
                    snp: 23,
                    microcode: 200,
                },
                chip_id: [7; 64],
            }
        }
    }

    #[derive(Clone)]
    struct VcekSpec {
        tcb: Tcb,
        hwid: [u8; 64],
        not_after: (i32, u8, u8),
    }

    impl VcekSpec {
        fn matching(r: &ReportSpec) -> Self {
            Self {
                tcb: r.tcb,
                hwid: r.chip_id,
                not_after: (2040, 1, 1),
            }
        }
    }

    fn int_ext(v: u8) -> Vec<u8> {
        if v < 0x80 {
            vec![0x02, 0x01, v]
        } else {
            vec![0x02, 0x02, 0x00, v]
        }
    }

    fn oid_arcs(s: &str) -> Vec<u64> {
        s.split('.').map(|x| x.parse().unwrap()).collect()
    }

    /// Signs a report with a fresh synthetic VCEK issued under `chain`.
    fn synth_evidence(chain: &SynthChain, rs: &ReportSpec, vs: &VcekSpec) -> (Vec<u8>, Vec<u8>) {
        let vcek_key = p384_key();
        let mut params = CertificateParams::new(Vec::<String>::new()).unwrap();
        let mut dn = DistinguishedName::new();
        dn.push(DnType::CommonName, "SYNTH-VCEK");
        params.distinguished_name = dn;
        params.not_before = rcgen::date_time_ymd(2025, 1, 1);
        params.not_after = rcgen::date_time_ymd(vs.not_after.0, vs.not_after.1, vs.not_after.2);
        for (oid, v) in [
            (OID_BOOTLOADER, vs.tcb.bootloader),
            (OID_TEE, vs.tcb.tee),
            (OID_SNP, vs.tcb.snp),
            (OID_UCODE, vs.tcb.microcode),
        ] {
            params
                .custom_extensions
                .push(CustomExtension::from_oid_content(
                    &oid_arcs(oid),
                    int_ext(v),
                ));
        }
        params
            .custom_extensions
            .push(CustomExtension::from_oid_content(
                &oid_arcs(OID_HWID),
                vs.hwid.to_vec(),
            ));
        let vcek = params.signed_by(&vcek_key, &chain.ask_issuer).unwrap();

        let mut r = vec![0u8; REPORT_LEN];
        r[OFF_VERSION..OFF_VERSION + 4].copy_from_slice(&2u32.to_le_bytes());
        r[OFF_SIG_ALGO..OFF_SIG_ALGO + 4].copy_from_slice(&1u32.to_le_bytes());
        r[OFF_POLICY..OFF_POLICY + 8].copy_from_slice(&rs.policy.to_le_bytes());
        r[OFF_MEASUREMENT..OFF_MEASUREMENT + 48].copy_from_slice(&[0x42; 48]);
        r[OFF_REPORT_DATA..OFF_REPORT_DATA + 64].copy_from_slice(&[0x24; 64]);
        r[OFF_REPORTED_TCB..OFF_REPORTED_TCB + 8].copy_from_slice(&[
            rs.tcb.bootloader,
            rs.tcb.tee,
            0,
            0,
            0,
            0,
            rs.tcb.snp,
            rs.tcb.microcode,
        ]);
        r[OFF_CHIP_ID..OFF_CHIP_ID + 64].copy_from_slice(&rs.chip_id);
        let rng = SystemRandom::new();
        let signer = EcdsaKeyPair::from_pkcs8(
            &ECDSA_P384_SHA384_FIXED_SIGNING,
            &vcek_key.serialize_der(),
            &rng,
        )
        .unwrap();
        let sig = signer.sign(&rng, &r[..SIGNED_LEN]).unwrap();
        let sig = sig.as_ref();
        assert_eq!(sig.len(), 96);
        for i in 0..48 {
            r[SIGNED_LEN + i] = sig[47 - i];
            r[SIGNED_LEN + 72 + i] = sig[48 + 47 - i];
        }
        (r, vcek.der().to_vec())
    }

    fn run(
        chain: &SynthChain,
        report: &[u8],
        vcek: &[u8],
        min: Tcb,
        now: SystemTime,
    ) -> Result<VerifiedSnpReport, SnpError> {
        verify_with_roots(
            &SnpEvidence {
                report,
                vcek_der: vcek,
            },
            &SnpPolicy { min_tcb: min },
            &chain.anchors,
            now,
        )
    }

    fn zero_tcb() -> Tcb {
        Tcb {
            bootloader: 0,
            tee: 0,
            snp: 0,
            microcode: 0,
        }
    }

    fn happy(product: Product) -> (SynthChain, Vec<u8>, Vec<u8>) {
        let chain = synth_chain(product, (2040, 1, 1));
        let rs = ReportSpec::default();
        let (r, v) = synth_evidence(&chain, &rs, &VcekSpec::matching(&rs));
        (chain, r, v)
    }

    #[test]
    fn synthetic_milan_chain_verifies() {
        let (chain, r, v) = happy(Product::Milan);
        let ok = run(&chain, &r, &v, zero_tcb(), fixed_now()).unwrap();
        assert_eq!(ok.measurement, [0x42; 48]);
        assert_eq!(ok.chip_id, [7; 64]);
        assert_eq!(ok.reported_tcb.microcode, 200);
    }

    #[test]
    fn synthetic_genoa_chain_verifies() {
        let (chain, r, v) = happy(Product::Genoa);
        assert!(run(&chain, &r, &v, zero_tcb(), fixed_now()).is_ok());
    }

    #[test]
    fn synthetic_turin_chain_is_product_error() {
        let (chain, r, v) = happy(Product::Turin);
        assert_eq!(
            run(&chain, &r, &v, zero_tcb(), fixed_now()).unwrap_err(),
            SnpError::Product
        );
    }

    #[test]
    fn synthetic_chain_is_not_accepted_by_the_public_entry() {
        let (_, r, v) = happy(Product::Milan);
        assert_eq!(
            verify_snp_report(
                &SnpEvidence {
                    report: &r,
                    vcek_der: &v
                },
                &lax()
            )
            .unwrap_err(),
            SnpError::Chain
        );
    }

    #[test]
    fn synthetic_debug_policy_bit_is_rejected_after_valid_signature() {
        let chain = synth_chain(Product::Milan, (2040, 1, 1));
        let rs = ReportSpec {
            policy: 0x30000 | POLICY_DEBUG_BIT,
            ..Default::default()
        };
        let (r, v) = synth_evidence(&chain, &rs, &VcekSpec::matching(&rs));
        assert_eq!(
            run(&chain, &r, &v, zero_tcb(), fixed_now()).unwrap_err(),
            SnpError::Debug
        );
    }

    #[test]
    fn synthetic_migrate_ma_policy_bit_is_rejected() {
        let chain = synth_chain(Product::Milan, (2040, 1, 1));
        let rs = ReportSpec {
            policy: 0x30000 | POLICY_MIGRATE_MA_BIT,
            ..Default::default()
        };
        let (r, v) = synth_evidence(&chain, &rs, &VcekSpec::matching(&rs));
        assert_eq!(
            run(&chain, &r, &v, zero_tcb(), fixed_now()).unwrap_err(),
            SnpError::MigrateMa
        );
    }

    #[test]
    fn synthetic_chip_id_not_matching_vcek_hwid_is_chain_error() {
        let chain = synth_chain(Product::Milan, (2040, 1, 1));
        let rs = ReportSpec::default();
        let vs = VcekSpec {
            hwid: [9; 64],
            ..VcekSpec::matching(&rs)
        };
        let (r, v) = synth_evidence(&chain, &rs, &vs);
        assert_eq!(
            run(&chain, &r, &v, zero_tcb(), fixed_now()).unwrap_err(),
            SnpError::Chain
        );
    }

    #[test]
    fn synthetic_reported_tcb_not_matching_vcek_tcb_is_chain_error() {
        let chain = synth_chain(Product::Milan, (2040, 1, 1));
        let rs = ReportSpec::default();
        let mut vs = VcekSpec::matching(&rs);
        vs.tcb.snp += 1;
        let (r, v) = synth_evidence(&chain, &rs, &vs);
        assert_eq!(
            run(&chain, &r, &v, zero_tcb(), fixed_now()).unwrap_err(),
            SnpError::Chain
        );
    }

    #[test]
    fn synthetic_tcb_floor_is_enforced() {
        let (chain, r, v) = happy(Product::Milan);
        let min = Tcb {
            microcode: 201,
            ..zero_tcb()
        };
        assert_eq!(
            run(&chain, &r, &v, min, fixed_now()).unwrap_err(),
            SnpError::Tcb
        );
    }

    #[test]
    fn synthetic_tampered_report_is_signature_error() {
        let (chain, mut r, v) = happy(Product::Milan);
        r[OFF_MEASUREMENT] ^= 1;
        assert_eq!(
            run(&chain, &r, &v, zero_tcb(), fixed_now()).unwrap_err(),
            SnpError::Signature
        );
    }

    #[test]
    fn vcek_outside_validity_period_is_rejected() {
        let (chain, r, v) = happy(Product::Milan);
        // VCEK is valid 2025-01-01..2040-01-01.
        assert_eq!(
            run(&chain, &r, &v, zero_tcb(), at_year(2021)).unwrap_err(),
            SnpError::Chain
        );
        assert_eq!(
            run(&chain, &r, &v, zero_tcb(), at_year(2045)).unwrap_err(),
            SnpError::Chain
        );
        assert!(run(&chain, &r, &v, zero_tcb(), at_year(2030)).is_ok());
    }

    #[test]
    fn expired_ask_is_rejected() {
        let chain = synth_chain(Product::Milan, (2028, 1, 1));
        let rs = ReportSpec::default();
        let (r, v) = synth_evidence(&chain, &rs, &VcekSpec::matching(&rs));
        assert!(run(&chain, &r, &v, zero_tcb(), at_year(2027)).is_ok());
        assert_eq!(
            run(&chain, &r, &v, zero_tcb(), at_year(2030)).unwrap_err(),
            SnpError::Chain
        );
    }

    #[test]
    fn vcek_with_trailing_bytes_is_rejected() {
        let (chain, r, mut v) = happy(Product::Milan);
        v.push(0);
        assert_eq!(
            run(&chain, &r, &v, zero_tcb(), fixed_now()).unwrap_err(),
            SnpError::Chain
        );
    }

    #[test]
    fn der_u8_decodes_tcb_components() {
        assert_eq!(der_u8(&[0x02, 0x01, 0x7f]), Ok(0x7f));
        assert_eq!(der_u8(&[0x02, 0x02, 0x00, 0x80]), Ok(0x80));
        assert_eq!(der_u8(&[0x02, 0x02, 0x00, 0xff]), Ok(0xff));
        // 0x80 without the sign-padding byte would be negative.
        assert_eq!(der_u8(&[0x02, 0x01, 0x80]), Err(SnpError::Chain));
        assert_eq!(
            der_u8(&[0x02, 0x03, 0x00, 0x01, 0x02]),
            Err(SnpError::Chain)
        );
        assert_eq!(der_u8(&[0x02, 0x02, 0x00]), Err(SnpError::Chain));
        assert_eq!(der_u8(&[0x04, 0x01, 0x01]), Err(SnpError::Chain));
        assert_eq!(der_u8(&[]), Err(SnpError::Chain));
    }

    #[test]
    fn short_buffers_are_malformed_not_panics() {
        assert_eq!(le_u32(&[0; 2], 0), Err(SnpError::Malformed));
        assert_eq!(le_u64(&[0; 8], 1), Err(SnpError::Malformed));
        assert_eq!(reported_tcb(&[0; 16]), Err(SnpError::Malformed));
    }

    #[test]
    fn tcb_meets_is_component_wise() {
        let min = Tcb {
            bootloader: 10,
            tee: 1,
            snp: 23,
            microcode: 84,
        };
        let newer_boot_older_tee = Tcb {
            bootloader: 11,
            tee: 0,
            snp: 23,
            microcode: 84,
        };
        assert!(!newer_boot_older_tee.meets(&min));
        assert!(min.meets(&min));
        assert!(Tcb {
            bootloader: 255,
            tee: 255,
            snp: 255,
            microcode: 255
        }
        .meets(&min));
    }
}
