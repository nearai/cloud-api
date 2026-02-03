//! Dev/testing routes for integration testing between Cloud API and RAG Service.
//! These endpoints are temporary and should not be deployed to production.

use axum::{extract::State, response::Json};
use serde::{Deserialize, Serialize};

/// State for dev routes - holds the RAG service base URL
#[derive(Clone)]
pub struct DevState {
    pub rag_service_base_url: Option<String>,
}

#[derive(Serialize)]
pub struct TestRagResponse {
    pub cloud_api: String,
    pub rag_service: String,
    pub rag_response: Option<RagPingResponse>,
    pub error: Option<String>,
}

#[derive(Deserialize, Serialize, Clone)]
pub struct RagPingResponse {
    pub service: String,
    pub status: String,
}

/// Test connectivity between Cloud API and RAG Service.
///
/// curl http://localhost:3000/v1/dev/test-rag
pub async fn test_rag_connectivity(
    State(state): State<DevState>,
) -> Json<TestRagResponse> {
    tracing::info!("dev_test_rag: testing RAG service connectivity");

    let Some(base_url) = &state.rag_service_base_url else {
        tracing::warn!("dev_test_rag: RAG_SERVICE_BASE_URL not configured");
        return Json(TestRagResponse {
            cloud_api: "ok".to_string(),
            rag_service: "not_configured".to_string(),
            rag_response: None,
            error: Some("RAG_SERVICE_BASE_URL not configured".to_string()),
        });
    };

    let url = format!("{}/dev/ping", base_url);
    tracing::info!(url = %url, "dev_test_rag: calling RAG service");

    let client = reqwest::Client::new();
    match client.get(&url).timeout(std::time::Duration::from_secs(5)).send().await {
        Ok(response) => {
            if response.status().is_success() {
                match response.json::<RagPingResponse>().await {
                    Ok(ping) => {
                        tracing::info!(
                            rag_service = %ping.service,
                            rag_status = %ping.status,
                            "dev_test_rag: RAG service connectivity confirmed"
                        );
                        Json(TestRagResponse {
                            cloud_api: "ok".to_string(),
                            rag_service: "ok".to_string(),
                            rag_response: Some(ping),
                            error: None,
                        })
                    }
                    Err(e) => {
                        tracing::error!(error = %e, "dev_test_rag: failed to parse RAG response");
                        Json(TestRagResponse {
                            cloud_api: "ok".to_string(),
                            rag_service: "error".to_string(),
                            rag_response: None,
                            error: Some(format!("Failed to parse response: {e}")),
                        })
                    }
                }
            } else {
                let status = response.status();
                let body = response.text().await.unwrap_or_default();
                tracing::error!(status = %status, "dev_test_rag: RAG service returned error");
                Json(TestRagResponse {
                    cloud_api: "ok".to_string(),
                    rag_service: "error".to_string(),
                    rag_response: None,
                    error: Some(format!("HTTP {status}: {body}")),
                })
            }
        }
        Err(e) => {
            tracing::error!(error = %e, "dev_test_rag: failed to connect to RAG service");
            Json(TestRagResponse {
                cloud_api: "ok".to_string(),
                rag_service: "unreachable".to_string(),
                rag_response: None,
                error: Some(format!("Connection failed: {e}")),
            })
        }
    }
}
