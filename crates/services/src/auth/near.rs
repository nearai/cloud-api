use chrono::{DateTime, Duration, Utc};
use config::NearConfig;
use near_api::{signer::NEP413Payload, types::Signature, AccountId, NetworkConfig, PublicKey};
use std::fmt::{Display, Formatter};
use std::sync::Arc;
use url::Url;

use super::ports::{AuthServiceTrait, NearNonceRepository, OAuthUserInfo, Session};

const MAX_NONCE_AGE_MS: u64 = 5 * 60 * 1000; // 5 minutes
const EXPECTED_MESSAGE: &str = "Sign in to NEAR AI Cloud API";

/// Custom error type for NEAR authentication
#[derive(Debug)]
pub enum NearAuthError {
    InvalidSignature,
    ReplayAttack,
    ExpiredNonce,
    InvalidTimestamp,
    InvalidNonce(String),
    InvalidRecipient(String),
    InvalidMessage(String),
    SignatureVerificationFailed(String),
    InternalError(String),
}

impl Display for NearAuthError {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InvalidSignature => write!(f, "Invalid signature"),
            Self::ReplayAttack => write!(f, "Nonce already used (replay attack detected)"),
            Self::ExpiredNonce => write!(f, "Signature expired"),
            Self::InvalidTimestamp => write!(f, "Invalid signature timestamp"),
            Self::InvalidNonce(msg) => write!(f, "Invalid nonce: {msg}"),
            Self::InvalidRecipient(msg) => write!(f, "Invalid recipient: {msg}"),
            Self::InvalidMessage(msg) => write!(f, "Invalid message: {msg}"),
            Self::SignatureVerificationFailed(msg) => {
                write!(f, "Signature verification failed: {msg}")
            }
            Self::InternalError(msg) => write!(f, "{msg}"),
        }
    }
}

impl std::error::Error for NearAuthError {}

/// Signed message data received from the wallet (NEP-413 output)
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct SignedMessage {
    #[serde(rename = "accountId")]
    pub account_id: AccountId,
    #[serde(rename = "publicKey")]
    pub public_key: PublicKey,
    pub signature: Signature,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub state: Option<String>,
}

/// Helper to verify NEP-413 signed messages and create sessions
pub struct NearAuthService {
    auth_service: Arc<dyn AuthServiceTrait>,
    nonce_repository: Arc<dyn NearNonceRepository>,
    config: NearConfig,
    network_config: NetworkConfig,
}

/// Validates that a nonce timestamp is within the acceptable window
///
/// # Arguments
/// * `now` - Current time for comparison
/// * `nonce_timestamp_ms` - Timestamp from nonce in milliseconds
///
/// # Errors
/// Returns error if:
/// - Timestamp is out of valid range
/// - Timestamp is older than MAX_NONCE_AGE_MS (expired)
/// - Timestamp is in the future
pub(crate) fn validate_nonce_timestamp_ms(
    now: DateTime<Utc>,
    nonce_timestamp_ms: u64,
) -> Result<(), NearAuthError> {
    let nonce_time = DateTime::from_timestamp_millis(nonce_timestamp_ms as i64).ok_or(
        NearAuthError::InvalidNonce("timestamp out of valid range".to_string()),
    )?;

    let age = now.signed_duration_since(nonce_time);

    if age > Duration::milliseconds(MAX_NONCE_AGE_MS as i64) {
        return Err(NearAuthError::ExpiredNonce);
    }

    if age < Duration::zero() {
        return Err(NearAuthError::InvalidTimestamp);
    }

    Ok(())
}

impl NearAuthService {
    pub fn new(
        auth_service: Arc<dyn AuthServiceTrait>,
        nonce_repository: Arc<dyn NearNonceRepository>,
        config: NearConfig,
    ) -> Self {
        let rpc_url = Url::parse(&config.rpc_url).unwrap_or_else(|_| {
            panic!(
                "Invalid NEAR RPC URL in configuration: '{}'",
                config.rpc_url
            )
        });
        let network_config = NetworkConfig::from_rpc_url("near", rpc_url);
        Self {
            auth_service,
            nonce_repository,
            config,
            network_config,
        }
    }

    async fn cleanup_nonces(&self) {
        if let Err(err) = self.nonce_repository.cleanup_expired_nonces().await {
            tracing::warn!("Failed to cleanup expired NEAR nonces: {}", err);
        }
    }

