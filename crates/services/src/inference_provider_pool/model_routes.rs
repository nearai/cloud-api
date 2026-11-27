//! Model routes: the named providers of a model.
//!
//! A route says that, under model `A`, the provider named `p` is catalog
//! entry `B`. The pool only holds the table. A request for `A` is served by
//! `A`'s own providers unless it names `p`; the caller of
//! [`InferenceProviderPool::model_routes_of`] then sends it as a request for
//! `B`, so `B`'s own configuration, price and checks apply. A route is never
//! a fallback: nothing here adds a provider to a model's candidates.
//!
//! Routes are loaded from [`ModelRoutesSource`] at startup and on every
//! provider refresh. Without a source there are no routes. A failed reload
//! keeps the last loaded table; until a first load succeeds the routes are
//! unknown ([`InferenceProviderPool::model_routes_unknown`]).

use std::collections::HashMap;
use std::sync::Arc;

use uuid::Uuid;

use super::InferenceProviderPool;

/// The most routes kept for one model; the rest are ignored.
pub(super) const MAX_ROUTES_PER_MODEL: usize = 8;

/// One enabled route, by canonical model names.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelRoute {
    pub id: Uuid,
    /// The model callers request.
    pub model: String,
    /// The name callers send to pick this route.
    pub provider: String,
    /// The catalog entry that serves the route.
    pub target: String,
}

/// Where the pool reads the enabled routes from.
#[async_trait::async_trait]
pub trait ModelRoutesSource: Send + Sync {
    async fn fetch_model_routes(&self) -> Result<Vec<ModelRoute>, String>;
}

/// The routes of each model, by provider name.
pub(super) type RouteTable = HashMap<String, Arc<[ModelRoute]>>;

/// A provider name callers can send and read back in a response header: 1 to
/// 32 lowercase letters, digits or `-`, not starting with `-`.
fn is_provider_name(name: &str) -> bool {
    (1..=32).contains(&name.len())
        && !name.starts_with('-')
        && name
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
}

/// Group the routes by model, ordered by provider name, and drop the ones
/// that cannot apply: a route to the model itself, a provider name outside
/// [`is_provider_name`], a second route with the same provider name, and
/// anything past [`MAX_ROUTES_PER_MODEL`].
pub(super) fn build_route_table(mut routes: Vec<ModelRoute>) -> RouteTable {
    routes.sort_by(|a, b| {
        (a.model.as_str(), a.provider.as_str(), a.id).cmp(&(
            b.model.as_str(),
            b.provider.as_str(),
            b.id,
        ))
    });
    let mut grouped: HashMap<String, Vec<ModelRoute>> = HashMap::new();
    for route in routes {
        if route.model == route.target || !is_provider_name(&route.provider) {
            continue;
        }
        let known = grouped.entry(route.model.clone()).or_default();
        if known.len() >= MAX_ROUTES_PER_MODEL
            || known.iter().any(|other| other.provider == route.provider)
        {
            continue;
        }
        known.push(route);
    }
    grouped
        .into_iter()
        .filter(|(_, routes)| !routes.is_empty())
        .map(|(model, routes)| (model, routes.into()))
        .collect()
}

impl InferenceProviderPool {
    /// Set where routes are read from. Set once at startup; a second call is
    /// ignored.
    pub fn set_model_routes_source(&self, source: Arc<dyn ModelRoutesSource>) {
        let _ = self.model_routes_source.set(source);
    }

    /// Replace the route table.
    pub fn set_model_routes(&self, routes: Vec<ModelRoute>) {
        self.model_routes
            .store(Some(Arc::new(build_route_table(routes))));
    }

    /// Reload the route table from the source. No source: nothing happens.
    pub async fn refresh_model_routes(&self) {
        let Some(source) = self.model_routes_source.get() else {
            return;
        };
        match source.fetch_model_routes().await {
            Ok(routes) => {
                let table = build_route_table(routes);
                tracing::info!(
                    models = table.len(),
                    routes = table.values().map(|routes| routes.len()).sum::<usize>(),
                    "Loaded model routes"
                );
                self.model_routes.store(Some(Arc::new(table)));
            }
            Err(error) => {
                tracing::warn!(
                    error_message_len = error.len(),
                    "Failed to load model routes; keeping the last loaded set"
                );
            }
        }
    }

    /// Whether routes are read from a source that has not answered yet. Which
    /// models have routes is then unknown, which is not the same as none.
    pub fn model_routes_unknown(&self) -> bool {
        self.model_routes_source.get().is_some() && self.model_routes.load().is_none()
    }

    /// Whether any model has a route.
    pub fn has_model_routes(&self) -> bool {
        self.model_routes
            .load()
            .as_ref()
            .is_some_and(|table| !table.is_empty())
    }

    /// The routes of `model` (canonical name), ordered by provider name.
    /// `None` when the model has none.
    pub fn model_routes_of(&self, model: &str) -> Option<Arc<[ModelRoute]>> {
        self.model_routes.load().as_ref()?.get(model).cloned()
    }
}
