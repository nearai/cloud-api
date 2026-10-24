use rustls::client::danger::{ServerCertVerified, ServerCertVerifier};
use rustls::client::WebPkiServerVerifier;
use rustls::pki_types::{pem::PemObject, CertificateDer, ServerName, UnixTime};
use rustls::RootCertStore;
use std::sync::Arc;
use std::time::Duration;

const COMPROMISED_CHAINS: &[(&str, &[u8])] = &[
    (
        "dv-g3b-aaa-emudhra.we-have-a-problem.com",
        include_bytes!("fixtures/emudhra/chains/dv-g3b-aaa-emudhra.we-have-a-problem.com.pem"),
    ),
    (
        "dv-g3b-emudhra.we-have-a-problem.com",
        include_bytes!("fixtures/emudhra/chains/dv-g3b-emudhra.we-have-a-problem.com.pem"),
    ),
    (
        "dv-g3c-aaa-emudhra.we-have-a-problem.com",
        include_bytes!("fixtures/emudhra/chains/dv-g3c-aaa-emudhra.we-have-a-problem.com.pem"),
    ),
    (
        "dv-g3c-emudhra.we-have-a-problem.com",
        include_bytes!("fixtures/emudhra/chains/dv-g3c-emudhra.we-have-a-problem.com.pem"),
    ),
    (
        "ev-g3b-aaa-emudhra.we-have-a-problem.com",
        include_bytes!("fixtures/emudhra/chains/ev-g3b-aaa-emudhra.we-have-a-problem.com.pem"),
    ),
    (
        "ev-g3b-emudhra.we-have-a-problem.com",
        include_bytes!("fixtures/emudhra/chains/ev-g3b-emudhra.we-have-a-problem.com.pem"),
    ),
    (
        "ev-g3c-aaa-emudhra.we-have-a-problem.com",
        include_bytes!("fixtures/emudhra/chains/ev-g3c-aaa-emudhra.we-have-a-problem.com.pem"),
    ),
    (
        "ev-g3c-emudhra.we-have-a-problem.com",
        include_bytes!("fixtures/emudhra/chains/ev-g3c-emudhra.we-have-a-problem.com.pem"),
    ),
    (
        "ov-g3b-aaa-emudhra.we-have-a-problem.com",
        include_bytes!("fixtures/emudhra/chains/ov-g3b-aaa-emudhra.we-have-a-problem.com.pem"),
    ),
    (
        "ov-g3b-emudhra.we-have-a-problem.com",
        include_bytes!("fixtures/emudhra/chains/ov-g3b-emudhra.we-have-a-problem.com.pem"),
    ),
    (
        "ov-g3c-aaa-emudhra.we-have-a-problem.com",
        include_bytes!("fixtures/emudhra/chains/ov-g3c-aaa-emudhra.we-have-a-problem.com.pem"),
    ),
    (
        "ov-g3c-emudhra.we-have-a-problem.com",
        include_bytes!("fixtures/emudhra/chains/ov-g3c-emudhra.we-have-a-problem.com.pem"),
    ),
];

const EMSIGN_ROOT: &[u8] = include_bytes!("fixtures/emudhra/roots/emSign_ECC_Root_CA_-_G3.pem");
const AAA_ROOT: &[u8] = include_bytes!("fixtures/emudhra/roots/Comodo_AAA_Services_root.pem");
const GTS_ROOT: &[u8] = include_bytes!("fixtures/emudhra/roots/GTS_Root_R4.pem");
const CONTROL_CHAIN: &[u8] = include_bytes!("fixtures/emudhra/control-api.openai.com.pem");

fn certificates(pem: &'static [u8]) -> Vec<CertificateDer<'static>> {
    CertificateDer::pem_slice_iter(pem)
        .collect::<Result<Vec<_>, _>>()
        .expect("valid PEM certificate fixture")
}

fn roots_from_pem(pems: &[&'static [u8]]) -> RootCertStore {
    let mut roots = RootCertStore::empty();
    for pem in pems {
        for cert in certificates(pem) {
            roots.add(cert).expect("valid root certificate fixture");
        }
    }
    roots
}

fn webpki_roots() -> RootCertStore {
    let mut roots = RootCertStore::empty();
    roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
    roots
}

fn verifier(roots: RootCertStore) -> Arc<WebPkiServerVerifier> {
    WebPkiServerVerifier::builder_with_provider(
        Arc::new(roots),
        Arc::new(rustls::crypto::aws_lc_rs::default_provider()),
    )
    .build()
    .expect("build WebPKI verifier")
}

fn verify(
    verifier: &WebPkiServerVerifier,
    chain: &[CertificateDer<'static>],
    host: &str,
) -> Result<ServerCertVerified, rustls::Error> {
    verifier.verify_server_cert(
        &chain[0],
        &chain[1..],
        &ServerName::try_from(host).expect("valid fixture hostname"),
        &[],
        UnixTime::since_unix_epoch(Duration::from_secs(1792497600)),
    )
}

fn assert_compromised_chains_rejected(verifier: &WebPkiServerVerifier) {
    assert_eq!(
        COMPROMISED_CHAINS.len(),
        12,
        "expected all 12 chain fixtures"
    );
    let verified: Vec<_> = COMPROMISED_CHAINS
        .iter()
        .filter_map(|(host, pem)| {
            let chain = certificates(pem);
            assert!(chain.len() >= 2, "{host}: missing intermediate");
            verify(verifier, &chain, host).is_ok().then_some(*host)
        })
        .collect();
    assert!(
        verified.is_empty(),
        "compromised chains unexpectedly verified: {}",
        verified.join(", ")
    );
}

#[test]
fn compromised_intermediates_rejected_even_when_roots_trusted() {
    // Guards against rustls-webpki < 0.103.16 with native roots from an unpatched image.
    let verifier = verifier(roots_from_pem(&[EMSIGN_ROOT, AAA_ROOT]));
    assert_compromised_chains_rejected(&verifier);
}

#[test]
fn compromised_intermediates_rejected_by_webpki_roots() {
    let verifier = verifier(webpki_roots());
    assert_compromised_chains_rejected(&verifier);
}

#[test]
fn control_chain_verifies() {
    let chain = certificates(CONTROL_CHAIN);
    assert!(chain.len() >= 2, "control chain missing intermediate");
    verify(
        &verifier(roots_from_pem(&[GTS_ROOT])),
        &chain,
        "api.openai.com",
    )
    .expect("control chain must verify with GTS Root R4");
    verify(&verifier(webpki_roots()), &chain, "api.openai.com")
        .expect("control chain must verify with webpki-roots");
}
