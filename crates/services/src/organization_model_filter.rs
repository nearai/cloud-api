//! The models an organization allows itself to use.
//!
//! `allowed_models` is stored in the organization's settings. When it is set,
//! the organization may use only those models, on every route that resolves a
//! model, and `GET /v1/models` called with one of its API keys lists only
//! those. Every other model is answered exactly like a model that does not
//! exist. The filter only narrows: an unlisted model still needs a grant.
//! `null` allows every model again.
//!
//! A leaked API key cannot lift the filter. A change applies within the
//! model-resolve cache TTL (30 s).

use std::collections::HashSet;
use std::sync::Arc;

use async_trait::async_trait;
use uuid::Uuid;

use crate::organization::OrganizationRepository;

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
