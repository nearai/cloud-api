use crate::middleware::auth::AuthenticatedApiKey;
use crate::models::ErrorResponse;
use axum::{
    extract::{Extension, Json, State},
    http::StatusCode,
    response::Json as ResponseJson,
};
use serde::{Deserialize, Serialize};
use services::usage::UsageServiceTrait;
use std::collections::HashMap;
use std::sync::Arc;
use utoipa::ToSchema;
use uuid::Uuid;

/// Where billing request IDs come from, referenced by every hint this route
/// returns. Kept in one place so the guidance cannot drift between the 400
/// and the not-found warning.
const REQUEST_ID_SOURCE_HINT: &str = "Request IDs are the UUIDs from the `inference-id` \
     response header returned by /v1/chat/completions and /v1/messages. The `x-request-id` \
     response header is a transport correlation ID and is not a billing request ID.";

/// Billing costs request (HuggingFace compatible)
#[derive(Debug, Deserialize, ToSchema)]
pub struct BillingCostsRequest {
    /// Array of request IDs to get costs for: the UUID values of the
    /// `inference-id` response header returned by /v1/chat/completions and
    /// /v1/messages (not the `x-request-id` header).
    #[serde(rename = "requestIds")]
    #[schema(value_type = Vec<Uuid>)]
    pub request_ids: Vec<String>,
}

/// Individual request cost
#[derive(Debug, Serialize, ToSchema)]
pub struct RequestCost {
    /// The request ID
    #[serde(rename = "requestId")]
    pub request_id: Uuid,
    /// Cost in nano-USD (10^-9 USD)
    #[serde(rename = "costNanoUsd")]
    pub cost_nano_usd: i64,
}

/// Billing costs response (HuggingFace compatible)
#[derive(Debug, Serialize, ToSchema)]
pub struct BillingCostsResponse {
    /// Array of request costs
    pub requests: Vec<RequestCost>,
    /// Present when some request IDs matched no usage record for this
    /// organization (those entries report costNanoUsd 0). Explains where the
    /// correct request IDs come from.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub warning: Option<String>,
}

/// State for billing routes
#[derive(Clone)]
pub struct BillingRouteState {
    pub usage_service: Arc<dyn UsageServiceTrait + Send + Sync>,
}

/// Get costs by request IDs
///
/// Returns the cost in nano-USD for each request ID provided. A request ID is
/// the UUID from the `inference-id` response header of /v1/chat/completions
/// and /v1/messages responses (equivalently: UUIDv5 of the response body `id`
/// under the DNS namespace). The `x-request-id` response header is a transport
/// correlation ID and cannot be used here.
///
/// Request IDs that are not found are returned with costNanoUsd: 0
/// (HuggingFace-compatible); the response then carries a `warning` field
/// pointing at the correct ID source. Usage for a just-finished request can
/// take a few seconds to become visible.
#[utoipa::path(
    post,
    path = "/v1/billing/costs",
    tag = "Billing",
    request_body = BillingCostsRequest,
    responses(
        (status = 200, description = "Costs retrieved successfully", body = BillingCostsResponse),
        (status = 400, description = "Invalid request", body = ErrorResponse),
        (status = 401, description = "Unauthorized", body = ErrorResponse),
        (status = 500, description = "Internal server error", body = ErrorResponse)
    ),
    security(
        ("api_key" = [])
    )
)]
pub async fn get_billing_costs(
    State(state): State<BillingRouteState>,
    Extension(api_key): Extension<AuthenticatedApiKey>,
    Json(request): Json<BillingCostsRequest>,
) -> Result<ResponseJson<BillingCostsResponse>, (StatusCode, ResponseJson<ErrorResponse>)> {
    tracing::debug!(
        "Billing costs request for {} inference IDs from organization: {}",
        request.request_ids.len(),
        api_key.organization.id
    );

    // Limit the number of request IDs to prevent abuse
    if request.request_ids.len() > 10000 {
        return Err((
            StatusCode::BAD_REQUEST,
            ResponseJson(ErrorResponse::new(
                "Maximum 10000 request IDs per request".to_string(),
                "invalid_request".to_string(),
            )),
        ));
    }

    // Parse leniently instead of deserializing straight into Uuid, so a
    // caller who submitted the wrong identifier (`x-request-id`, an Anthropic
    // `msg_...`/`req_...` id) gets pointed at the right one rather than a
    // bare deserialization error.
    let mut request_ids = Vec::with_capacity(request.request_ids.len());
    for raw in &request.request_ids {
        match Uuid::parse_str(raw) {
            Ok(id) => request_ids.push(id),
            Err(_) => {
                return Err((
                    StatusCode::BAD_REQUEST,
                    ResponseJson(ErrorResponse::new(
                        format!(
                            "requestIds entry '{}' is not a UUID. {REQUEST_ID_SOURCE_HINT}",
                            truncate_for_error(raw)
                        ),
                        "invalid_request".to_string(),
                    )),
                ));
            }
        }
    }

    let found_costs = state
        .usage_service
        .get_costs_by_inference_ids(api_key.organization.id.0, request_ids.clone())
        .await
        .map_err(|e| {
            tracing::error!("Failed to get billing costs: {}", e);
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                ResponseJson(ErrorResponse::new(
                    "Failed to retrieve billing costs".to_string(),
                    "internal_server_error".to_string(),
                )),
            )
        })?;
    let found_costs: HashMap<Uuid, i64> = found_costs
        .into_iter()
        .map(|c| (c.inference_id, c.cost_nano_usd))
        .collect();

    let mut not_found = 0usize;
    let requests: Vec<RequestCost> = request_ids
        .iter()
        .map(|id| {
            let cost_nano_usd = found_costs.get(id).copied().unwrap_or_else(|| {
                not_found += 1;
                0
            });
            RequestCost {
                request_id: *id,
                cost_nano_usd,
            }
        })
        .collect();

    let warning = (not_found > 0).then(|| {
        format!(
            "{not_found} of {} request IDs matched no usage record for this organization and \
             report costNanoUsd 0. {REQUEST_ID_SOURCE_HINT} Usage for a just-finished request \
             can take a few seconds to become visible.",
            requests.len()
        )
    });

    Ok(ResponseJson(BillingCostsResponse { requests, warning }))
}

/// Bound how much of a caller-supplied identifier is echoed back in an error.
fn truncate_for_error(raw: &str) -> String {
    const MAX: usize = 48;
    if raw.chars().count() <= MAX {
        raw.to_string()
    } else {
        let head: String = raw.chars().take(MAX).collect();
        format!("{head}…")
    }
}
