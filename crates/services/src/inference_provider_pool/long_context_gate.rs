//! Enforcement of `ChatRoutingHints::long_context_allowed`: a request whose
//! organization may not use a model's long-context tier never reaches it.
//!
//! The tier boundary is the model's CONFIGURED base capacity: the catalog
//! row's `long_context` expansion ([`super::expand_inference_endpoints`]),
//! recorded per model whenever its inference URLs are loaded
//! ([`InferenceProviderPool::record_long_context_boundaries`]). It never
//! comes from the providers currently registered, so a base fleet that drops
//! out cannot make the model look single-tier. A NEAR provider whose declared
//! capacity is above the boundary is the long tier. Third-party fallbacks
//! never define or move the boundary and are never dropped. A model without
//! a configured long tier is unaffected by the flag.
//!
//! For a model with a long tier, a restricted request:
//! - is rejected before any provider call when its size estimate exceeds the
//!   boundary ([`context_routing::requirement`] for chat,
//!   [`context_routing::systemone_requirement`] for System One). It is never
//!   routed to the base tier instead.
//! - otherwise runs on a candidate list without the long tier, so neither a
//!   retry, nor the context-length-400 fall-through, nor the low-priority pin
//!   can reach it. With no base provider registered, it gets no provider.

use std::collections::HashMap;
use std::sync::Arc;

use inference_providers::{ChatCompletionParams, CompletionError, ProviderTier, SystemOneRequest};

