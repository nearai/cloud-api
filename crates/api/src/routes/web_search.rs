//! GET /v1/web/search — standalone web search endpoint (API Key auth, bills via services).

use crate::middleware::auth::AuthenticatedApiKey;
use crate::models::{ErrorResponse, WebSearchQueryParams, WebSearchResponse, WebSearchResultItem};
use axum::{
    extract::{Extension, Query, State},
    http::StatusCode,
    response::Json as ResponseJson,
};
use services::responses::tools::{WebSearchParams, WebSearchProviderTrait};
use services::service_usage::ports::SERVICE_NAME_WEB_SEARCH;
use services::service_usage::{ServiceUsageError, ServiceUsageService};
use std::sync::Arc;
use tracing::debug;
use uuid::Uuid;

#[derive(Clone)]
pub struct WebSearchRouteState {
    pub web_search_provider: Arc<dyn WebSearchProviderTrait>,
    pub service_usage_service: Arc<ServiceUsageService>,
}

/// GET /v1/web/search — proxy to Brave, record service usage. Returns 503 if web_search not configured.
#[utoipa::path(
    get,
    path = "/v1/web/search",
    tag = "Web Search",
    params(WebSearchQueryParams),
    responses(
        (status = 200, description = "Search results", body = WebSearchResponse),
        (status = 400, description = "Missing or invalid query", body = ErrorResponse),
        (status = 502, description = "Search provider error", body = ErrorResponse),
        (status = 503, description = "Web search service not configured", body = ErrorResponse),
    ),
    security(
        ("api_key" = [])
    )
)]
pub async fn get_web_search(
    State(state): State<WebSearchRouteState>,
    Extension(api_key): Extension<AuthenticatedApiKey>,
    Query(params): Query<WebSearchQueryParams>,
) -> Result<ResponseJson<WebSearchResponse>, (StatusCode, ResponseJson<crate::models::ErrorResponse>)>
{
    if params.q.trim().is_empty() {
        return Err((
            StatusCode::BAD_REQUEST,
            ResponseJson(crate::models::ErrorResponse::new(
                "Query parameter 'q' is required and cannot be empty".to_string(),
                "invalid_request".to_string(),
            )),
        ));
    }

    let search_params = WebSearchParams {
        query: params.q.clone(),
        country: params.country,
        search_lang: params.search_lang,
        ui_lang: params.ui_lang,
        count: params.count,
        offset: params.offset,
        safesearch: params.safesearch,
        freshness: params.freshness,
        text_decorations: params.text_decorations,
        spellcheck: params.spellcheck,
        units: params.units,
        extra_snippets: params.extra_snippets,
        summary: params.summary,
    };

    let results = state
        .web_search_provider
        .search(search_params)
        .await
        .map_err(|_e| {
            // Do not log error content; it may contain user query (privacy)
            debug!("Web search provider request failed");
            (
                StatusCode::BAD_GATEWAY,
                ResponseJson(crate::models::ErrorResponse::new(
                    "Web search request failed".to_string(),
                    "bad_gateway".to_string(),
                )),
            )
        })?;

    let result_count = results.len() as u32;
    let response = WebSearchResponse {
        query: params.q,
        result_count,
        results: results
            .into_iter()
            .map(|r| WebSearchResultItem {
                title: r.title,
                url: r.url,
                description: r.snippet,
                published: None,
                site_name: None,
            })
            .collect(),
    };

    let api_key_id = Uuid::parse_str(&api_key.api_key.id.0).map_err(|_| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            ResponseJson(crate::models::ErrorResponse::new(
                "Invalid API key id".to_string(),
                "internal_server_error".to_string(),
            )),
        )
    })?;
    if let Err(e) = state
        .service_usage_service
        .record_service_usage(
            api_key.organization.id.0,
            api_key.workspace.id.0,
            api_key_id,
            SERVICE_NAME_WEB_SEARCH,
            1,
            None,
        )
        .await
    {
        match &e {
            ServiceUsageError::ServiceNotFound(_) => {
                return Err((
                    StatusCode::SERVICE_UNAVAILABLE,
                    ResponseJson(crate::models::ErrorResponse::new(
                        "Web search is not configured".to_string(),
                        "service_unavailable".to_string(),
                    )),
                ));
            }
            ServiceUsageError::InternalError(_) | ServiceUsageError::CostOverflow => {
                return Err((
                    StatusCode::INTERNAL_SERVER_ERROR,
                    ResponseJson(crate::models::ErrorResponse::new(
                        "Failed to record usage".to_string(),
                        "internal_server_error".to_string(),
                    )),
                ));
            }
        }
    }

    Ok(ResponseJson(response))
}
