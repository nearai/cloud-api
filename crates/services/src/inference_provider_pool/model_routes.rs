//! Model routes: extra providers for a model, each named by another catalog
//! entry.
//!
//! A route says that requests for model `A` may also be served by the
//! providers of catalog entry `B`. `B` keeps its own configuration, so a
//! route carries none: the pool resolves it per request against the live
//! provider map, and `B`'s refreshes, failures and removal apply to the route
//! with no second registry to keep in step.
//!
//! Rules:
//! - The model's own providers always lead. Route targets follow in route
//!   order (priority, then target name) and serve only when the providers
//!   ahead of them cannot.
//! - A route never changes which kind of provider may serve a model. It
//!   applies only when the model and the target are both served purely by
//!   non-attested providers; any other pairing is skipped.
//! - One hop: a target's own routes are never followed.
//! - A request that pins a signing key, or whose organization turned
//!   fallback off, gets no route targets.
//! - Only chat operations use routes (streaming and not); image, audio and
//!   the other operations use the model's own providers.
//!
//! Routes are loaded from [`ModelRoutesSource`] at startup and on every
//! provider refresh. Without a source the table stays empty and candidate
//! selection is unchanged. A failed reload keeps the last loaded table.

use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::Arc;

use inference_providers::ProviderTier;
use uuid::Uuid;

use super::{InferenceProviderPool, InferenceProviderTrait, ProviderMappings};

/// The most routes honoured for one model; the rest are ignored.
pub(super) const MAX_ROUTES_PER_MODEL: usize = 8;

/// One enabled route, by canonical model names.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelRoute {
    pub id: Uuid,
    /// The model callers request.
    pub model: String,
    /// The catalog entry whose providers serve the route.
    pub target: String,
    /// Lower is tried first.
    pub priority: i32,
}

/// Where the pool reads the enabled routes from.
#[async_trait::async_trait]
pub trait ModelRoutesSource: Send + Sync {
    async fn fetch_model_routes(&self) -> Result<Vec<ModelRoute>, String>;
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct RouteTarget {
    pub(super) id: Uuid,
    pub(super) target: String,
}

/// Route targets per model, in the order they are tried.
pub(super) type RouteTable = HashMap<String, Vec<RouteTarget>>;

/// A provider reached through a route, and its place in the route order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct RouteMark {
    pub(super) id: Uuid,
    /// 1 for the first route target, 2 for the second, and so on. The
    /// model's own providers rank 0.
    pub(super) rank: u8,
}

pub(super) fn provider_key(provider: &Arc<InferenceProviderTrait>) -> usize {
    Arc::as_ptr(provider) as *const () as usize
}

/// Order the routes of each model and drop the ones that can never apply: a
/// route to itself, a second route to the same target, and anything past
/// [`MAX_ROUTES_PER_MODEL`].
pub(super) fn build_route_table(mut routes: Vec<ModelRoute>) -> RouteTable {
    routes.sort_by(|a, b| {
        (a.model.as_str(), a.priority, a.target.as_str(), a.id).cmp(&(
            b.model.as_str(),
            b.priority,
            b.target.as_str(),
            b.id,
        ))
    });
    let mut table = RouteTable::new();
    for route in routes {
        if route.model == route.target {
            continue;
        }
        let targets = table.entry(route.model).or_default();
        if targets.len() >= MAX_ROUTES_PER_MODEL
            || targets.iter().any(|known| known.target == route.target)
        {
            continue;
        }
        targets.push(RouteTarget {
            id: route.id,
            target: route.target,
        });
    }
    table.retain(|_, targets| !targets.is_empty());
    table
}

fn all_non_attested(providers: &[Arc<InferenceProviderTrait>]) -> bool {
    !providers.is_empty()
        && providers
            .iter()
            .all(|provider| provider.tier() == ProviderTier::NonAttested)
}

/// Append the route targets' providers to `providers`, the model's eligible
/// own providers, and return the mark of each appended one, keyed by
/// provider. Appends nothing unless the model's own are all non-attested
/// (module docs).
pub(super) fn append_route_targets(
    providers: &mut Vec<Arc<InferenceProviderTrait>>,
    targets: Vec<(RouteMark, Vec<Arc<InferenceProviderTrait>>)>,
) -> HashMap<usize, RouteMark> {
    let mut marks = HashMap::new();
    if targets.is_empty() || !all_non_attested(providers) {
        return marks;
    }
    let mut seen: HashSet<usize> = providers.iter().map(provider_key).collect();
    for (mark, target_providers) in targets {
        for provider in target_providers {
            let key = provider_key(&provider);
            if seen.insert(key) {
                marks.insert(key, mark);
                providers.push(provider);
            }
        }
    }
    marks
}

