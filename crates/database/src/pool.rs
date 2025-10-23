use deadpool_postgres::{Config, Pool, Runtime};
use std::fs::File;
use std::io::BufReader;
use tracing::{debug, info};

/// NOTE: Direct pool creation is deprecated. Use ClusterManager with Patroni discovery instead.
/// This module now only provides utility functions for TLS pool creation used by ClusterManager.
///
/// Create pool using rustls with either custom certificate or platform verifier
pub fn create_pool_with_rustls(cfg: Config, cert_path: Option<&str>) -> anyhow::Result<Pool> {
    use tokio_postgres_rustls::MakeRustlsConnect;

    // Install the default crypto provider (ring) if not already installed
    let _ = rustls::crypto::ring::default_provider().install_default();

    let client_config = if let Some(cert_path) = cert_path {
        // Load custom certificate from file
        debug!(
            "Using rustls with custom CA certificate from: {}",
            cert_path
        );
        debug!("Loading CA certificate from: {}", cert_path);

        let cert_file = File::open(cert_path)
            .map_err(|e| anyhow::anyhow!("Failed to open certificate file {cert_path}: {e}"))?;
        let mut reader = BufReader::new(cert_file);

        // Parse the PEM certificates
        let certs = rustls_pemfile::certs(&mut reader)
            .collect::<Result<Vec<_>, _>>()
            .map_err(|e| anyhow::anyhow!("Failed to parse certificate: {e}"))?;

        if certs.is_empty() {
            return Err(anyhow::anyhow!("No certificates found in {cert_path}"));
        }

        debug!("Found {} certificate(s) in {}", certs.len(), cert_path);

        // Create root certificate store and add custom certificates
        let mut root_store = rustls::RootCertStore::empty();
        for cert in certs {
            root_store
                .add(cert)
                .map_err(|e| anyhow::anyhow!("Failed to add certificate to root store: {e}"))?;
        }

        debug!(
            "Successfully loaded custom CA certificate from {}",
            cert_path
        );

        // Build TLS configuration with custom certificates
        rustls::ClientConfig::builder()
            .with_root_certificates(root_store)
            .with_no_client_auth()
    } else {
        // Use platform verifier for system certificates
        // This uses OS-native verification (Security.framework on macOS, etc.)
        // and includes revocation checking via OCSP/CRLs
        info!("Using rustls with platform verifier (OS certificate store)");

        use rustls_platform_verifier::ConfigVerifierExt;
        rustls::ClientConfig::with_platform_verifier()
            .map_err(|e| anyhow::anyhow!("Failed to create platform verifier: {e}"))?
    };

    let tls = MakeRustlsConnect::new(client_config);

    cfg.create_pool(Some(Runtime::Tokio1), tls)
        .map_err(|e| anyhow::anyhow!("Failed to create TLS pool: {e}"))
}

/// Create pool using native-tls (simpler for accepting self-signed certificates)
pub fn create_pool_with_native_tls(
    cfg: Config,
    accept_invalid_certs: bool,
) -> anyhow::Result<Pool> {
    use native_tls::TlsConnector;
    use postgres_native_tls::MakeTlsConnector;

    let mut builder = TlsConnector::builder();
    if accept_invalid_certs {
        info!("Configuring TLS to accept self-signed certificates");
        builder.danger_accept_invalid_certs(true);
    }

    let connector = builder
        .build()
        .map_err(|e| anyhow::anyhow!("Failed to create TLS connector: {e}"))?;
    let tls = MakeTlsConnector::new(connector);

    cfg.create_pool(Some(Runtime::Tokio1), tls)
        .map_err(|e| anyhow::anyhow!("Failed to create TLS pool: {e}"))
}

/// Connection pool type alias
pub type DbPool = Pool;
