use async_trait::async_trait;
use uuid::Uuid;

/// Where the long-context access check reads its two inputs.
#[async_trait]
pub trait LongContextAccessRepository: Send + Sync {
    /// Canonical names of the models whose long-context tier the platform
    /// blocked for `organization_id` (`organization_model_long_context_blocks`).
    /// A schema without that table has no blocks.
    async fn blocked_model_names(&self, organization_id: Uuid) -> anyhow::Result<Vec<String>>;

    /// The organization's settings: `None` when the organization does not
    /// exist or has none.
    async fn organization_settings(
        &self,
        organization_id: Uuid,
    ) -> anyhow::Result<Option<serde_json::Value>>;
}
