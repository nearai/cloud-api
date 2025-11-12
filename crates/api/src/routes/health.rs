use axum::{http::StatusCode, response::Json as ResponseJson};
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

/// Health check response
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct HealthResponse {
    /// Service status
    pub status: String,
    /// Service version (optional)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
}

/// Health check endpoint
///
/// Returns the health status of the service.
/// This endpoint requires no authentication and is useful for monitoring and load balancers.
#[utoipa::path(
    get,
    path = "/v1/health",
    responses(
        (status = 200, description = "Service is healthy", body = HealthResponse),
    ),
    tag = "Health"
)]
pub async fn health_check() -> (StatusCode, ResponseJson<HealthResponse>) {
    (
        StatusCode::OK,
        ResponseJson(HealthResponse {
            status: "ok".to_string(),
            version: option_env!("CARGO_PKG_VERSION").map(|v| v.to_string()),
        }),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_health_check() {
        let (status, ResponseJson(response)) = health_check().await;

        assert_eq!(status, StatusCode::OK);
        assert_eq!(response.status, "ok");
    }
}
