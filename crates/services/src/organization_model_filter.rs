//! The models an organization allows itself to use.
//!
//! Owners and admins set `allowed_models` in the organization's settings
//! (written through cloud-platform; this service has no write route). When it is set, the
//! organization may use only those models, on every route that resolves a
//! model, and `GET /v1/models` called with one of its API keys lists only
//! those. Every other model is answered exactly like a model that does not
//! exist. The filter only narrows: an unlisted model still needs a grant.
//! `null` allows every model again.
//!
//! The settings need a signed-in member, so a leaked API key cannot lift the
//! filter. Changes apply within the model-resolve cache TTL (30 s).

use std::collections::{BTreeSet, HashSet};
use std::sync::Arc;

use async_trait::async_trait;
use uuid::Uuid;

use crate::models::{ModelsError, ModelsServiceTrait};
use crate::organization::OrganizationRepository;

/// The most models one organization can list.
pub const MAX_ALLOWED_MODELS: usize = 256;

/// Where the models service reads an organization's filter.
#[async_trait]
pub trait OrganizationModelFilterSource: Send + Sync {
    /// The canonical names the organization allows, `None` when it allows
    /// every model. An error fails the request closed.
    async fn allowed_models(
        &self,
        organization_id: Uuid,
    ) -> anyhow::Result<Option<Arc<HashSet<String>>>>;
}

/// The filter stored in the organization's settings.
pub struct OrganizationSettingsModelFilter(pub Arc<dyn OrganizationRepository>);

#[async_trait]
impl OrganizationModelFilterSource for OrganizationSettingsModelFilter {
    async fn allowed_models(
        &self,
        organization_id: Uuid,
    ) -> anyhow::Result<Option<Arc<HashSet<String>>>> {
        // An organization that does not exist has no key to call with, so it
        // needs no filter.
        Ok(self
            .0
            .get_by_id(organization_id)
            .await?
            .and_then(|organization| organization.allowed_models())
            .map(Arc::new))
    }
}

/// Why a requested `allowed_models` list was refused.
#[derive(Debug, thiserror::Error)]
pub enum AllowedModelsError {
    #[error("allowed_models must name at least one model; send null to allow every model")]
    Empty,
    #[error("allowed_models accepts at most {MAX_ALLOWED_MODELS} models")]
    TooMany,
    #[error("Model '{0}' not found")]
    UnknownModel(String),
    #[error(transparent)]
    Models(#[from] ModelsError),
}

/// Check the names an organization asks to allow and return their canonical
/// names, sorted and without duplicates. Each must be the name or an alias of
/// an active model the organization could use without a filter: a listed
/// model, or an unlisted one it holds a grant for. The current filter is not
/// applied, so a PATCH can widen it. A name that fails is reported like an
/// unknown model, whether it is unknown or unlisted without a grant.
pub async fn canonical_allowed_models(
    models_service: &dyn ModelsServiceTrait,
    organization_id: Uuid,
    requested: &[String],
) -> Result<Vec<String>, AllowedModelsError> {
    if requested.is_empty() {
        return Err(AllowedModelsError::Empty);
    }
    if requested.len() > MAX_ALLOWED_MODELS {
        return Err(AllowedModelsError::TooMany);
    }
    let models = models_service.get_models_with_pricing().await?;
    let mut restricted = None;
    let mut canonical = BTreeSet::new();
    for name in requested {
        let model = models
            .iter()
            .find(|model| &model.model_name == name)
            .or_else(|| {
                models
                    .iter()
                    .find(|model| model.aliases.iter().any(|alias| alias == name))
            })
            .ok_or_else(|| AllowedModelsError::UnknownModel(name.clone()))?;
        if !model.is_listed {
            if restricted.is_none() {
                restricted = Some(models_service.get_restricted_model_access().await?);
            }
            let granted = restricted.iter().flatten().any(|access| {
                access.model_name == model.model_name
                    && access.organization_ids.contains(&organization_id)
            });
            if !granted {
                return Err(AllowedModelsError::UnknownModel(name.clone()));
            }
        }
        canonical.insert(model.model_name.clone());
    }
    Ok(canonical.into_iter().collect())
}
