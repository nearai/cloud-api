use async_trait::async_trait;
use uuid::Uuid;

use crate::organization::LongContextDisabledModels;

/// What an organization turned off itself, derived from its
/// `long_context_disabled_models` setting.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LongContextDisabledPolicy {
    /// The stored value is malformed: every model is off.
    pub disabled_all: bool,
    /// Canonical names turned off. Empty when `disabled_all`.
    pub disabled_models: Vec<String>,
}

impl LongContextDisabledPolicy {
    /// Derive the policy from an organization's settings (`None`: no
    /// settings) with the organization parser.
    pub fn from_settings(organization_id: Uuid, settings: Option<&serde_json::Value>) -> Self {
        match LongContextDisabledModels::from_settings(organization_id, settings) {
            LongContextDisabledModels::None => Self::default(),
            LongContextDisabledModels::All => Self {
                disabled_all: true,
                disabled_models: Vec::new(),
            },
            LongContextDisabledModels::Models(names) => {
                let mut disabled_models: Vec<String> = names.into_iter().collect();
                disabled_models.sort();
                Self {
                    disabled_all: false,
                    disabled_models,
                }
            }
        }
    }

    pub(crate) fn into_disabled_models(self) -> LongContextDisabledModels {
        if self.disabled_all {
            LongContextDisabledModels::All
        } else if self.disabled_models.is_empty() {
            LongContextDisabledModels::None
        } else {
            LongContextDisabledModels::Models(self.disabled_models.into_iter().collect())
        }
    }
}

/// Where the long-context access check reads its two inputs.
#[async_trait]
pub trait LongContextAccessRepository: Send + Sync {
    /// Canonical names of the models whose long-context tier the platform
    /// blocked for `organization_id` (`organization_model_long_context_blocks`).
    /// A schema without that table has no blocks.
    async fn blocked_model_names(&self, organization_id: Uuid) -> anyhow::Result<Vec<String>>;

    /// What the organization turned off itself. An organization that does not
    /// exist or has no settings turns nothing off.
    async fn long_context_disabled(
        &self,
        organization_id: Uuid,
    ) -> anyhow::Result<LongContextDisabledPolicy>;
}
