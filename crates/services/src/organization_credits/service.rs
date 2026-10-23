use super::models::*;
use super::ports::{CreditTransferRepositoryPort, OrganizationCredentialRepositoryPort};
use async_trait::async_trait;
use serde_json::Value;
use std::sync::Arc;
use uuid::Uuid;

#[async_trait]
pub trait OrganizationCreditServiceTrait: Send + Sync {
    async fn transfer(
        &self,
        org: Uuid,
        actor: &TransferActor,
        key: &str,
        request: &TransferRequest,
    ) -> anyhow::Result<(Value, bool)>;
    async fn capacity(&self, org: Uuid, actor: &TransferActor) -> anyhow::Result<Value>;
    async fn history(
        &self,
        org: Uuid,
        actor: &TransferActor,
        query: HistoryRequest,
    ) -> anyhow::Result<Value>;
    async fn configure_policy(
        &self,
        org: Uuid,
        update: Option<(OrganizationPolicy, Uuid, String)>,
    ) -> anyhow::Result<Value>;
    async fn reconciliation(&self, org: Uuid) -> anyhow::Result<Value>;
    async fn list_credentials(&self, org: Uuid, owner: Uuid) -> anyhow::Result<Value>;
    async fn save_credential(
        &self,
        org: Uuid,
        owner: Uuid,
        id: Option<Uuid>,
        request: &CredentialRequest,
    ) -> anyhow::Result<Value>;
    async fn revoke_credential(&self, org: Uuid, owner: Uuid, id: Uuid) -> anyhow::Result<Value>;
    async fn rotate_credential(&self, org: Uuid, owner: Uuid, id: Uuid) -> anyhow::Result<Value>;
}

pub struct OrganizationCreditService {
    transfers: Arc<dyn CreditTransferRepositoryPort>,
    credentials: Arc<dyn OrganizationCredentialRepositoryPort>,
}

impl OrganizationCreditService {
    pub fn new(
        transfers: Arc<dyn CreditTransferRepositoryPort>,
        credentials: Arc<dyn OrganizationCredentialRepositoryPort>,
    ) -> Self {
        Self {
            transfers,
            credentials,
        }
    }
}

#[async_trait]
impl OrganizationCreditServiceTrait for OrganizationCreditService {
    async fn transfer(
        &self,
        org: Uuid,
        actor: &TransferActor,
        key: &str,
        request: &TransferRequest,
    ) -> anyhow::Result<(Value, bool)> {
        request.validate(org, key.trim())?;

        self.transfers.transfer(org, actor, key, request).await
    }

    async fn capacity(&self, org: Uuid, actor: &TransferActor) -> anyhow::Result<Value> {
        self.transfers.capacity(org, actor).await
    }

    async fn history(
        &self,
        org: Uuid,
        actor: &TransferActor,
        query: HistoryRequest,
    ) -> anyhow::Result<Value> {
        query.validate()?;

        self.transfers.history(org, actor, query).await
    }

    async fn configure_policy(
        &self,
        org: Uuid,
        update: Option<(OrganizationPolicy, Uuid, String)>,
    ) -> anyhow::Result<Value> {
        if let Some((policy, _, reason)) = &update {
            policy.validate(reason)?;
        }

        self.transfers.configure_policy(org, update).await
    }

    async fn reconciliation(&self, org: Uuid) -> anyhow::Result<Value> {
        self.transfers.reconciliation(org).await
    }

    async fn list_credentials(&self, org: Uuid, owner: Uuid) -> anyhow::Result<Value> {
        self.credentials.list_credentials(org, owner).await
    }

    async fn save_credential(
        &self,
        org: Uuid,
        owner: Uuid,
        id: Option<Uuid>,
        request: &CredentialRequest,
    ) -> anyhow::Result<Value> {
        request.validate()?;

        self.credentials
            .save_credential(org, owner, id, request)
            .await
    }

    async fn revoke_credential(&self, org: Uuid, owner: Uuid, id: Uuid) -> anyhow::Result<Value> {
        self.credentials.revoke_credential(org, owner, id).await
    }

    async fn rotate_credential(&self, org: Uuid, owner: Uuid, id: Uuid) -> anyhow::Result<Value> {
        self.credentials.rotate_credential(org, owner, id).await
    }
}
