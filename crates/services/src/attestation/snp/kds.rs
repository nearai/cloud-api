//! SNP verification with guest-supplied or AMD KDS endorsement certificates.
use std::{
    borrow::Cow,
    time::{Duration, SystemTime},
};

use anyhow::{bail, ensure, Context, Result};
use moka::future::Cache;
use sev::{firmware::guest::AttestationReport, parser::ByteParser, Generation};
use x509_parser::{
    pem::Pem,
    prelude::{CertificateRevocationList, FromDer},
    time::ASN1Time,
};

use super::{
    embedded_anchors, parse_cert, verify_evidence, Anchor, Product, SnpEvidence, REPORT_LEN,
};

const KDS: &str = "https://kdsintf.amd.com/vcek/v1";

pub(in crate::attestation) struct AmdKds {
    client: reqwest::Client,
    collateral: Cache<String, bytes::Bytes>,
}

impl AmdKds {
    pub fn new() -> Result<Self> {
        Ok(Self {
            client: reqwest::Client::builder()
                .connect_timeout(Duration::from_secs(30))
                .timeout(Duration::from_secs(120))
                .build()?,
            collateral: Cache::builder()
                .max_capacity(1024)
                .time_to_live(Duration::from_secs(3600))
                .build(),
        })
    }

    async fn fetch(&self, url: &str) -> Result<bytes::Bytes> {
        self.collateral
            .try_get_with(url.to_owned(), async {
                self.client
                    .get(url)
                    .send()
                    .await?
                    .error_for_status()?
                    .bytes()
                    .await
            })
            .await
            .map_err(anyhow::Error::from)
    }

    pub async fn verify(&self, bytes: &[u8], cert_chain: &[Vec<u8>]) -> Result<AttestationReport> {
        ensure!(bytes.len() == REPORT_LEN, "invalid SNP report length");
        let report = AttestationReport::from_bytes(bytes)?;
        // A CPU-specific firmware policy requires the signed CPUID introduced in v3.
        ensure!(
            report.version == 3,
            "dstack SNP admission requires a v3 report with CPUID"
        );
        let family = report.cpuid_fam_id.context("missing SNP CPU family")?;
        let model = report.cpuid_mod_id.context("missing SNP CPU model")?;
        let (product, name) = match Generation::identify_cpu(family, model)? {
            Generation::Milan => (Product::Milan, "Milan"),
            Generation::Genoa => (Product::Genoa, "Genoa"),
            Generation::Turin => (Product::Turin, "Turin"),
            _ => bail!("unsupported SNP product"),
        };
        let pinned = embedded_anchors()?
            .iter()
            .find(|a| a.product == product)
            .context("missing AMD product root")?;
        let base = format!("{KDS}/{name}");
        let (ask, vcek) = match cert_chain {
            [] => {
                let chain = self.fetch(&format!("{base}/cert_chain")).await?;
                let ask = Pem::iter_from_buffer(&chain)
                    .next()
                    .context("missing KDS ASK")??
                    .contents;
                let vcek = self.fetch(&vcek_url(&base, product, &report)).await?;
                (Cow::Owned(ask), Cow::Owned(vcek.to_vec()))
            }
            [ask, vcek] => (certificate_der(ask)?, certificate_der(vcek)?),
            [table] => certificate_table(table)?,
            _ => bail!("invalid SNP certificate chain"),
        };
        // The guest or KDS can supply a rotated ASK. Its trust root is always embedded.
        let rotated;
        let anchor = if pinned.ask_der == *ask {
            pinned
        } else {
            rotated = Anchor::new(product, ask.into_owned(), pinned.ark_der.clone())?;
            &rotated
        };
        verify_evidence(
            &SnpEvidence {
                report: bytes,
                vcek_der: &vcek,
            },
            std::slice::from_ref(anchor),
            SystemTime::now(),
            &[product],
        )?;
        let url = format!("{base}/crl");
        let crl = self.fetch(&url).await?;
        if let Err(err) = check_crl(anchor, &crl, ASN1Time::now()) {
            self.collateral.invalidate(&url).await;
            return Err(err);
        }
        Ok(report)
    }
}

