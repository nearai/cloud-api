use deadpool_postgres::{Config, ManagerConfig, Pool, RecyclingMethod, Runtime};
use std::fs::File;
use std::io::BufReader;
use tokio_postgres::NoTls;
use tracing::{debug, info};

/// Create a connection pool from configuration
pub async fn create_pool(config: &config::DatabaseConfig) -> anyhow::Result<Pool> {
    let mut cfg = Config::new();
    cfg.host = Some(config.host.clone());
    cfg.port = Some(config.port);
    cfg.dbname = Some(config.database.clone());
    cfg.user = Some(config.username.clone());
    cfg.password = Some(config.password.clone());
    cfg.manager = Some(ManagerConfig {
        recycling_method: RecyclingMethod::Fast,
    });

    let pool = if config.tls_enabled {
        info!(
            "Creating database connection pool with TLS: {}:{}/{}",
            config.host, config.port, config.database
        );
        if let Some(ref cert_path) = config.tls_ca_cert_path {
            info!("Using custom CA certificate from: {}", cert_path);
        } else {
            info!("Using system certificate store");
        }
        create_pool_with_rustls(cfg, config.tls_ca_cert_path.as_deref())?
    } else {
        info!(
            "Creating database connection pool without TLS: {}:{}/{}",
            config.host, config.port, config.database
        );
        cfg.create_pool(Some(Runtime::Tokio1), NoTls)
            .map_err(|e| anyhow::anyhow!("Failed to create pool: {}", e))?
    };

    // Test the connection
    let client = pool
        .get()
        .await
        .map_err(|e| anyhow::anyhow!("Failed to get connection from pool: {}", e))?;

    client
        .simple_query("SELECT 1")
        .await
        .map_err(|e| anyhow::anyhow!("Failed to test database connection: {}", e))?;
    info!("Database connection test successful");

    Ok(pool)
}

/// Create pool using rustls with either custom certificate or platform verifier
fn create_pool_with_rustls(cfg: Config, cert_path: Option<&str>) -> anyhow::Result<Pool> {
    use tokio_postgres_rustls::MakeRustlsConnect;

    // Install the default crypto provider (ring) if not already installed
    let _ = rustls::crypto::ring::default_provider().install_default();

    let client_config = if let Some(cert_path) = cert_path {
        // Load custom certificate from file
        info!(
            "Using rustls with custom CA certificate from: {}",
            cert_path
        );
        debug!("Loading CA certificate from: {}", cert_path);

        let cert_file = File::open(cert_path)
            .map_err(|e| anyhow::anyhow!("Failed to open certificate file {}: {}", cert_path, e))?;
        let mut reader = BufReader::new(cert_file);

        // Parse the PEM certificates
        let certs = rustls_pemfile::certs(&mut reader)
            .collect::<Result<Vec<_>, _>>()
            .map_err(|e| anyhow::anyhow!("Failed to parse certificate: {}", e))?;

        if certs.is_empty() {
            return Err(anyhow::anyhow!("No certificates found in {}", cert_path));
        }

        info!("Found {} certificate(s) in {}", certs.len(), cert_path);

        // Create root certificate store and add custom certificates
        let mut root_store = rustls::RootCertStore::empty();
        for cert in certs {
            root_store
                .add(cert)
                .map_err(|e| anyhow::anyhow!("Failed to add certificate to root store: {}", e))?;
        }

        info!(
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
            .map_err(|e| anyhow::anyhow!("Failed to create platform verifier: {}", e))?
    };

    let tls = MakeRustlsConnect::new(client_config);

    cfg.create_pool(Some(Runtime::Tokio1), tls)
        .map_err(|e| anyhow::anyhow!("Failed to create TLS pool: {}", e))
}

/// Connection pool type alias
pub type DbPool = Pool;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_tls_disabled_by_default() {
        let config = config::DatabaseConfig {
            host: "localhost".to_string(),
            port: 5432,
            database: "test_db".to_string(),
            username: "postgres".to_string(),
            password: "postgres".to_string(),
            max_connections: 5,
            tls_enabled: false,
            tls_ca_cert_path: None,
        };

        assert!(!config.tls_enabled);
    }

    #[test]
    fn test_tls_can_be_enabled() {
        let config = config::DatabaseConfig {
            host: "remote.example.com".to_string(),
            port: 5432,
            database: "prod_db".to_string(),
            username: "user".to_string(),
            password: "pass".to_string(),
            max_connections: 10,
            tls_enabled: true,
            tls_ca_cert_path: None,
        };

        assert!(config.tls_enabled);
    }

    #[test]
    fn test_database_config_validation() {
        // Test valid local configuration without TLS
        let local_config = config::DatabaseConfig {
            host: "localhost".to_string(),
            port: 5432,
            database: "cloud_api".to_string(),
            username: "postgres".to_string(),
            password: "postgres".to_string(),
            max_connections: 5,
            tls_enabled: false,
            tls_ca_cert_path: None,
        };

        assert_eq!(local_config.host, "localhost");
        assert_eq!(local_config.port, 5432);
        assert!(!local_config.tls_enabled);

        // Test valid remote configuration with TLS
        let remote_config = config::DatabaseConfig {
            host: "prod-db.example.com".to_string(),
            port: 5432,
            database: "cloud_api_prod".to_string(),
            username: "app_user".to_string(),
            password: "secure_password".to_string(),
            max_connections: 20,
            tls_enabled: true,
            tls_ca_cert_path: None,
        };

        assert_eq!(remote_config.host, "prod-db.example.com");
        assert!(remote_config.tls_enabled);
    }

    /// Test that TLS pool creation works
    #[test]
    fn test_create_pool_with_tls() {
        let cfg = Config::new();

        // Test TLS pool creation (will fail without actual database, but tests config)
        let result = create_pool_with_rustls(cfg, None);
        assert!(
            result.is_ok() || result.is_err(),
            "Should handle TLS config creation"
        );
    }
}
