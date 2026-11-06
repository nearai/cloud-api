use crate::ohttp_gateway::{OhttpAttestation, OhttpGateway};
use services::{
    attestation::ports::AttestationServiceTrait, auth::AuthServiceTrait,
    completions::CompletionServiceTrait, files::FileServiceTrait, mcp::McpClientManager,
    models::ModelsServiceTrait, organization::OrganizationServiceTrait,
    workspace::WorkspaceServiceTrait,
};
use std::sync::Arc;

/// Application state shared across all route handlers
#[derive(Clone)]
pub struct AppState {
    pub organization_service: Arc<dyn OrganizationServiceTrait + Send + Sync>,
    pub workspace_service: Arc<dyn WorkspaceServiceTrait + Send + Sync>,
    pub mcp_manager: Arc<McpClientManager>,
    pub completion_service: Arc<dyn CompletionServiceTrait>,
    pub models_service: Arc<dyn ModelsServiceTrait>,
    pub auth_service: Arc<dyn AuthServiceTrait>,
    pub attestation_service: Arc<dyn AttestationServiceTrait>,
    pub usage_service: Arc<dyn services::usage::UsageServiceTrait + Send + Sync>,
    pub service_usage_service:
        Arc<dyn services::service_usage::ServiceUsageServiceTrait + Send + Sync>,
    pub user_service: Arc<dyn services::user::UserServiceTrait + Send + Sync>,
    pub contact_email_service: Arc<services::contact_email::ContactEmailService>,
    pub files_service: Arc<dyn FileServiceTrait + Send + Sync>,
    pub inference_provider_pool: Arc<services::inference_provider_pool::InferenceProviderPool>,
    pub metrics_service: Arc<dyn services::metrics::MetricsServiceTrait>,
    pub staking_farm_service: Arc<services::staking_farm::StakingFarmService>,
    pub aml_service: Arc<services::aml::AmlService>,
    pub config: Arc<config::ApiConfig>,
    /// OHTTP gateway for RFC 9458 decapsulation/encapsulation. `None` when OHTTP_ENABLED is unset.
    pub ohttp_gateway: Option<Arc<OhttpGateway>>,
    /// Pre-built attestation payload for the OHTTP key config; included in GET /v1/attestation/report.
    pub ohttp_attestation: Option<OhttpAttestation>,
    /// HTTP client used exclusively for OHTTP loopback requests to self.
    pub http_client: reqwest::Client,
}
