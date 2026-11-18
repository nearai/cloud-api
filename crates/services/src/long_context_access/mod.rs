//! Per-organization access to a model's long-context tier.
//!
//! Some models serve two context tiers behind one id (see
//! `inference_provider_pool::context_routing`). An organization may use the
//! long tier of such a model unless either:
//!
//! - the platform blocked it for that model (a row in
//!   `organization_model_long_context_blocks`), or
//! - the organization turned it off itself (`long_context_disabled_models` in
//!   its settings, see [`Organization::long_context_disabled_models`]).
//!
//! The provider pool enforces the answer (`ChatRoutingHints::long_context_allowed`).
//!
//! Both inputs are written elsewhere, so this module only reads them, cached
//! per organization for [`POLICY_TTL`]: a change applies within that time.
//! Each input falls back on its own: a failed or timed-out read keeps the
//! last value of that input read successfully for that organization, or else
//! its default (nothing blocked, nothing turned off). A degraded answer is
//! cached for the TTL like any other, so a failing read runs at most once per
//! organization per TTL.
//!
//! [`Organization::long_context_disabled_models`]: crate::organization::Organization::long_context_disabled_models

pub mod ports;

use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration;

use moka::future::Cache;
use uuid::Uuid;

use crate::organization::LongContextDisabledModels;
pub use ports::LongContextAccessRepository;

/// How long one organization's answer is reused.
const POLICY_TTL: Duration = Duration::from_secs(30);
/// How long the last successful read of each input is kept as the fallback
/// for a failed one.
const LAST_GOOD_TTL: Duration = Duration::from_secs(24 * 60 * 60);
/// Bound on each read, so a slow database cannot hold requests.
const LOAD_TIMEOUT: Duration = Duration::from_secs(2);
/// One entry per organization that sent a request within the TTL.
const CACHE_CAPACITY: u64 = 10_000;

/// One organization's long-context access inputs.
#[derive(Debug)]
struct OrganizationPolicy {
    /// Canonical names the platform blocked.
    blocked: Arc<HashSet<String>>,
    /// What the organization turned off itself.
    disabled: Arc<LongContextDisabledModels>,
}

impl OrganizationPolicy {
    fn allows(&self, model_name: &str) -> bool {
        !self.blocked.contains(model_name) && !self.disabled.contains(model_name)
    }
}

/// A cache of one input's last successful read per organization.
fn last_good<V: Clone + Send + Sync + 'static>() -> Cache<Uuid, V> {
    Cache::builder()
        .max_capacity(CACHE_CAPACITY)
        .time_to_live(LAST_GOOD_TTL)
        .build()
}

/// The value of a read, or the error kind of a failed one.
fn read_outcome<T>(
    result: Result<anyhow::Result<T>, tokio::time::error::Elapsed>,
) -> Result<T, &'static str> {
    match result {
        Ok(Ok(value)) => Ok(value),
        Ok(Err(_)) => Err("read_failed"),
        Err(_) => Err("timeout"),
    }
}

/// Answers whether an organization may use a model's long-context tier.
pub struct LongContextAccess {
    repository: Arc<dyn LongContextAccessRepository>,
    policies: Cache<Uuid, Arc<OrganizationPolicy>>,
    last_blocked: Cache<Uuid, Arc<HashSet<String>>>,
    last_disabled: Cache<Uuid, Arc<LongContextDisabledModels>>,
    load_timeout: Duration,
}

impl LongContextAccess {
    pub fn new(repository: Arc<dyn LongContextAccessRepository>) -> Self {
        Self::with_timing(repository, POLICY_TTL, LOAD_TIMEOUT)
    }

    fn with_timing(
        repository: Arc<dyn LongContextAccessRepository>,
        policy_ttl: Duration,
        load_timeout: Duration,
    ) -> Self {
        Self {
            repository,
            policies: Cache::builder()
                .max_capacity(CACHE_CAPACITY)
                .time_to_live(policy_ttl)
                .build(),
            last_blocked: last_good(),
            last_disabled: last_good(),
            load_timeout,
        }
    }

    /// Whether `organization_id` may use the long-context tier of
    /// `model_name` (canonical). Never fails: see the module docs for how a
    /// failed read is answered.
    pub async fn is_allowed(&self, organization_id: Uuid, model_name: &str) -> bool {
        // `get_with` runs one load per organization at a time; concurrent
        // misses wait for it.
        self.policies
            .get_with(organization_id, self.load(organization_id))
            .await
            .allows(model_name)
    }

    async fn load(&self, organization_id: Uuid) -> Arc<OrganizationPolicy> {
        let (blocked, settings) = tokio::join!(
            tokio::time::timeout(
                self.load_timeout,
                self.repository.blocked_model_names(organization_id)
            ),
            tokio::time::timeout(
                self.load_timeout,
                self.repository.organization_settings(organization_id)
            ),
        );

        let blocked = match read_outcome(blocked) {
            Ok(names) => {
                let blocked = Arc::new(names.into_iter().collect::<HashSet<_>>());
                self.last_blocked
                    .insert(organization_id, blocked.clone())
                    .await;
                blocked
            }
            Err(error_kind) => {
                tracing::warn!(
                    %organization_id,
                    error_kind,
                    "Failed to read long-context blocks; using the last known value"
                );
                self.last_blocked
                    .get(&organization_id)
                    .await
                    .unwrap_or_default()
            }
        };

        let disabled = match read_outcome(settings) {
            Ok(settings) => {
                let disabled = Arc::new(LongContextDisabledModels::from_settings(
                    organization_id,
                    settings.as_ref(),
                ));
                self.last_disabled
                    .insert(organization_id, disabled.clone())
                    .await;
                disabled
            }
            Err(error_kind) => {
                tracing::warn!(
                    %organization_id,
                    error_kind,
                    "Failed to read organization long-context settings; using the last known value"
                );
                self.last_disabled
                    .get(&organization_id)
                    .await
                    .unwrap_or_default()
            }
        };

        Arc::new(OrganizationPolicy { blocked, disabled })
    }
}

#[cfg(test)]
mod tests;