fn vcek_url(base: &str, product: Product, report: &AttestationReport) -> String {
    let tcb = report.reported_tcb;
    let chip = if product == Product::Turin {
        &report.chip_id[..8]
    } else {
        &report.chip_id[..]
    };
    let mut url = format!(
        "{base}/{}?blSPL={}&teeSPL={}&snpSPL={}&ucodeSPL={}",
        hex::encode(chip),
        tcb.bootloader,
        tcb.tee,
        tcb.snp,
        tcb.microcode
    );
    if let Some(fmc) = tcb.fmc {
        url.push_str(&format!("&fmcSPL={fmc}"));
    }
    url
}

fn certificate_der(bytes: &[u8]) -> Result<Cow<'_, [u8]>> {
    if bytes.starts_with(b"-----BEGIN CERTIFICATE-----") {
        let (_, pem) = x509_parser::pem::parse_x509_pem(bytes)?;
        Ok(Cow::Owned(pem.contents))
    } else {
        Ok(Cow::Borrowed(bytes))
    }
}

type Certificates<'a> = (Cow<'a, [u8]>, Cow<'a, [u8]>);

fn certificate_table(bytes: &[u8]) -> Result<Certificates<'_>> {
    const ASK: uuid::Uuid = uuid::uuid!("4ab7b379-bbac-4fe4-a02f-05aef327c782");
    const VCEK: uuid::Uuid = uuid::uuid!("63da758d-e664-4564-adc5-f4b93be8accd");
    const VLEK: uuid::Uuid = uuid::uuid!("a8074bc2-a25a-483e-aae6-39c045a0b8a1");
    let mut ask = None;
    let mut vcek = None;
    for entry in bytes.chunks_exact(24) {
        if entry == [0; 24] {
            return Ok((
                ask.context("missing SNP ASK")?,
                vcek.context("missing SNP VCEK")?,
            ));
        }
        let guid = uuid::Uuid::from_slice(&entry[..16])?;
        let offset = u32::from_le_bytes(entry[16..20].try_into()?) as usize;
        let size = u32::from_le_bytes(entry[20..24].try_into()?) as usize;
        ensure!(offset >= 24 && size != 0, "invalid SNP certificate range");
        let data = bytes
            .get(offset..)
            .and_then(|rest| rest.get(..size))
            .context("invalid SNP certificate range")?;
        match guid {
            ASK => ensure!(
                ask.replace(Cow::Borrowed(data)).is_none(),
                "duplicate SNP ASK"
            ),
            VCEK => ensure!(
                vcek.replace(Cow::Borrowed(data)).is_none(),
                "duplicate SNP VCEK"
            ),
            VLEK => bail!("unsupported SNP VLEK"),
            _ => {}
        }
    }
    bail!("unterminated SNP certificate table")
}