/// How many chats [`RecentChatRoutes`] remembers. The header is read right
/// after the provider answers, so only the newest entries matter.
const RECENT_CHAT_ROUTES: usize = 10_000;

/// The route that served each of the most recent route-served chats.
#[derive(Default)]
pub(super) struct RecentChatRoutes {
    by_chat: HashMap<String, Uuid>,
    /// Insertion order, oldest first, for eviction.
    order: VecDeque<String>,
}

impl RecentChatRoutes {
    fn insert(&mut self, chat_id: &str, route_id: Uuid) {
        if self.by_chat.insert(chat_id.to_string(), route_id).is_none() {
            self.order.push_back(chat_id.to_string());
        }
        while self.order.len() > RECENT_CHAT_ROUTES {
            if let Some(oldest) = self.order.pop_front() {
                self.by_chat.remove(&oldest);
            }
        }
    }

    fn remove(&mut self, chat_id: &str) {
        if self.by_chat.remove(chat_id).is_some() {
            self.order.retain(|known| known != chat_id);
        }
    }
}

impl InferenceProviderPool {
    /// Set where routes are read from. Set once at startup; a second call is
    /// ignored.
    pub fn set_model_routes_source(&self, source: Arc<dyn ModelRoutesSource>) {
        let _ = self.model_routes_source.set(source);
    }

    /// Replace the route table.
    pub fn set_model_routes(&self, routes: Vec<ModelRoute>) {
        self.model_routes.store(Arc::new(build_route_table(routes)));
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
                    routes = table.values().map(Vec::len).sum::<usize>(),
                    "Loaded model routes"
                );
                self.model_routes.store(Arc::new(table));
            }
            Err(error) => {
                tracing::warn!(
                    error_message_len = error.len(),
                    "Failed to load model routes; keeping the last loaded set"
                );
            }
        }
    }

    /// The providers of each of `model_id`'s route targets, in route order,
    /// read from `mappings`. Empty unless every provider registered for the
    /// model (`own`) is non-attested. A target that is not registered, or
    /// that any attested provider serves, is left out.
    pub(super) fn route_targets(
        &self,
        model_id: &str,
        own: &[Arc<InferenceProviderTrait>],
        mappings: &ProviderMappings,
    ) -> Vec<(RouteMark, Vec<Arc<InferenceProviderTrait>>)> {
        let table = self.model_routes.load();
        let Some(targets) = table.get(model_id) else {
            return Vec::new();
        };
        if !all_non_attested(own) {
            return Vec::new();
        }
        targets
            .iter()
            .enumerate()
            .filter_map(|(index, target)| {
                let providers = mappings.model_to_providers.get(&target.target)?;
                all_non_attested(providers).then(|| {
                    (
                        RouteMark {
                            id: target.id,
                            rank: u8::try_from(index + 1).unwrap_or(u8::MAX),
                        },
                        providers.clone(),
                    )
                })
            })
            .collect()
    }

    /// Remember which route served `chat_id`; `None` when the model's own
    /// provider served it, which also forgets an earlier chat of the same id.
    pub(super) fn record_chat_route(&self, chat_id: &str, route_id: Option<Uuid>) {
        // An upstream that sends no id gives nothing to tell two chats apart.
        if chat_id.is_empty() {
            return;
        }
        let mut chat_routes = self.chat_routes.lock().unwrap_or_else(|e| e.into_inner());
        match route_id {
            Some(route_id) => chat_routes.insert(chat_id, route_id),
            None => chat_routes.remove(chat_id),
        }
    }

    /// The route whose target served `chat_id`. `None` when the model's own
    /// provider served it, and when the chat is unknown.
    pub fn serving_route_id(&self, chat_id: &str) -> Option<Uuid> {
        if chat_id.is_empty() {
            return None;
        }
        self.chat_routes
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .by_chat
            .get(chat_id)
            .copied()
    }
}
