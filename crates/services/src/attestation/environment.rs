use hmac::{Hmac, KeyInit, Mac};
use sha2::Sha256;

use super::{AttestationError, AttestationService, VpcInfo};

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

pub fn load_vpc_shared_secret() -> Option<String> {
    if let Ok(path) = std::env::var("VPC_SHARED_SECRET_FILE") {
        std::fs::read_to_string(path)
            .map_err(|_| tracing::warn!("Failed to read VPC shared secret file"))
            .ok()
            .map(|s| s.trim().to_string())
    } else {
        None
    }
}

/// Optional secret for `POST /auth/vpc/login`, read from `SERVICE_LOGIN_SECRET_FILE`.
pub fn load_service_login_secret() -> Option<String> {
    let path = std::env::var("SERVICE_LOGIN_SECRET_FILE").ok()?;
    std::fs::read_to_string(path)
        .map_err(|_| tracing::warn!("Failed to read service login secret file"))
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

/// `VPC_LOGIN_ALLOW_SHARED_SECRET`: whether the VPC shared secret is still accepted for login.
/// Defaults to true; only `false`/`0` (case-insensitive) turn it off.
pub fn load_vpc_login_allow_shared_secret() -> bool {
    match std::env::var("VPC_LOGIN_ALLOW_SHARED_SECRET") {
        Ok(v) => !matches!(v.trim().to_ascii_lowercase().as_str(), "false" | "0"),
        Err(_) => true,
    }
}

/// Constant-time check of an HMAC-SHA256 hex signature over the decimal timestamp.
fn hmac_matches(secret: &str, message: &[u8], provided: &[u8]) -> Result<bool, AttestationError> {
    let mut mac = Hmac::<Sha256>::new_from_slice(secret.as_bytes())
        .map_err(|e| AttestationError::InternalError(format!("Failed to create HMAC: {e}")))?;
    mac.update(message);
    Ok(mac.verify_slice(provided).is_ok())
}

impl AttestationService {
    pub(in crate::attestation) async fn verify_vpc_signature_impl(
        &self,
        timestamp: i64,
        signature: String,
    ) -> Result<bool, AttestationError> {
        let mut secrets: Vec<&str> = Vec::new();
        if let Some(secret) = self.service_login_secret.as_deref() {
            secrets.push(secret);
        }
        if self.vpc_login_allow_shared_secret {
            if let Some(secret) = self.vpc_shared_secret.as_deref() {
                secrets.push(secret);
            }
        }
        if secrets.is_empty() {
            return Err(AttestationError::InternalError(
                "Failed to load VPC shared secret".to_string(),
            ));
        }
        let now = chrono::Utc::now().timestamp();
        let diff = (now - timestamp).abs();
        if diff > 30 {
            tracing::warn!(
                "VPC signature timestamp expired: current={now}, provided={timestamp}, diff={diff}"
            );
            return Ok(false);
        }

        let provided_bytes = match hex::decode(&signature) {
            Ok(bytes) => bytes,
            Err(_) => {
                tracing::warn!("Invalid hex in VPC signature");
                return Ok(false);
            }
        };
        let message = timestamp.to_string();
        // Check every candidate so timing does not reveal which one matched.
        let mut matched = false;
        for secret in secrets {
            matched |= hmac_matches(secret, message.as_bytes(), &provided_bytes)?;
        }
        if !matched {
            tracing::warn!("VPC signature mismatch");
        }
        Ok(matched)
    }
}

#[cfg(test)]
mod login_signature_tests {
    use super::*;
    use crate::attestation::chat_signature_lifecycle_tests::{
        lifecycle_service, RecordingRepository,
    };
    use crate::inference_provider_pool::InferenceProviderPool;
    use config::ExternalProvidersConfig;
    use std::sync::Arc;

    fn service_with_secrets(
        new: Option<&str>,
        old: Option<&str>,
        allow_old: bool,
    ) -> AttestationService {
        let pool = Arc::new(InferenceProviderPool::new(
            None,
            ExternalProvidersConfig::default(),
        ));
        let mut svc = lifecycle_service(Arc::new(RecordingRepository::default()), pool);
        svc.service_login_secret = new.map(str::to_string);
        svc.vpc_shared_secret = old.map(str::to_string);
        svc.vpc_login_allow_shared_secret = allow_old;
        svc
    }

    fn sign(secret: &str, ts: i64) -> String {
        let mut mac = Hmac::<Sha256>::new_from_slice(secret.as_bytes()).unwrap();
        mac.update(ts.to_string().as_bytes());
        hex::encode(mac.finalize().into_bytes())
    }

    #[tokio::test]
    async fn accepts_new_and_old_secret_when_allowed() {
        let svc = service_with_secrets(Some("new"), Some("old"), true);
        let ts = chrono::Utc::now().timestamp();
        assert!(svc
            .verify_vpc_signature_impl(ts, sign("new", ts))
            .await
            .unwrap());
        assert!(svc
            .verify_vpc_signature_impl(ts, sign("old", ts))
            .await
            .unwrap());
        assert!(!svc
            .verify_vpc_signature_impl(ts, sign("other", ts))
            .await
            .unwrap());
    }

    #[tokio::test]
    async fn rejects_old_secret_when_not_allowed() {
        let svc = service_with_secrets(Some("new"), Some("old"), false);
        let ts = chrono::Utc::now().timestamp();
        assert!(svc
            .verify_vpc_signature_impl(ts, sign("new", ts))
            .await
            .unwrap());
        assert!(!svc
            .verify_vpc_signature_impl(ts, sign("old", ts))
            .await
            .unwrap());
    }

    #[tokio::test]
    async fn old_secret_only_not_allowed_is_unavailable() {
        let svc = service_with_secrets(None, Some("old"), false);
        let ts = chrono::Utc::now().timestamp();
        assert!(svc
            .verify_vpc_signature_impl(ts, sign("old", ts))
            .await
            .is_err());
    }

    #[tokio::test]
    async fn rejects_stale_timestamp_and_bad_hex() {
        let svc = service_with_secrets(Some("new"), None, true);
        let ts = chrono::Utc::now().timestamp() - 300;
        assert!(!svc
            .verify_vpc_signature_impl(ts, sign("new", ts))
            .await
            .unwrap());
        let now = chrono::Utc::now().timestamp();
        assert!(!svc
            .verify_vpc_signature_impl(now, "zz".into())
            .await
            .unwrap());
    }

    #[tokio::test]
    async fn no_secrets_is_unavailable() {
        let svc = service_with_secrets(None, None, true);
        let ts = chrono::Utc::now().timestamp();
        assert!(svc
            .verify_vpc_signature_impl(ts, sign("x", ts))
            .await
            .is_err());
    }
}
