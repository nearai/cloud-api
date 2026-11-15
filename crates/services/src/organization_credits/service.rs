use super::models::*;
use super::ports::CreditTransferRepositoryPort;
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
}

pub struct OrganizationCreditService {
    transfers: Arc<dyn CreditTransferRepositoryPort>,
}

impl OrganizationCreditService {
    pub fn new(transfers: Arc<dyn CreditTransferRepositoryPort>) -> Self {
        Self { transfers }
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
}