    fn validate_recipient(&self, recipient: &str) -> Result<(), NearAuthError> {
        if recipient == self.config.expected_recipient {
            Ok(())
        } else {
            Err(NearAuthError::InvalidRecipient(format!(
                "expected {}, got {}",
                self.config.expected_recipient, recipient
            )))
        }
    }

    fn validate_message(message: &str) -> Result<(), NearAuthError> {
        if message == EXPECTED_MESSAGE {
            Ok(())
        } else {
            Err(NearAuthError::InvalidMessage(format!(
                "expected '{EXPECTED_MESSAGE}', got '{message}'"
            )))
        }
    }

    pub async fn verify_and_authenticate(
        &self,
        signed_message: SignedMessage,
        payload: NEP413Payload,
        ip_address: Option<String>,
        user_agent: String,
        encoding_key: String,
    ) -> anyhow::Result<(String, Session, String)> {
        let account_id = signed_message.account_id.to_string();

        tracing::info!("NEAR authentication attempt for account: {}", account_id);

        // 1. Validate recipient
        self.validate_recipient(&payload.recipient)
            .map_err(|e| anyhow::anyhow!(e))?;

        // 2. Validate message
        Self::validate_message(&payload.message).map_err(|e| anyhow::anyhow!(e))?;

        // 3. Cleanup expired nonces
        self.cleanup_nonces().await;

        // 4. Extract timestamp from nonce (first 8 bytes are timestamp)
        // Nonce format: [8 bytes timestamp (big-endian)] + [24 bytes random]
        // Nonce is guaranteed to be 32 bytes (validated during deserialization)
        let nonce_timestamp_ms = u64::from_be_bytes(payload.nonce[0..8].try_into().unwrap());

        // Reject zero-timestamp nonces - a valid nonce must have a current timestamp
        if nonce_timestamp_ms == 0 {
            tracing::warn!(
                "NEAR signature rejected: nonce has zero timestamp for account {}",
                account_id
            );
            return Err(anyhow::anyhow!(NearAuthError::InvalidNonce(
                "zero timestamp".to_string()
            )));
        }

        // Validate timestamp is within acceptable range
        validate_nonce_timestamp_ms(Utc::now(), nonce_timestamp_ms).map_err(|e| {
            tracing::warn!(
                "NEAR signature rejected: invalid timestamp for account {}: {}",
                account_id,
                e
            );
            anyhow::anyhow!(e)
        })?;

        // 5. Verify signature AND public key ownership via near-api
        let is_valid = payload
            .verify(
                &signed_message.account_id,
                signed_message.public_key,
                &signed_message.signature,
                &self.network_config,
            )
            .await
            .map_err(|e| {
                anyhow::anyhow!(NearAuthError::SignatureVerificationFailed(e.to_string()))
            })?;

        if !is_valid {
            return Err(anyhow::anyhow!(NearAuthError::InvalidSignature));
        }

        // 6. Consume nonce AFTER signature verification (replay protection)
        // This prevents attackers from burning legitimate nonces with invalid signatures
        let nonce_hex = hex::encode(payload.nonce);
        let nonce_consumed = self.nonce_repository.consume_nonce(&nonce_hex).await?;
        if !nonce_consumed {
            tracing::warn!("NEAR signature replay detected for account {}", account_id);
            return Err(anyhow::anyhow!(NearAuthError::ReplayAttack));
        }

        // 7. Find or create user via AuthService
        let oauth_info = OAuthUserInfo {
            provider: "near".to_string(),
            provider_user_id: account_id.clone(),
            email: format!("{account_id}@near"),
            username: account_id.clone(),
            display_name: Some(account_id.clone()),
            avatar_url: None,
        };
        let user = self
            .auth_service
            .get_or_create_oauth_user(oauth_info)
            .await?;

        // 8. Create session via AuthService (dual-token system)
        let (access_token, session, refresh_token) = self
            .auth_service
            .create_session(
                user.id.clone(),
                ip_address,
                user_agent,
                encoding_key,
                1,
                7 * 24,
            )
            .await?;

        tracing::info!("NEAR authentication successful - account_id={}", account_id);

        Ok((access_token, session, refresh_token))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    #[test]
    fn test_rejects_out_of_range_timestamp() {
        let now = Utc.timestamp_millis_opt(1_000_000).unwrap();

        // i64::MAX is beyond chrono's valid range
        let nonce_timestamp_ms = i64::MAX as u64;

        let result = validate_nonce_timestamp_ms(now, nonce_timestamp_ms);

        assert!(result.is_err(), "Out-of-range timestamp should be rejected");
        let err_msg = format!("{}", result.unwrap_err());
        assert!(
            err_msg.contains("out of valid range"),
            "Error should mention out of range, got: {err_msg}"
        );
    }

    #[test]
    fn test_accepts_recent_valid_timestamp() {
        let now = Utc.timestamp_millis_opt(1_000_000).unwrap();
        let nonce_ts = now.timestamp_millis() - 60_000; // 1 minute ago

        let result = validate_nonce_timestamp_ms(now, nonce_ts as u64);

        assert!(result.is_ok(), "Recent timestamp should be accepted");
    }

    #[test]
    fn test_rejects_expired_timestamp() {
        let now = Utc.timestamp_millis_opt(1_000_000).unwrap();
        let nonce_ts = now.timestamp_millis() - (10 * 60 * 1000); // 10 minutes ago (exceeds 5-minute window)

        let result = validate_nonce_timestamp_ms(now, nonce_ts as u64);

        assert!(result.is_err(), "Expired timestamp should be rejected");
        let err_msg = format!("{}", result.unwrap_err());
        assert!(
            err_msg.contains("Signature expired"),
            "Error should mention expiration, got: {err_msg}"
        );
    }

    #[test]
    fn test_rejects_future_timestamp() {
        let now = Utc.timestamp_millis_opt(1_000_000).unwrap();
        let nonce_ts = now.timestamp_millis() + 60_000; // 1 minute in the future

        let result = validate_nonce_timestamp_ms(now, nonce_ts as u64);

        assert!(result.is_err(), "Future timestamp should be rejected");
        let err_msg = format!("{}", result.unwrap_err());
        assert!(
            err_msg.contains("Invalid signature timestamp"),
            "Error should mention invalid timestamp, got: {err_msg}"
        );
    }

    #[test]
    fn test_accepts_timestamp_at_boundary() {
        let now = Utc.timestamp_millis_opt(1_000_000).unwrap();

        // Exactly at the 5-minute boundary should still be valid
        let nonce_ts = now.timestamp_millis() - (5 * 60 * 1000); // Exactly 5 minutes ago

        let result = validate_nonce_timestamp_ms(now, nonce_ts as u64);

        assert!(
            result.is_ok(),
            "Timestamp at exactly 5-minute boundary should be accepted"
        );
    }

    #[test]
    fn test_rejects_timestamp_just_beyond_boundary() {
        let now = Utc.timestamp_millis_opt(1_000_000).unwrap();

        // Just beyond the 5-minute boundary should be rejected
        let nonce_ts = now.timestamp_millis() - (5 * 60 * 1000 + 1); // 5 minutes + 1ms ago

        let result = validate_nonce_timestamp_ms(now, nonce_ts as u64);

        assert!(
            result.is_err(),
            "Timestamp beyond 5-minute window should be rejected"
        );
    }

    #[test]
    fn test_negative_i64_max_out_of_range() {
        // i64::MIN also causes from_timestamp_millis to return None
        let now = Utc.timestamp_millis_opt(1_000_000).unwrap();

        // Large negative value (way before year -262144)
        let nonce_timestamp_ms = (-9_000_000_000_000_000i64) as u64;

        let result = validate_nonce_timestamp_ms(now, nonce_timestamp_ms);

        assert!(
            result.is_err(),
            "Timestamp way in the past should be rejected"
        );
    }

    #[test]
    fn test_empty_nonce_timestamp_validation() {
        let now = Utc.timestamp_millis_opt(1_000_000).unwrap();
        let empty_nonce_ms = 0u64;

        let result = validate_nonce_timestamp_ms(now, empty_nonce_ms);

        // Zero timestamp is from 1970, should be rejected as expired
        assert!(result.is_err(), "Empty nonce should be rejected");
    }

    #[test]
    fn test_nonce_timestamp_with_small_value() {
        // Test that a future timestamp is correctly rejected
        // now = 1_000_000 ms (16.67 minutes after Unix epoch)
        // short_timestamp_ms = 0xFFFF_FFFF (4294967295 ms = 49.7 days after epoch)
        // This timestamp is ~49.5 days in the future
        let now = Utc.timestamp_millis_opt(1_000_000).unwrap();
        let short_timestamp_ms = 0xFFFF_FFFFu64;

        let result = validate_nonce_timestamp_ms(now, short_timestamp_ms);

        assert!(result.is_err(), "Future timestamp should be rejected");
        let err_msg = format!("{}", result.unwrap_err());
        assert!(
            err_msg.contains("Invalid signature timestamp"),
            "Error should mention invalid timestamp, got: {err_msg}"
        );
    }
}
