use chrono::{DateTime, Duration, Utc};
use config::NearConfig;
use near_api::{signer::NEP413Payload, types::Signature, AccountId, NetworkConfig, PublicKey};
use std::sync::Arc;
use url::Url;

use super::ports::{AuthServiceTrait, NearNonceRepository, OAuthUserInfo, Session};

const MAX_NONCE_AGE_MS: u64 = 5 * 60 * 1000; // 5 minutes
const EXPECTED_MESSAGE: &str = "Sign in to NEAR AI Cloud API";

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

impl NearAuthService {
    pub fn new(
        auth_service: Arc<dyn AuthServiceTrait>,
        nonce_repository: Arc<dyn NearNonceRepository>,
        config: NearConfig,
    ) -> Self {
        let network_config =
            NetworkConfig::from_rpc_url("near", Url::parse(&config.rpc_url).unwrap());
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

    fn validate_recipient(&self, recipient: &str) -> anyhow::Result<()> {
        if recipient == self.config.expected_recipient {
            Ok(())
        } else {
            Err(anyhow::anyhow!(
                "Invalid recipient: expected {}, got {}",
                self.config.expected_recipient,
                recipient
            ))
        }
    }

    fn validate_message(message: &str) -> anyhow::Result<()> {
        if message == EXPECTED_MESSAGE {
            Ok(())
        } else {
            Err(anyhow::anyhow!(
                "Invalid message: expected '{EXPECTED_MESSAGE}', got '{message}'"
            ))
        }
    }

    pub async fn verify_and_authenticate(
        &self,
        signed_message: SignedMessage,
        payload: NEP413Payload,
        ip_address: Option<String>,
        user_agent: String,
        encoding_key: String,
    ) -> anyhow::Result<(String, Session, String, bool)> {
        let account_id = signed_message.account_id.to_string();

        tracing::info!("NEAR authentication attempt for account: {}", account_id);

        // 1. Validate recipient
        self.validate_recipient(&payload.recipient)?;

        // 2. Validate message
        Self::validate_message(&payload.message)?;

        // 3. Cleanup expired nonces
        self.cleanup_nonces().await;

        // 4. Extract timestamp from nonce (first 8 bytes are timestamp)
        // Nonce format: [8 bytes timestamp (big-endian)] + [24 bytes random]
        if payload.nonce.len() >= 8 {
            let timestamp_bytes = &payload.nonce[0..8];
            let nonce_timestamp_ms = u64::from_be_bytes([
                timestamp_bytes[0],
                timestamp_bytes[1],
                timestamp_bytes[2],
                timestamp_bytes[3],
                timestamp_bytes[4],
                timestamp_bytes[5],
                timestamp_bytes[6],
                timestamp_bytes[7],
            ]);

            if nonce_timestamp_ms > 0 {
                let nonce_time = DateTime::from_timestamp_millis(nonce_timestamp_ms as i64);
                if let Some(nonce_time) = nonce_time {
                    let age = Utc::now().signed_duration_since(nonce_time);
                    if age > Duration::milliseconds(MAX_NONCE_AGE_MS as i64) {
                        tracing::warn!(
                            "NEAR signature expired for account {}: age={:?}ms, max_age={}ms",
                            account_id,
                            age.num_milliseconds(),
                            MAX_NONCE_AGE_MS
                        );
                        return Err(anyhow::anyhow!("Signature expired"));
                    }
                    if age < Duration::zero() {
                        tracing::warn!(
                            "NEAR signature has future timestamp for account {}",
                            account_id
                        );
                        return Err(anyhow::anyhow!("Invalid signature timestamp"));
                    }
                }
            }
        }

        // 5. Verify signature AND public key ownership via near-api
        let is_valid = payload
            .verify(
                &signed_message.account_id,
                signed_message.public_key,
                &signed_message.signature,
                &self.network_config,
            )
            .await
            .map_err(|e| anyhow::anyhow!("Signature verification failed: {e}"))?;

        if !is_valid {
            return Err(anyhow::anyhow!("Invalid signature"));
        }

        // 6. Consume nonce AFTER signature verification (replay protection)
        // This prevents attackers from burning legitimate nonces with invalid signatures
        let nonce_hex = hex::encode(payload.nonce);
        let nonce_consumed = self.nonce_repository.consume_nonce(&nonce_hex).await?;
        if !nonce_consumed {
            tracing::warn!("NEAR signature replay detected for account {}", account_id);
            return Err(anyhow::anyhow!(
                "Nonce already used (replay attack detected)"
            ));
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

        Ok((
            access_token,
            session,
            refresh_token,
            user.created_at == user.updated_at,
        ))
    }
}
