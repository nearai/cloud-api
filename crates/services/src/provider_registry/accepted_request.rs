//! Internal request-owned snapshots. Caller supplied correlation IDs are never
//! keys for budgets: unrelated requests cannot share or reset this state.
use super::{entities::RouteRuntime, ModelRoutingPolicy};
use crate::models::ModelWithPricing;
use std::{collections::HashMap, sync::Arc};
use uuid::Uuid;

pub(crate) struct AcceptedRequest {
    pub id: Uuid,
    pub model: ModelWithPricing,
    pub policy: Option<ModelRoutingPolicy>,
    pub routes: HashMap<Uuid, RouteRuntime>,
    pub deadline: Option<tokio::time::Instant>,
    pub budget: tokio::sync::Mutex<Budget>,
}

pub(crate) struct Budget {
    pub attempts: usize,
    pub reserved_cost: i64,
    pub cost_known: bool,
    // Accepted once; later tool input growth cannot raise the retry baseline.
    pub customer_cost: Option<i64>,
}

impl Default for Budget {
    fn default() -> Self {
        Self {
            attempts: 0,
            reserved_cost: 0,
            cost_known: true,
            customer_cost: None,
        }
    }
}

tokio::task_local! {
    pub(crate) static ACCEPTED: Option<Arc<AcceptedRequest>>;
}

pub(crate) fn for_model(name: &str) -> Option<Arc<AcceptedRequest>> {
    ACCEPTED
        .try_with(|accepted| {
            accepted
                .as_ref()
                .filter(|accepted| {
                    accepted.model.model_name == name
                        || accepted.model.aliases.iter().any(|alias| alias == name)
                })
                .cloned()
        })
        .ok()
        .flatten()
}
