//! Named provider choice for chat completions (`docs/model-routes.md`).
//!
//! A model with routes lets a request name one of its providers in
//! `provider.only`. The request is then handled as a request for the route's
//! target catalog entry: this module rewrites the request before anything
//! else reads its model, so the target's own configuration, price, access
//! check and signature rules apply. Without `provider.only`, and for a model
//! without routes, the request is left exactly as it arrived.

use std::collections::HashMap;

use serde_json::Value;
use services::inference_provider_pool::ModelRoute;

use super::*;

/// Response header naming the provider a request chose. Absent when the
/// request named none.
pub(super) const HEADER_MODEL_PROVIDER: &str = "x-model-provider";

const PROVIDER: &str = "provider";
const ONLY: &str = "only";

/// The route a request named.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct NamedProvider {
    pub(super) route_id: Uuid,
    pub(super) provider: String,
    /// The catalog entry the request is sent as.
    pub(super) target: String,
}

/// The request's `provider.only`, when it names something. `null` and an
/// empty list name nothing, like a request without the field.
fn named_only(extra: &HashMap<String, Value>) -> Option<&Value> {
    extra
        .get(PROVIDER)?
        .get(ONLY)
        .filter(|only| !only.is_null() && only.as_array().is_none_or(|names| !names.is_empty()))
}

/// The route `only` names among `routes`, the routes of `model` the caller
/// can use: exactly one name, and one of theirs. Anything else is refused
/// with the names that would work, if any.
fn choose(model: &str, only: &Value, routes: &[ModelRoute]) -> Result<NamedProvider, String> {
    let named = match only.as_array().map(Vec::as_slice) {
        Some([name]) => name.as_str(),
        _ => None,
    };
    named
        .and_then(|name| routes.iter().find(|route| route.provider == name))
        .map(|route| NamedProvider {
            route_id: route.id,
            provider: route.provider.clone(),
            target: route.target.clone(),
        })
        .ok_or_else(|| {
            if routes.is_empty() {
                return format!(
                    "Model '{model}' has no provider that can be named in provider.only. \
                     Leave provider.only out for the default provider."
                );
            }
            let names: Vec<&str> = routes.iter().map(|route| route.provider.as_str()).collect();
            format!(
                "Model '{model}' has these providers: {}. Name exactly one of them in \
                 provider.only, or leave provider.only out for the default provider.",
                names.join(", ")
            )
        })
}

/// Take `provider.only` out of a request's extra fields; an emptied
/// `provider` object goes with it. The target must not receive a name that
/// only means something here.
fn remove_only(extra: &mut HashMap<String, Value>) {
    let emptied = extra
        .get_mut(PROVIDER)
        .and_then(Value::as_object_mut)
        .is_some_and(|provider| {
            provider.remove(ONLY);
            provider.is_empty()
        });
    if emptied {
        extra.remove(PROVIDER);
    }
}

/// The same rewrite on the request's original JSON, which a provider
/// adapter may read instead of the parsed request.
pub(super) fn rewrite_original(original: &mut Value, named: &NamedProvider) {
    let Some(body) = original.as_object_mut() else {
        return;
    };
    body.insert("model".to_string(), Value::String(named.target.clone()));
    let emptied = body
        .get_mut(PROVIDER)
        .and_then(Value::as_object_mut)
        .is_some_and(|provider| {
            provider.remove(ONLY);
            provider.is_empty()
        });
    if emptied {
        body.remove(PROVIDER);
    }
}

/// What a catalog lookup means for a request that names a provider.
#[derive(Debug, PartialEq, Eq)]
enum Lookup<T> {
    /// The caller may use the model.
    Usable(T),
    /// Unknown, or not this caller's to use.
    Missing,
    /// The catalog could not answer. The request must fail: carrying on
    /// would serve a named provider's request from the default one.
    Failed,
}

