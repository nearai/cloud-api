use crate::models::ErrorResponse;
use axum::{extract::State, http::StatusCode, response::Json as ResponseJson};
use serde::{Deserialize, Serialize};
use services::usage::UsageServiceTrait;
use std::sync::Arc;
use utoipa::ToSchema;
use uuid::Uuid;

/// Billing costs request (HuggingFace compatible)
#[derive(Debug, Deserialize, ToSchema)]
pub struct BillingCostsRequest {
    /// Array of request IDs (inference IDs) to get costs for
    #[serde(rename = "requestIds")]
    pub request_ids: Vec<Uuid>,
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
}

/// State for billing routes
#[derive(Clone)]
pub struct BillingRouteState {
    pub usage_service: Arc<dyn UsageServiceTrait + Send + Sync>,
}

/// Get costs by request IDs
///
/// Returns the cost in nano-USD for each request ID provided.
/// This endpoint is designed for HuggingFace billing integration.
///
/// Request IDs that are not found are not included in the response.
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
    ResponseJson(request): ResponseJson<BillingCostsRequest>,
) -> Result<ResponseJson<BillingCostsResponse>, (StatusCode, ResponseJson<ErrorResponse>)> {
    tracing::debug!(
        "Billing costs request for {} inference IDs",
        request.request_ids.len()
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

    let costs = state
        .usage_service
        .get_costs_by_inference_ids(request.request_ids)
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

    let response = BillingCostsResponse {
        requests: costs
            .into_iter()
            .map(|c| RequestCost {
                request_id: c.inference_id,
                cost_nano_usd: c.cost_nano_usd,
            })
            .collect(),
    };

    Ok(ResponseJson(response))
}
