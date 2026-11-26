//! Provider-neutral AMD SEV-SNP attestation report verifier.
//!
//! Verifies an `ATTESTATION_REPORT` (AMD SEV-SNP ABI spec, doc 56860) against
//! its VCEK certificate and the AMD ARK/ASK chain embedded in this binary.
//! The offline entry point accepts the VCEK supplied by its caller.
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
//! The offline entry point accepts Milan and Genoa. The KDS-backed entry
//! point also supports Turin and checks the signing ASK against AMD's CRL.

pub(super) mod kds;

use sev::{firmware::host::TcbVersion, parser::ByteParser, Generation};
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
const OID_FMC: &str = "1.3.6.1.4.1.3704.1.3.9";
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

impl From<TcbVersion> for Tcb {
    fn from(tcb: TcbVersion) -> Self {
        Self {
            bootloader: tcb.bootloader,
            tee: tcb.tee,
            snp: tcb.snp,
            microcode: tcb.microcode,
        }
    }
}

impl Product {
    fn reported_tcb(self, report: &[u8]) -> Result<TcbVersion, SnpError> {
        let bytes = report
            .get(OFF_REPORTED_TCB..OFF_REPORTED_TCB + 8)
            .ok_or(SnpError::Malformed)?;
        let generation = match self {
            Self::Milan => Generation::Milan,
            Self::Genoa => Generation::Genoa,
            Self::Turin => Generation::Turin,
        };
        TcbVersion::from_bytes_with(bytes, generation).map_err(|_| SnpError::Malformed)
    }
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
    check_signature_algorithm(alg)?;
    let key = issuer.map(|i| &i.tbs_certificate.subject_pki);
    cert.verify_signature(key).map_err(|_| SnpError::Chain)
}

fn check_signature_algorithm(
    alg: &x509_parser::x509::AlgorithmIdentifier<'_>,
) -> Result<(), SnpError> {
    use x509_parser::{
        oid_registry::OID_NIST_HASH_SHA384, signature_algorithm::SignatureAlgorithm,
    };
    if alg.algorithm == OID_SIG_ECDSA_WITH_SHA384 && alg.parameters.is_none() {
        return Ok(());
    }
    if alg.algorithm == OID_PKCS1_RSASSAPSS {
        if let Ok(SignatureAlgorithm::RSASSA_PSS(params)) = SignatureAlgorithm::try_from(alg) {
            let mask = params.mask_gen_algorithm().map_err(|_| SnpError::Chain)?;
            if *params.hash_algorithm_oid() == OID_NIST_HASH_SHA384
                && mask.mgf.to_id_string() == "1.2.840.113549.1.1.8"
                && mask.hash == OID_NIST_HASH_SHA384
                && params.salt_length() == 48
                && params.trailer_field() == 1
            {
                return Ok(());
            }
        }
    }
    Err(SnpError::Chain)
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
    let mut values = cert
        .extensions()
        .iter()
        .filter(|e| e.oid.to_id_string() == oid);
    match (values.next(), values.next()) {
        (Some(ext), None) => Ok(ext.value),
        _ => Err(SnpError::Chain),
    }
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

/// AMD VCEKs encode HWID as raw bytes or a DER OCTET STRING. Turin also
/// permits the 8-byte chip identifier used by KDS.
fn check_hwid(
    cert: &X509Certificate<'_>,
    product: Product,
    chip: &[u8; 64],
) -> Result<(), SnpError> {
    let valid_length = |len| len == 64 || (product == Product::Turin && len == 8);
    let encoded = ext_value(cert, OID_HWID)?;
    let id = match encoded {
        raw if valid_length(raw.len()) => raw,
        [0x04, len, id @ ..] if usize::from(*len) == id.len() && valid_length(id.len()) => id,
        _ => return Err(SnpError::Chain),
    };
    if !chip.starts_with(id) || chip == &[0; 64] {
        return Err(SnpError::Chain);
    }
    Ok(())
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
            if !ark.is_ca()
                || !ask.is_ca()
                || ark.subject() != ark.issuer()
                || ask.issuer() != ark.subject()
            {
                return Err(SnpError::Chain);
            }
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

/// Applies the offline verifier's product policy and reported TCB floor.
fn verify_with_roots(
    ev: &SnpEvidence<'_>,
    policy: &SnpPolicy,
    anchors: &[Anchor],
    now: SystemTime,
) -> Result<VerifiedSnpReport, SnpError> {
    let report = verify_evidence(ev, anchors, now, &[Product::Milan, Product::Genoa])?;
    if !report.reported_tcb.meets(&policy.min_tcb) {
        return Err(SnpError::Tcb);
    }
    Ok(report)
}

/// Authenticates the report, including the signing chip and its reported TCB.
/// Callers supply their accepted products and enforce their firmware minimums.
fn verify_evidence(
    ev: &SnpEvidence<'_>,
    anchors: &[Anchor],
    now: SystemTime,
    products: &[Product],
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
    if !products.contains(&product) {
        return Err(SnpError::Product);
    }

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
    let version = product.reported_tcb(r)?;
    let reported_tcb = Tcb::from(version);
    let mut chip_id = [0u8; 64];
    chip_id.copy_from_slice(&r[OFF_CHIP_ID..OFF_CHIP_ID + 64]);
    let cert_tcb = Tcb {
        bootloader: ext_u8(&vcek, OID_BOOTLOADER)?,
        tee: ext_u8(&vcek, OID_TEE)?,
        snp: ext_u8(&vcek, OID_SNP)?,
        microcode: ext_u8(&vcek, OID_UCODE)?,
    };
    check_hwid(&vcek, product, &chip_id)?;
    if let Some(fmc) = version.fmc {
        if ext_u8(&vcek, OID_FMC)? != fmc {
            return Err(SnpError::Chain);
        }
    }
    if cert_tcb != reported_tcb {
        return Err(SnpError::Chain);
    }

    // 5. Guest policy.
    check_policy(le_u64(r, OFF_POLICY)?)?;

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
mod tests;