fn lookup<T>(result: Result<T, services::models::ModelsError>) -> Lookup<T> {
    match result {
        Ok(found) => Lookup::Usable(found),
        Err(services::models::ModelsError::NotFound(_)) => Lookup::Missing,
        Err(_) => Lookup::Failed,
    }
}

fn lookup_failed() -> Response {
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        ResponseJson(ErrorResponse::new(
            "Failed to resolve the provider named in provider.only".to_string(),
            "internal_server_error".to_string(),
        )),
    )
        .into_response()
}

fn refused(message: String) -> Response {
    (
        StatusCode::BAD_REQUEST,
        ResponseJson(ErrorResponse::new(
            message,
            "invalid_request_error".to_string(),
        )),
    )
        .into_response()
}

/// Apply the provider a request names. Returns the choice after pointing
/// `request` at its target, `None` when the request is left as it is, and
/// the response to return when the name cannot be honoured: a 400 for a
/// name the model does not offer, a 500 when the catalog cannot be read or
/// the routes have never loaded.
///
/// Left as it is: a request without `provider.only`, a model without routes,
/// and a model this caller cannot use (the completion service then answers
/// as for any unknown model). A model with routes never hands `provider.only`
/// to its own provider, also for a caller who can use none of them.
pub(super) async fn apply_provider_choice(
    app_state: &AppState,
    caller: ModelCaller,
    request: &mut ChatCompletionRequest,
) -> Result<Option<NamedProvider>, Response> {
    let pool = &app_state.inference_provider_pool;
    if named_only(&request.extra).is_none() {
        return Ok(None);
    }
    // Routes that have never loaded are unknown, not absent: carrying on
    // could serve a named provider's request from the default one.
    if pool.model_routes_unknown() {
        return Err(lookup_failed());
    }
    if !pool.has_model_routes() {
        return Ok(None);
    }
    let model = match lookup(
        app_state
            .models_service
            .resolve_and_get_model(&request.model, caller)
            .await,
    ) {
        Lookup::Usable(model) => model,
        Lookup::Missing => return Ok(None),
        Lookup::Failed => return Err(lookup_failed()),
    };
    let Some(routes) = pool.model_routes_of(&model.model_name) else {
        return Ok(None);
    };
    // Only the routes whose target this caller may request count: the
    // others, and their names, are not this caller's to learn about.
    let mut usable = Vec::with_capacity(routes.len());
    for route in routes.iter() {
        match lookup(
            app_state
                .models_service
                .resolve_and_get_model(&route.target, caller)
                .await,
        ) {
            Lookup::Usable(_) => usable.push(route.clone()),
            Lookup::Missing => {}
            Lookup::Failed => return Err(lookup_failed()),
        }
    }
    // An alias is never resolved silently, and a named provider replaces the
    // model the alias warning would describe: ask for the canonical name.
    if model.model_name != request.model {
        return Err(refused(format!(
            "Model '{}' is an alias of '{}'. Use the canonical model name '{}' to name a provider.",
            request.model, model.model_name, model.model_name
        )));
    }
    let Some(only) = named_only(&request.extra) else {
        return Ok(None);
    };
    let named = choose(&model.model_name, only, &usable).map_err(refused)?;
    tracing::info!(
        route_id = %named.route_id,
        "Chat request sent to the provider it named"
    );
    request.model = named.target.clone();
    remove_only(&mut request.extra);
    Ok(Some(named))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const MODEL: &str = "vendor/model";

    fn route(provider: &str, target: &str) -> ModelRoute {
        ModelRoute {
            id: Uuid::new_v4(),
            model: MODEL.to_string(),
            provider: provider.to_string(),
            target: target.to_string(),
        }
    }

    fn extra(provider: Value) -> HashMap<String, Value> {
        HashMap::from([(PROVIDER.to_string(), provider)])
    }

    #[test]
    fn one_known_name_picks_its_route() {
        let routes = [
            route("alpha", "vendor/via-alpha"),
            route("beta", "vendor/via-beta"),
        ];
        let named = choose(MODEL, &json!(["beta"]), &routes).expect("a known name");
        assert_eq!(
            named,
            NamedProvider {
                route_id: routes[1].id,
                provider: "beta".to_string(),
                target: "vendor/via-beta".to_string(),
            }
        );
    }

    #[test]
    fn anything_but_one_known_name_is_refused_with_the_valid_names() {
        let routes = [
            route("alpha", "vendor/via-alpha"),
            route("beta", "vendor/via-beta"),
        ];
        for only in [
            json!(["gamma"]),
            json!(["alpha", "beta"]),
            json!("alpha"),
            json!([7]),
            json!(["Alpha"]),
        ] {
            let message = choose(MODEL, &only, &routes).expect_err("refused");
            assert_eq!(
                message,
                "Model 'vendor/model' has these providers: alpha, beta. Name exactly one of them \
                 in provider.only, or leave provider.only out for the default provider.",
                "for {only}"
            );
        }
    }

    #[test]
    fn a_name_is_refused_without_names_when_no_route_is_usable() {
        let message = choose(MODEL, &json!(["alpha"]), &[]).expect_err("refused");
        assert_eq!(
            message,
            "Model 'vendor/model' has no provider that can be named in provider.only. Leave \
             provider.only out for the default provider."
        );
    }

    #[test]
    fn only_a_missing_model_lets_the_request_carry_on() {
        use services::models::ModelsError;
        assert_eq!(lookup(Ok::<_, ModelsError>(1)), Lookup::Usable(1));
        assert_eq!(
            lookup::<u8>(Err(ModelsError::NotFound("m".to_string()))),
            Lookup::Missing
        );
        for failure in [
            ModelsError::InternalError("down".to_string()),
            ModelsError::InvalidParams("bad".to_string()),
        ] {
            assert_eq!(lookup::<u8>(Err(failure)), Lookup::Failed);
        }
    }

    #[test]
    fn only_is_read_from_the_provider_object() {
        assert_eq!(named_only(&HashMap::new()), None);
        assert_eq!(named_only(&extra(json!({"sort": "price"}))), None);
        assert_eq!(named_only(&extra(json!("alpha"))), None);
        assert_eq!(named_only(&extra(json!({"only": null}))), None);
        assert_eq!(named_only(&extra(json!({"only": []}))), None);
        assert_eq!(
            named_only(&extra(json!({"only": ["alpha"]}))),
            Some(&json!(["alpha"]))
        );
    }

    #[test]
    fn removing_only_keeps_the_other_provider_fields() {
        let mut with_more = extra(json!({"only": ["alpha"], "zdr": true}));
        remove_only(&mut with_more);
        assert_eq!(with_more, extra(json!({"zdr": true})));

        let mut alone = extra(json!({"only": ["alpha"]}));
        alone.insert("seed".to_string(), json!(1));
        remove_only(&mut alone);
        assert_eq!(alone, HashMap::from([("seed".to_string(), json!(1))]));
    }

    #[test]
    fn the_original_request_gets_the_same_rewrite() {
        let named = NamedProvider {
            route_id: Uuid::new_v4(),
            provider: "alpha".to_string(),
            target: "vendor/via-alpha".to_string(),
        };

        let mut original = json!({
            "model": MODEL,
            "messages": [],
            "provider": {"only": ["alpha"], "zdr": true}
        });
        rewrite_original(&mut original, &named);
        assert_eq!(
            original,
            json!({"model": "vendor/via-alpha", "messages": [], "provider": {"zdr": true}})
        );

        let mut only_alone = json!({"model": MODEL, "provider": {"only": ["alpha"]}});
        rewrite_original(&mut only_alone, &named);
        assert_eq!(only_alone, json!({"model": "vendor/via-alpha"}));
    }
}