use super::{context_routing, ChatRoutingHints, InferenceProviderPool, InferenceProviderTrait};

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
    /// Record the configured long-context boundary of every model in
    /// `models`, the `(model, inference_url, max_context)` entries of a load
    /// (each model with all its entries): the base capacity when its entries
    /// declare two or more capacities, otherwise none.
    pub(crate) fn record_long_context_boundaries(&self, models: &[(String, String, Option<u32>)]) {
        let mut capacities: HashMap<&str, Vec<Option<u32>>> = HashMap::new();
        for (model, _, capacity) in models {
            capacities
                .entry(model.as_str())
                .or_default()
                .push(*capacity);
        }
        let mut boundaries = self
            .long_context_boundaries
            .write()
            .unwrap_or_else(|e| e.into_inner());
        for (model, capacities) in capacities {
            match context_routing::base_capacity(capacities) {
                Some(boundary) => {
                    boundaries.insert(model.to_string(), boundary);
                }
                None => {
                    boundaries.remove(model);
                }
            }
        }
    }

    /// The configured long-context tier boundary of `model_id` (module
    /// docs), or `None` when the model has no long tier.
    pub fn long_context_base_capacity(&self, model_id: &str) -> Option<u32> {
        self.long_context_boundaries
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .get(model_id)
            .copied()
    }

    /// Remove the long tier from `providers` (module docs). Unchanged for a
    /// model without a long tier.
    pub(super) fn drop_long_context_tier(
        &self,
        model_id: &str,
        providers: Vec<Arc<InferenceProviderTrait>>,
    ) -> Vec<Arc<InferenceProviderTrait>> {
        let Some(boundary) = self.long_context_base_capacity(model_id) else {
            return providers;
        };
        let before = providers.len();
        let kept: Vec<_> = {
            let states = self
                .provider_load_state
                .read()
                .unwrap_or_else(|e| e.into_inner());
            providers
                .into_iter()
                .filter(|provider| {
                    let ptr = Arc::as_ptr(provider) as *const () as usize;
                    let capacity = states.get(&ptr).and_then(|state| state.max_context_tokens);
                    !is_long_tier(provider.tier() == ProviderTier::Near, capacity, boundary)
                })
                .collect()
        };
        // Numbers and the model name only.
        tracing::debug!(
            model_id = %model_id,
            base_capacity = boundary,
            dropped = before - kept.len(),
            "Excluded the long-context tier for this organization"
        );
        kept
    }

    /// The boundary of `model_id` when its organization may not use the long
    /// tier and the request's size (`required`, computed only then) is above
    /// it. `None` when the request may proceed.
    fn limit_exceeded(
        &self,
        model_id: &str,
        long_context_allowed: bool,
        required: impl FnOnce() -> u64,
    ) -> Option<u32> {
        if long_context_allowed {
            return None;
        }
        let boundary = self.long_context_base_capacity(model_id)?;
        (required() > u64::from(boundary)).then_some(boundary)
    }

    /// The limit a chat request exceeds when its organization may not use
    /// the model's long-context tier (`long_context_allowed == false`).
    /// `None` when the request may proceed.
    pub fn long_context_limit_exceeded(
        &self,
        params: &ChatCompletionParams,
        long_context_allowed: bool,
    ) -> Option<u32> {
        self.limit_exceeded(&params.model, long_context_allowed, || {
            context_routing::requirement(&context_routing::estimate_input(params))
        })
    }

    /// As [`Self::long_context_limit_exceeded`], for a System One request.
    pub fn systemone_long_context_limit_exceeded(
        &self,
        request: &SystemOneRequest,
        long_context_allowed: bool,
    ) -> Option<u32> {
        self.limit_exceeded(&request.model, long_context_allowed, || {
            context_routing::systemone_requirement(request)
        })
    }

    /// The error for a request above `limit`, returned before any provider
    /// call. Callers check first with the `*_limit_exceeded` methods to
    /// return a typed error; this guard keeps the pool from sending such a
    /// request to the base tier whatever the caller did.
    fn limit_error(model_id: &str, limit: u32) -> CompletionError {
        // Numbers and the model name only.
        tracing::info!(
            model_id = %model_id,
            limit,
            "Rejected a request above the organization's long-context limit"
        );
        CompletionError::HttpError {
            status_code: 400,
            message: limit_message(limit),
            is_external: false,
        }
    }

    /// Reject a chat request above its organization's long-context limit.
    pub(super) fn reject_above_long_context_limit(
        &self,
        params: &ChatCompletionParams,
        hints: &ChatRoutingHints,
    ) -> Result<(), CompletionError> {
        match self.long_context_limit_exceeded(params, hints.long_context_allowed) {
            None => Ok(()),
            Some(limit) => Err(Self::limit_error(&params.model, limit)),
        }
    }

    /// Reject a System One request above its organization's long-context
    /// limit.
    pub(super) fn reject_systemone_above_long_context_limit(
        &self,
        request: &SystemOneRequest,
        long_context_allowed: bool,
    ) -> Result<(), CompletionError> {
        match self.systemone_long_context_limit_exceeded(request, long_context_allowed) {
            None => Ok(()),
            Some(limit) => Err(Self::limit_error(&request.model, limit)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use config::ExternalProvidersConfig;

    #[test]
    fn only_near_providers_above_the_boundary_are_long_tier() {
        assert!(is_long_tier(true, Some(1_000_000), 100_000));
        assert!(!is_long_tier(true, Some(100_000), 100_000));
        assert!(!is_long_tier(true, None, 100_000));
        assert!(!is_long_tier(false, Some(1_000_000), 100_000));
    }

    #[test]
    fn boundaries_are_recorded_from_the_catalog_expansion() {
        let pool = InferenceProviderPool::new(None, ExternalProvidersConfig::default());
        let tiered = serde_json::json!({"long_context": {
            "inference_url": "https://m-long.example",
            "max_context_tokens": 1_000_000,
            "base_max_context_tokens": 100_000
        }});
        let mut models = super::super::expand_inference_endpoints(
            "org/tiered",
            "https://m.example",
            Some(1_000_000),
            Some(&tiered),
        );
        models.extend(super::super::expand_inference_endpoints(
            "org/single",
            "https://s.example",
            Some(262_144),
            None,
        ));
        pool.record_long_context_boundaries(&models);
        assert_eq!(pool.long_context_base_capacity("org/tiered"), Some(100_000));
        assert_eq!(pool.long_context_base_capacity("org/single"), None);
        assert_eq!(pool.long_context_base_capacity("org/unknown"), None);

        // A later load without the long tier clears the boundary; a load of
        // other models leaves it alone.
        pool.record_long_context_boundaries(&super::super::expand_inference_endpoints(
            "org/other",
            "https://o.example",
            Some(8_192),
            None,
        ));
        assert_eq!(pool.long_context_base_capacity("org/tiered"), Some(100_000));
        pool.record_long_context_boundaries(&super::super::expand_inference_endpoints(
            "org/tiered",
            "https://m.example",
            Some(1_000_000),
            None,
        ));
        assert_eq!(pool.long_context_base_capacity("org/tiered"), None);
    }
}
