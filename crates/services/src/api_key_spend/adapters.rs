//! Adapters from existing components to the spend-cache ports.

use std::sync::Arc;

use async_trait::async_trait;
use uuid::Uuid;

use super::ports::ApiKeySpendSource;
use crate::usage::UsageRepository;

/// Reads spend through the usage repository's `get_api_key_spend`, unchanged.
pub struct UsageRepositorySpendSource(pub Arc<dyn UsageRepository>);

#[async_trait]
impl ApiKeySpendSource for UsageRepositorySpendSource {
    async fn api_key_spend(&self, api_key_id: Uuid) -> anyhow::Result<i64> {
        self.0.get_api_key_spend(api_key_id).await
    }
}
