//! Explicit wallet binding. Proof verification does not create a login session.
use super::{StakingFarmAmlGate, UpsertStakingFarmSourceRequest};
use crate::{aml::AmlFlow, auth::near::SignedMessage};
use async_trait::async_trait;
use chrono::{DateTime, Duration, Utc};
use config::{NearConfig, StakingFarmConfig};
use near_api::{signer::NEP413Payload, AccountId, NetworkConfig};
use rand_core::{OsRng, RngCore};
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use uuid::Uuid;

pub const BINDING_TERMS_VERSION: &str = "org-staking-admin-v1";

#[derive(Debug, thiserror::Error)]
pub enum BindingError {
    #[error("Wallet binding is unavailable")]
    Unavailable,
    #[error("Organization owner or admin permission is required")]
    Forbidden,
    #[error("Organization or wallet already has a different binding")]
    Conflict,
    #[error("Binding challenge is invalid or expired")]
    InvalidChallenge,
    #[error("Wallet proof or binding acknowledgment is invalid")]
    InvalidProof,
    #[error("Wallet account is unavailable")]
    AccountUnavailable,
    #[error("Too many binding attempts; try again later")]
    RateLimited,
}

#[derive(Debug, Clone, Serialize, Deserialize, utoipa::ToSchema)]
pub struct BindingPayload {
    pub message: String,
    pub nonce: Vec<u8>,
    pub recipient: String,
}
impl BindingPayload {
    pub fn nep413(&self) -> anyhow::Result<NEP413Payload> {
        Ok(NEP413Payload {
            message: self.message.clone(),
            nonce: self
                .nonce
                .as_slice()
                .try_into()
                .map_err(|_| BindingError::InvalidChallenge)?,
            recipient: self.recipient.clone(),
            callback_url: None,
        })
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, utoipa::ToSchema)]
pub struct BindingChallenge {
    pub phase: String,
    pub challenge_id: Uuid,
    pub organization_id: Uuid,
    pub actor_user_id: Uuid,
    pub near_account_id: String,
    pub expires_at: DateTime<Utc>,
    pub payload: BindingPayload,
    pub binding_terms_version: String,
    pub network_id: String,
    pub contract_id: String,
    pub farm_product_id: String,
    pub farm_price_id: Option<String>,
    pub credit_nano_usd_per_reward_unit: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize, utoipa::ToSchema)]
pub struct WalletMembership {
    pub user_id: Uuid,
    pub role: String,
    pub status: String,
}

#[cfg_attr(test, mockall::automock)]
#[async_trait]
pub trait BindingRepository: Send + Sync {
    async fn eligible(&self, org: Uuid) -> anyhow::Result<bool>;
    async fn wallet_organization(
        &self,
        account: &str,
        actor: Uuid,
        network: &str,
        contract: &str,
    ) -> anyhow::Result<Option<Uuid>>;
    async fn begin_attempt(&self, id: Uuid, actor: Uuid) -> anyhow::Result<()>;
    async fn prepare(&self, challenge: &BindingChallenge) -> anyhow::Result<()>;
    async fn get_challenge(
        &self,
        id: Uuid,
        org: Uuid,
        actor: Uuid,
    ) -> anyhow::Result<BindingChallenge>;
    /// Return a committed retry result without granting membership again.
    async fn committed(
        &self,
        id: Uuid,
        org: Uuid,
        actor: Uuid,
        key: Uuid,
        digest: &str,
    ) -> anyhow::Result<Option<WalletMembership>>;
    /// Atomically bind, provision the verified identity, grant Admin and consume proof.
    async fn confirm(
        &self,
        challenge: &BindingChallenge,
        request: UpsertStakingFarmSourceRequest,
        key: Uuid,
        public_key: &str,
        proof_digest: &str,
    ) -> anyhow::Result<WalletMembership>;
}

#[cfg_attr(test, mockall::automock)]
#[async_trait]
pub trait BindingVerifier: Send + Sync {
    async fn verify(&self, payload: &BindingPayload, message: &SignedMessage)
        -> anyhow::Result<()>;
}
pub struct NearBindingVerifier {
    network: NetworkConfig,
}
impl NearBindingVerifier {
    pub fn new(config: &NearConfig) -> anyhow::Result<Self> {
        Ok(Self {
            network: NetworkConfig::from_rpc_url(&config.network_id, config.rpc_url.parse()?),
        })
    }
}
#[async_trait]
impl BindingVerifier for NearBindingVerifier {
    async fn verify(
        &self,
        payload: &BindingPayload,
        message: &SignedMessage,
    ) -> anyhow::Result<()> {
        // Verify both the signature and current account key ownership; no session/nonce login side effects.
        crate::auth::near::verify_wallet_control(&payload.nep413()?, message, &self.network).await
    }
}