/// AMD signs each product's ASK revocation list with its ARK. Check its
/// validity window even on a cache hit.
fn check_crl(anchor: &Anchor, der: &[u8], now: ASN1Time) -> Result<()> {
    let (rest, crl) = CertificateRevocationList::from_der(der)?;
    let ark = parse_cert(&anchor.ark_der)?;
    let ask = parse_cert(&anchor.ask_der)?;
    ensure!(
        rest.is_empty() && crl.issuer() == ark.subject(),
        "invalid AMD CRL issuer or encoding"
    );
    ensure!(
        crl.signature_algorithm == crl.tbs_cert_list.signature,
        "inconsistent CRL signature algorithm"
    );
    super::check_signature_algorithm(&crl.signature_algorithm)?;
    crl.verify_signature(ark.public_key())?;
    ensure!(
        crl.last_update() <= now && crl.next_update().is_some_and(|next| now < next),
        "AMD CRL is not current"
    );
    ensure!(
        !crl.extensions().iter().any(|ext| ext.critical)
            && !crl
                .iter_revoked_certificates()
                .any(|entry| entry.extensions().iter().any(|ext| ext.critical)),
        "unsupported critical AMD CRL extension"
    );
    ensure!(
        !crl.iter_revoked_certificates()
            .any(|entry| entry.serial() == &ask.serial),
        "AMD ASK is revoked"
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn checks_amd_crl_signature_and_validity_window() {
        let crl = include_bytes!("../testdata/amd-milan.crl");
        let roots = embedded_anchors().unwrap();
        let milan = roots.iter().find(|a| a.product == Product::Milan).unwrap();
        let genoa = roots.iter().find(|a| a.product == Product::Genoa).unwrap();
        let current = ASN1Time::from_timestamp(1_794_000_000).unwrap();
        check_crl(milan, crl, current).unwrap();
        assert!(check_crl(milan, crl, ASN1Time::from_timestamp(1_797_000_000).unwrap()).is_err());
        assert!(check_crl(genoa, crl, current).is_err());
        let mut damaged = crl.to_vec();
        *damaged.last_mut().unwrap() ^= 1;
        assert!(check_crl(milan, &damaged, current).is_err());
    }

    #[test]
    fn rejects_an_ask_listed_on_its_arks_crl() {
        use rcgen::{
            date_time_ymd, BasicConstraints, CertificateParams, CertificateRevocationListParams,
            DnType, IsCa, Issuer, KeyIdMethod, KeyPair, KeyUsagePurpose, RevokedCertParams,
            PKCS_ECDSA_P384_SHA384,
        };
        let mut ca = CertificateParams::new(Vec::<String>::new()).unwrap();
        ca.distinguished_name.push(DnType::CommonName, "ARK");
        ca.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
        ca.key_usages = vec![KeyUsagePurpose::KeyCertSign, KeyUsagePurpose::CrlSign];
        let key = KeyPair::generate_for(&PKCS_ECDSA_P384_SHA384).unwrap();
        let ark = ca.self_signed(&key).unwrap();
        let issuer = Issuer::new(ca, key);
        let mut ca = CertificateParams::new(Vec::<String>::new()).unwrap();
        ca.distinguished_name.push(DnType::CommonName, "ASK");
        ca.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
        ca.serial_number = Some(7u64.into());
        let ask = ca
            .signed_by(
                &KeyPair::generate_for(&PKCS_ECDSA_P384_SHA384).unwrap(),
                &issuer,
            )
            .unwrap();
        let anchor = Anchor::new(Product::Milan, ask.der().to_vec(), ark.der().to_vec()).unwrap();
        let crl = CertificateRevocationListParams {
            this_update: date_time_ymd(2026, 1, 1),
            next_update: date_time_ymd(2027, 1, 1),
            crl_number: 1u64.into(),
            issuing_distribution_point: None,
            key_identifier_method: KeyIdMethod::Sha256,
            revoked_certs: vec![RevokedCertParams {
                serial_number: 7u64.into(),
                revocation_time: date_time_ymd(2026, 1, 1),
                reason_code: None,
                invalidity_date: None,
            }],
        }
        .signed_by(&issuer)
        .unwrap();
        let err = check_crl(
            &anchor,
            crl.der(),
            ASN1Time::from_timestamp(1_794_000_000).unwrap(),
        )
        .unwrap_err();
        assert!(err.to_string().contains("revoked"));
    }

    #[test]
    fn extracts_ask_and_vcek_from_a_terminated_kernel_table() {
        let mut table = Vec::new();
        for (guid, offset) in [
            (uuid::uuid!("4ab7b379-bbac-4fe4-a02f-05aef327c782"), 72u32),
            (uuid::uuid!("63da758d-e664-4564-adc5-f4b93be8accd"), 73u32),
        ] {
            table.extend_from_slice(guid.as_bytes());
            table.extend_from_slice(&offset.to_le_bytes());
            table.extend_from_slice(&1u32.to_le_bytes());
        }
        table.extend_from_slice(&[0; 24]);
        table.extend_from_slice(&[7, 8]);
        let (ask, vcek) = certificate_table(&table).unwrap();
        assert_eq!((&*ask, &*vcek), (&[7u8][..], &[8u8][..]));
        table.pop();
        assert!(certificate_table(&table).is_err());
    }
}
