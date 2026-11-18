//! Enforcement of `ChatRoutingHints::long_context_allowed`: a request whose
//! organization may not use a model's long-context tier never reaches it.
//!
//! The tier boundary is [`context_routing::base_capacity`] over the declared
//! capacities of the model's NEAR providers, taken from the model's whole
//! provider set (never a narrowed candidate list, so pinning one signing key
//! cannot hide the boundary). A NEAR provider whose declared capacity is
//! above it is the long tier. A third-party fallback's window never creates
//! or moves the boundary, and third-party fallbacks are never dropped. A
//! model with fewer than two declared NEAR capacities has no long tier, and
//! the flag changes nothing for it.
//!
//! For a model with a long tier, a restricted request:
//! - is rejected before any provider call when its prompt estimate
//!   ([`context_routing::requirement`]) exceeds the boundary. It is never
//!   routed to the base tier instead.
//! - otherwise runs on a candidate list without the long tier, so neither a
//!   retry, nor the context-length-400 fall-through, nor the low-priority pin
//!   can reach it.

use std::sync::Arc;

use inference_providers::{ChatCompletionParams, CompletionError, ProviderTier};

use super::{context_routing, ChatRoutingHints, InferenceProviderPool, InferenceProviderTrait};

/// The tier boundary for providers given as `(is NEAR, declared capacity)`:
/// `None` when the model has no long-context tier.
fn boundary(providers: impl IntoIterator<Item = (bool, Option<u32>)>) -> Option<u32> {
    context_routing::base_capacity(
        providers
            .into_iter()
            .filter(|(is_near, _)| *is_near)
            .map(|(_, capacity)| capacity),
    )
}

/// Whether a provider is in the long tier of a model whose boundary is
/// `boundary`.
fn is_long_tier(is_near: bool, capacity: Option<u32>, boundary: u32) -> bool {
    is_near && capacity.is_some_and(|capacity| capacity > boundary)
}

/// The client-facing message for a request above the limit.
pub fn limit_message(limit: u32) -> String {
    format!(
        "This request needs more context than this model allows for your organization \
         (limit: {limit} tokens)."
    )
}

impl InferenceProviderPool {
    /// `(is NEAR, declared capacity)` of each provider, in order.
    fn tiers_and_capacities(
        &self,
        providers: &[Arc<InferenceProviderTrait>],
    ) -> Vec<(bool, Option<u32>)> {
        let states = self
            .provider_load_state
            .read()
            .unwrap_or_else(|e| e.into_inner());
        providers
            .iter()
            .map(|provider| {
                let ptr = Arc::as_ptr(provider) as *const () as usize;
                (
                    provider.tier() == ProviderTier::Near,
                    states.get(&ptr).and_then(|state| state.max_context_tokens),
                )
            })
            .collect()
    }

    /// The long-context tier boundary of `model_id` (module docs), or `None`
    /// when the model has no long tier or is unknown.
    pub async fn long_context_base_capacity(&self, model_id: &str) -> Option<u32> {
        let providers = self
            .provider_mappings
            .read()
            .await
            .model_to_providers
            .get(model_id)?
            .clone();
        boundary(self.tiers_and_capacities(&providers))
    }

    /// Remove the long tier from `providers`, which must be the model's whole
    /// provider set (module docs). Unchanged for a model without a long tier.
    pub(super) fn drop_long_context_tier(
        &self,
        model_id: &str,
        providers: Vec<Arc<InferenceProviderTrait>>,
    ) -> Vec<Arc<InferenceProviderTrait>> {
        let tiers = self.tiers_and_capacities(&providers);
        let Some(boundary) = boundary(tiers.iter().copied()) else {
            return providers;
        };
        let before = providers.len();
        let kept: Vec<_> = providers
            .into_iter()
            .zip(tiers)
            .filter(|(_, (is_near, capacity))| !is_long_tier(*is_near, *capacity, boundary))
            .map(|(provider, _)| provider)
            .collect();
        // Numbers and the model name only.
        tracing::debug!(
            model_id = %model_id,
            base_capacity = boundary,
            dropped = before - kept.len(),
            "Excluded the long-context tier for this organization"
        );
        kept
    }

    /// The limit `params` exceeds when its organization may not use the
    /// model's long-context tier (`long_context_allowed == false`): the tier
    /// boundary, when the prompt estimate is above it. `None` when the
    /// request may proceed.
    pub async fn long_context_limit_exceeded(
        &self,
        params: &ChatCompletionParams,
        long_context_allowed: bool,
    ) -> Option<u32> {
        if long_context_allowed {
            return None;
        }
        let boundary = self.long_context_base_capacity(&params.model).await?;
        let required = context_routing::requirement(&context_routing::estimate_input(params));
        (required > u64::from(boundary)).then_some(boundary)
    }

    /// Reject, before any provider call, a request above its organization's
    /// long-context limit. Callers check first with
    /// [`Self::long_context_limit_exceeded`] to return a typed error; this
    /// guard keeps the pool from sending such a request to the base fleet
    /// whatever the caller did.
    pub(super) async fn reject_above_long_context_limit(
        &self,
        params: &ChatCompletionParams,
        hints: &ChatRoutingHints,
    ) -> Result<(), CompletionError> {
        match self
            .long_context_limit_exceeded(params, hints.long_context_allowed)
            .await
        {
            None => Ok(()),
            Some(limit) => {
                // Numbers and the model name only.
                tracing::info!(
                    model_id = %params.model,
                    limit,
                    "Rejected a request above the organization's long-context limit"
                );
                Err(CompletionError::HttpError {
                    status_code: 400,
                    message: limit_message(limit),
                    is_external: false,
                })
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn boundary_comes_from_near_capacities_only() {
        // Two NEAR tiers: the smaller one is the boundary.
        assert_eq!(
            boundary([(true, Some(100_000)), (true, Some(1_000_000))]),
            Some(100_000)
        );
        // A third-party fallback with a smaller window does not make a
        // single-tier NEAR model tiered...
        assert_eq!(
            boundary([(true, Some(262_144)), (false, Some(65_536))]),
            None
        );
        // ...and does not move the boundary of a tiered one.
        assert_eq!(
            boundary([
                (true, Some(100_000)),
                (true, Some(1_000_000)),
                (false, Some(32_768))
            ]),
            Some(100_000)
        );
        // Undeclared capacities create no tier.
        assert_eq!(boundary([(true, Some(100_000)), (true, None)]), None);
    }

    #[test]
    fn only_near_providers_above_the_boundary_are_long_tier() {
        assert!(is_long_tier(true, Some(1_000_000), 100_000));
        assert!(!is_long_tier(true, Some(100_000), 100_000));
        assert!(!is_long_tier(true, None, 100_000));
        assert!(!is_long_tier(false, Some(1_000_000), 100_000));
    }
}
