use deadpool_postgres::{Config, Pool, Runtime};
use rustls::pki_types::{pem::PemObject, CertificateDer};
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

        // Parse the PEM certificates using rustls-pki-types (replacing deprecated rustls-pemfile)
        let certs: Vec<CertificateDer<'static>> = CertificateDer::pem_reader_iter(&mut reader)
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

/// Shared, swappable handle to the active write pool.
///
/// Repositories hold clones of this handle and acquire connections through it
/// per call. When the Patroni leader changes, `ClusterManager` installs the new
/// leader's pool via [`DbPool::replace`] and every clone immediately routes new
/// acquisitions to the new leader. A plain `deadpool::Pool` clone cannot do
/// this — its target host is fixed at creation, so pools captured at startup
/// keep dialing the old leader after a failover (the 2026-07-12 outage: the
/// leader change was detected and a new pool was built, but no repository ever
/// saw it).
#[derive(Clone)]
pub struct DbPool {
    inner: std::sync::Arc<std::sync::RwLock<Option<Pool>>>,
}

impl DbPool {
    /// Create a handle wrapping an already-built pool.
    pub fn new(pool: Pool) -> Self {
        Self {
            inner: std::sync::Arc::new(std::sync::RwLock::new(Some(pool))),
        }
    }

    /// Create a handle with no pool installed yet. [`DbPool::get`] fails with
    /// `PoolError::Closed` until [`DbPool::replace`] installs one.
    pub fn uninitialized() -> Self {
        Self {
            inner: std::sync::Arc::new(std::sync::RwLock::new(None)),
        }
    }

    /// Install `pool` as the active pool for this handle and every clone of it.
    /// The previous pool (if any) is dropped once its checked-out connections
    /// are returned.
    pub fn replace(&self, pool: Pool) {
        let mut guard = self.inner.write().unwrap_or_else(|e| e.into_inner());
        *guard = Some(pool);
    }

    /// The currently installed pool, if any.
    pub fn current(&self) -> Option<Pool> {
        self.inner.read().unwrap_or_else(|e| e.into_inner()).clone()
    }

    /// Acquire a connection from the currently installed pool.
    pub async fn get(&self) -> Result<deadpool_postgres::Object, deadpool_postgres::PoolError> {
        // Clone the pool out of the lock so it is not held across the await.
        let pool = self.current().ok_or(deadpool_postgres::PoolError::Closed)?;
        pool.get().await
    }

    /// Status of the currently installed pool, or `None` before initialization.
    pub fn status(&self) -> Option<deadpool::Status> {
        self.current().map(|pool| pool.status())
    }
}

impl From<Pool> for DbPool {
    fn from(pool: Pool) -> Self {
        Self::new(pool)
    }
}

// Manual impl: the inner pool's Debug output includes connection config, which
// must never end up in logs.
impl std::fmt::Debug for DbPool {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DbPool")
            .field("initialized", &self.current().is_some())
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use deadpool::managed::QueueMode;

    fn lazy_pool(max_size: usize) -> Pool {
        // deadpool creates connections lazily, so building a pool against an
        // unreachable host is fine as long as no connection is acquired.
        let mut cfg = Config::new();
        cfg.host = Some("pool-test-host.invalid".to_string());
        cfg.port = Some(5432);
        cfg.dbname = Some("test".to_string());
        cfg.user = Some("test".to_string());
        cfg.password = Some("test".to_string());
        cfg.pool = Some(deadpool_postgres::PoolConfig {
            max_size,
            timeouts: deadpool_postgres::Timeouts::default(),
            queue_mode: QueueMode::Fifo,
        });
        cfg.create_pool(Some(Runtime::Tokio1), tokio_postgres::NoTls)
            .expect("lazy pool must build without connecting")
    }

    #[tokio::test]
    async fn replace_propagates_to_existing_clones() {
        let handle = DbPool::new(lazy_pool(3));
        // Simulates a repository holding a clone taken at startup.
        let repository_clone = handle.clone();
        assert_eq!(repository_clone.status().unwrap().max_size, 3);

        handle.replace(lazy_pool(7));
        assert_eq!(
            repository_clone.status().unwrap().max_size,
            7,
            "clones taken before replace() must observe the new pool"
        );
    }

    #[tokio::test]
    async fn get_on_uninitialized_handle_fails_closed() {
        let handle = DbPool::uninitialized();
        assert!(handle.status().is_none());
        match handle.get().await {
            Err(deadpool_postgres::PoolError::Closed) => {}
            other => panic!("expected PoolError::Closed, got {other:?}"),
        }
    }
}
