use super::VpcInfo;

pub fn load_vpc_info() -> Option<VpcInfo> {
    let vpc_server_app_id = std::env::var("VPC_SERVER_APP_ID").ok();
    let vpc_hostname = if let Ok(path) = std::env::var("VPC_HOSTNAME_FILE") {
        std::fs::read_to_string(path)
            .map_err(|e| tracing::warn!("Failed to read VPC hostname file: {e}"))
            .ok()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
    } else {
        None
    };

    if vpc_server_app_id.is_some() || vpc_hostname.is_some() {
        Some(VpcInfo {
            vpc_server_app_id,
            vpc_hostname,
        })
    } else {
        None
    }
}

pub fn load_tls_cert_fingerprint() -> Option<String> {
    let path = std::env::var("TLS_CERT_PATH").ok()?;
    match compute_spki_hash(&path) {
        Ok(hash) => {
            tracing::info!(
                tls_cert_path = %path,
                fingerprint = %hash,
                "TLS certificate SPKI hash computed"
            );
            Some(hash)
        }
        Err(e) => {
            tracing::warn!(
                tls_cert_path = %path,
                error = %e,
                "Failed to compute TLS cert fingerprint (TLS_CERT_PATH)"
            );
            None
        }
    }
}

pub fn compute_spki_hash(cert_path: &str) -> Result<String, String> {
    use sha2::Digest;
    let pem_data =
        std::fs::read(cert_path).map_err(|e| format!("failed to read cert {cert_path}: {e}"))?;
    let (_, pem) = x509_parser::pem::parse_x509_pem(&pem_data)
        .map_err(|e| format!("failed to parse PEM: {e}"))?;
    let (_, cert) = x509_parser::parse_x509_certificate(&pem.contents)
        .map_err(|e| format!("failed to parse X.509: {e}"))?;
    let spki_der = cert.tbs_certificate.subject_pki.raw;
    let mut hasher = sha2::Sha256::new();
    hasher.update(spki_der);
    Ok(hex::encode(hasher.finalize()))
}
