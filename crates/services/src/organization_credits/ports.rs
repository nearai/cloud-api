use super::models::*;
use async_trait::async_trait;
use serde_json::Value;
use uuid::Uuid;

/// Atomic persistence operations. Adapters recheck authorization and constraints
/// against locked state before committing; callers cannot split a transfer.
#[cfg_attr(test, mockall::automock)]
#[async_trait]
pub trait CreditTransferRepositoryPort: Send + Sync {
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
}

/// Credential lifecycle persistence. Adapters check current owner authority and
/// the management policy under locks; rotation preserves identity and usage.
#[cfg_attr(test, mockall::automock)]
#[async_trait]
pub trait OrganizationCredentialRepositoryPort: Send + Sync {
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