pub struct StakingBindingService {
    repository: Arc<dyn BindingRepository>,
    verifier: Arc<dyn BindingVerifier>,
    aml: Arc<dyn StakingFarmAmlGate>,
    config: StakingFarmConfig,
    recipient: String,
}
impl StakingBindingService {
    pub fn new(
        repository: Arc<dyn BindingRepository>,
        verifier: Arc<dyn BindingVerifier>,
        aml: Arc<dyn StakingFarmAmlGate>,
        config: StakingFarmConfig,
        recipient: String,
    ) -> Self {
        Self {
            repository,
            verifier,
            aml,
            config,
            recipient,
        }
    }
    pub async fn eligible(&self, org: Uuid) -> anyhow::Result<bool> {
        Ok(self.config.enabled
            && self.config.selected_org_binding_enabled
            && self.repository.eligible(org).await?)
    }
    pub async fn wallet_organization(
        &self,
        account: &str,
        actor: Uuid,
    ) -> anyhow::Result<Option<Uuid>> {
        self.repository
            .wallet_organization(
                account,
                actor,
                &self.config.network_id,
                &self.config.contract_id,
            )
            .await
    }
    fn enabled(&self) -> anyhow::Result<()> {
        if !self.config.enabled || !self.config.selected_org_binding_enabled {
            return Err(BindingError::Unavailable.into());
        }
        Ok(())
    }
    pub async fn prepare(
        &self,
        organization_id: Uuid,
        actor_user_id: Uuid,
        account: String,
    ) -> anyhow::Result<BindingChallenge> {
        self.enabled()?;
        let account: AccountId = account.parse().map_err(|_| BindingError::InvalidProof)?;
        let account = account.to_string();
        let challenge_id = Uuid::new_v4();
        let expires_at = Utc::now() + Duration::minutes(5);
        let mut nonce = vec![0; 32];
        OsRng.fill_bytes(&mut nonce);
        let c = &self.config;
        let message = format!("Bind {account} to Cloud organization {organization_id} as its permanent staking credit source and add its NEAR Cloud account as an organization Admin. The wallet cannot be changed and the organization cannot be deleted, even after unstaking.\nAction: org-staking-bind\nActor: {actor_user_id}\nNetwork: {}\nContract: {}\nProduct: {}\nChallenge: {challenge_id}\nExpires: {expires_at}\nTerms: {BINDING_TERMS_VERSION}", c.network_id, c.contract_id, c.farm_product_id);
        let challenge = BindingChallenge {
            phase: "prepare".into(),
            challenge_id,
            organization_id,
            actor_user_id,
            near_account_id: account,
            expires_at,
            payload: BindingPayload {
                message,
                nonce,
                recipient: self.recipient.clone(),
            },
            binding_terms_version: BINDING_TERMS_VERSION.into(),
            network_id: c.network_id.clone(),
            contract_id: c.contract_id.clone(),
            farm_product_id: c.farm_product_id.clone(),
            farm_price_id: c.farm_price_id.clone(),
            credit_nano_usd_per_reward_unit: c.credit_nano_usd_per_reward_unit,
        };
        self.repository.prepare(&challenge).await?;
        Ok(challenge)
    }
    pub async fn confirm(
        &self,
        org: Uuid,
        actor: Uuid,
        id: Uuid,
        key: Uuid,
        terms: &str,
        message: SignedMessage,
    ) -> anyhow::Result<WalletMembership> {
        self.enabled()?;
        let challenge = self.repository.get_challenge(id, org, actor).await?;
        if terms != challenge.binding_terms_version
            || message.account_id.as_str() != challenge.near_account_id
        {
            return Err(BindingError::InvalidProof.into());
        }
        // A retry cannot use another account/proof or restore deliberately removed membership.
        use sha2::{Digest, Sha256};
        let proof_digest = hex::encode(Sha256::digest(serde_json::to_vec(&message)?));
        if let Some(result) = self
            .repository
            .committed(id, org, actor, key, &proof_digest)
            .await?
        {
            return Ok(result);
        }
        if challenge.expires_at <= Utc::now()
            || challenge.payload.recipient != self.recipient
            || challenge.network_id != self.config.network_id
            || challenge.contract_id != self.config.contract_id
            || challenge.farm_product_id != self.config.farm_product_id
            || challenge.farm_price_id != self.config.farm_price_id
            || challenge.credit_nano_usd_per_reward_unit
                != self.config.credit_nano_usd_per_reward_unit
        {
            return Err(BindingError::InvalidChallenge.into());
        }
        self.repository.begin_attempt(id, actor).await?;
        self.verifier.verify(&challenge.payload, &message).await?;
        self.aml
            .check_near_account(
                Some(actor),
                &challenge.near_account_id,
                AmlFlow::StakingFarmSync,
            )
            .await?;
        let request = UpsertStakingFarmSourceRequest {
            organization_id: org,
            near_account_id: challenge.near_account_id.clone(),
            network_id: challenge.network_id.clone(),
            contract_id: challenge.contract_id.clone(),
            farm_product_id: challenge.farm_product_id.clone(),
            farm_price_id: challenge.farm_price_id.clone(),
            credit_nano_usd_per_reward_unit: challenge.credit_nano_usd_per_reward_unit,
            created_by_user_id: Some(actor),
        };
        self.repository
            .confirm(
                &challenge,
                request,
                key,
                &message.public_key.to_string(),
                &proof_digest,
            )
            .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::aml::AmlError;
    use std::sync::Mutex;
    struct Aml {
        accounts: Mutex<Vec<String>>,
        blocked: bool,
    }
    #[async_trait]
    impl StakingFarmAmlGate for Aml {
        async fn check_near_account(
            &self,
            _: Option<Uuid>,
            account: &str,
            _: AmlFlow,
        ) -> Result<(), AmlError> {
            self.accounts.lock().unwrap().push(account.into());
            if self.blocked {
                Err(AmlError::AccountBlocked)
            } else {
                Ok(())
            }
        }
    }
    fn config() -> StakingFarmConfig {
        StakingFarmConfig {
            enabled: true,
            selected_org_binding_enabled: true,
            network_id: "testnet".into(),
            contract_id: "stake.testnet".into(),
            farm_product_id: "cloud".into(),
            ..Default::default()
        }
    }
    fn challenge() -> BindingChallenge {
        BindingChallenge {
            phase: "prepare".into(),
            challenge_id: Uuid::new_v4(),
            organization_id: Uuid::new_v4(),
            actor_user_id: Uuid::new_v4(),
            near_account_id: "alice.testnet".into(),
            expires_at: Utc::now() + Duration::minutes(5),
            payload: BindingPayload {
                message: "Bind this org and grant Admin".into(),
                nonce: vec![7; 32],
                recipient: "cloud.example".into(),
            },
            binding_terms_version: BINDING_TERMS_VERSION.into(),
            network_id: "testnet".into(),
            contract_id: "stake.testnet".into(),
            farm_product_id: "cloud".into(),
            farm_price_id: None,
            credit_nano_usd_per_reward_unit: 1_000_000_000,
        }
    }
    async fn proof(payload: &BindingPayload) -> SignedMessage {
        // Public test-only seed from NEAR's NEP-413 test fixtures.
        let signer = near_api::signer::Signer::from_seed_phrase(
            "fatal edge jacket cash hard pass gallery fabric whisper size rain biology",
            None,
        )
        .unwrap();
        let public_key = signer.get_public_key().await.unwrap();
        let account_id: AccountId = "alice.testnet".parse().unwrap();
        let signature = signer
            .sign_message_nep413(account_id.clone(), public_key, &payload.nep413().unwrap())
            .await
            .unwrap();
        SignedMessage {
            account_id,
            public_key,
            signature,
            state: None,
        }
    }
    fn repository(c: &BindingChallenge) -> MockBindingRepository {
        let mut repo = MockBindingRepository::new();
        let stored = c.clone();
        repo.expect_get_challenge()
            .returning(move |id, org, actor| {
                if id != stored.challenge_id
                    || org != stored.organization_id
                    || actor != stored.actor_user_id
                {
                    return Err(BindingError::InvalidChallenge.into());
                }
                Ok(stored.clone())
            });
        repo.expect_committed().returning(|_, _, _, _, _| Ok(None));
        repo
    }
    #[tokio::test]
    async fn invalid_expired_wrong_actor_or_unacknowledged_proof_never_commits() {
        for case in [
            "expired",
            "wrong_actor",
            "wrong_wallet",
            "missing_terms",
            "stored_terms_changed",
        ] {
            let mut c = challenge();
            if case == "expired" {
                c.expires_at = Utc::now() - Duration::seconds(1);
            }
            if case == "stored_terms_changed" {
                c.binding_terms_version = "previous-version".into();
            }
            let repo = repository(&c);
            let verifier = MockBindingVerifier::new();
            let aml = Arc::new(Aml {
                accounts: Mutex::new(vec![]),
                blocked: false,
            });
            let service = StakingBindingService::new(
                Arc::new(repo),
                Arc::new(verifier),
                aml.clone(),
                config(),
                "cloud.example".into(),
            );
            let mut message = proof(&c.payload).await;
            if case == "wrong_wallet" {
                message.account_id = "bob.testnet".parse().unwrap();
            }
            let actor = if case == "wrong_actor" {
                Uuid::new_v4()
            } else {
                c.actor_user_id
            };
            let terms = if case == "missing_terms" {
                ""
            } else {
                BINDING_TERMS_VERSION
            };
            assert!(service
                .confirm(
                    c.organization_id,
                    actor,
                    c.challenge_id,
                    Uuid::new_v4(),
                    terms,
                    message
                )
                .await
                .is_err());
            assert!(aml.accounts.lock().unwrap().is_empty());
        }
    }
    #[tokio::test]
    async fn proof_and_source_wallet_aml_are_required_before_commit() {
        for blocked in [false, true] {
            let mut c = challenge();
            if !blocked {
                c.binding_terms_version = "previous-version".into();
                c.payload.message.push_str(" Terms: previous-version");
            }
            let mut repo = repository(&c);
            repo.expect_begin_attempt()
                .times(1)
                .returning(|_, _| Ok(()));
            if !blocked {
                repo.expect_confirm()
                    .times(1)
                    .withf(|c, r, _, _, _| {
                        c.near_account_id == r.near_account_id
                            && r.near_account_id == "alice.testnet"
                    })
                    .returning(|_, _, _, _, _| {
                        Ok(WalletMembership {
                            user_id: Uuid::new_v4(),
                            role: "admin".into(),
                            status: "active".into(),
                        })
                    });
            }
            let mut verifier = MockBindingVerifier::new();
            verifier.expect_verify().times(1).returning(|_, _| Ok(()));
            let aml = Arc::new(Aml {
                accounts: Mutex::new(vec![]),
                blocked,
            });
            let service = StakingBindingService::new(
                Arc::new(repo),
                Arc::new(verifier),
                aml.clone(),
                config(),
                "cloud.example".into(),
            );
            let result = service
                .confirm(
                    c.organization_id,
                    c.actor_user_id,
                    c.challenge_id,
                    Uuid::new_v4(),
                    &c.binding_terms_version,
                    proof(&c.payload).await,
                )
                .await;
            assert_eq!(result.is_err(), blocked);
            assert_eq!(*aml.accounts.lock().unwrap(), vec!["alice.testnet"]);
        }
    }
    #[tokio::test]
    async fn real_nep413_verifier_requires_full_access_and_rejects_modified_payload() {
        use wiremock::{matchers::method, Mock, MockServer, ResponseTemplate};
        let c = challenge();
        let message = proof(&c.payload).await;
        for full_access in [true, false] {
            let server = MockServer::start().await;
            let permission = if full_access {
                serde_json::json!("FullAccess")
            } else {
                serde_json::json!({"FunctionCall":{"allowance":null,"receiver_id":"stake.testnet","method_names":[]}})
            };
            Mock::given(method("POST")).respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({"jsonrpc":"2.0","id":"dontcare","result":{"block_hash":"11111111111111111111111111111111","block_height":1,"nonce":0,"permission":permission}}))).mount(&server).await;
            let verifier = NearBindingVerifier::new(&NearConfig {
                rpc_url: server.uri(),
                network_id: "testnet".into(),
                expected_recipient: "cloud.example".into(),
            })
            .unwrap();
            assert_eq!(
                verifier.verify(&c.payload, &message).await.is_ok(),
                full_access
            );
            let mut tampered = c.payload.clone();
            tampered.message.push_str("other destination");
            assert!(verifier.verify(&tampered, &message).await.is_err());
        }
    }
}
