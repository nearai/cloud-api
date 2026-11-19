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
//! Each input falls back on its own when a read fails or times out: it keeps
//! the last value of that input read successfully for that organization
//! (for up to [`LAST_GOOD_TTL`]), or else fails closed: a failed blocks read
//! blocks every model, a failed settings read turns every model off. A
//! fail-closed answer is cached for [`COLD_FALLBACK_TTL`] only; every other
//! answer for [`POLICY_TTL`]. One load runs per organization at a time, so a
//! failing database sees at most one read per organization per window. A
//! schema without the blocks table is a successful read with no blocks (the
//! repository's concern).
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
/// How long an answer is reused when a read failed and that input had no
/// last good value (the answer fails closed).
const COLD_FALLBACK_TTL: Duration = Duration::from_secs(2);
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
    /// Canonical names the platform blocked. `None`: unknown (the read
    /// failed with no last good value), so every model is blocked.
    blocked: Option<Arc<HashSet<String>>>,
    /// What the organization turned off itself (`All` when the read failed
    /// with no last good value).
    disabled: Arc<LongContextDisabledModels>,
    /// A read failed and that input had no last good value.
    cold_fallback: bool,
}

/// Expiry of a cached answer: [`COLD_FALLBACK_TTL`]-like for a cold
/// fallback, [`POLICY_TTL`]-like otherwise.
struct PolicyExpiry {
    ttl: Duration,
    cold_fallback_ttl: Duration,
}

impl moka::Expiry<Uuid, Arc<OrganizationPolicy>> for PolicyExpiry {
    fn expire_after_create(
        &self,
        _organization_id: &Uuid,
        policy: &Arc<OrganizationPolicy>,
        _created_at: std::time::Instant,
    ) -> Option<Duration> {
        Some(if policy.cold_fallback {
            self.cold_fallback_ttl
        } else {
            self.ttl
        })
    }
}

impl OrganizationPolicy {
    fn allows(&self, model_name: &str) -> bool {
        self.blocked
            .as_ref()
            .is_some_and(|blocked| !blocked.contains(model_name))
            && !self.disabled.contains(model_name)
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
        Self::with_timing(repository, POLICY_TTL, COLD_FALLBACK_TTL, LOAD_TIMEOUT)
    }

    fn with_timing(
        repository: Arc<dyn LongContextAccessRepository>,
        policy_ttl: Duration,
        cold_fallback_ttl: Duration,
        load_timeout: Duration,
    ) -> Self {
        Self {
            repository,
            policies: Cache::builder()
                .max_capacity(CACHE_CAPACITY)
                .expire_after(PolicyExpiry {
                    ttl: policy_ttl,
                    cold_fallback_ttl,
                })
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

        let mut cold_fallback = false;
        let blocked = match read_outcome(blocked) {
            Ok(names) => {
                let blocked = Arc::new(names.into_iter().collect::<HashSet<_>>());
                self.last_blocked
                    .insert(organization_id, blocked.clone())
                    .await;
                Some(blocked)
            }
            Err(error_kind) => {
                let last_good = self.last_blocked.get(&organization_id).await;
                cold_fallback |= last_good.is_none();
                tracing::warn!(
                    %organization_id,
                    error_kind,
                    has_last_good = last_good.is_some(),
                    "Failed to read long-context blocks; using the last known value, \
                     or blocking every model without one"
                );
                last_good
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
                let last_good = self.last_disabled.get(&organization_id).await;
                cold_fallback |= last_good.is_none();
                tracing::warn!(
                    %organization_id,
                    error_kind,
                    has_last_good = last_good.is_some(),
                    "Failed to read organization long-context settings; using the last known \
                     value, or turning every model off without one"
                );
                last_good.unwrap_or_else(|| Arc::new(LongContextDisabledModels::All))
            }
        };

        Arc::new(OrganizationPolicy {
            blocked,
            disabled,
            cold_fallback,
        })
    }
}

#[cfg(test)]
mod tests;
