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
}
