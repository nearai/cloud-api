//! Advertised route capabilities, not evidence that any request was verified.
use serde::{Deserialize, Serialize};
use uuid::Uuid;

#[derive(Clone, Debug, Serialize, Deserialize, utoipa::ToSchema)]
pub struct RoutingCapabilities {
    #[schema(value_type = String)]
    pub default_tee_requirement: inference_providers::routing::TeeRequirement,
    pub profiles: Vec<RouteCapabilities>,
}

#[derive(Clone, Debug, Serialize, Deserialize, utoipa::ToSchema)]
pub struct RouteCapabilities {
    /// Omitted for legacy routes that have not acquired registry identity.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub route_id: Option<Uuid>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub deployment_id: Option<Uuid>,
    /// Availability of a verifier-backed transport, not a verification result.
    pub tee_supported: bool,
    /// Null means unknown; an empty list means explicitly unsupported.
    pub reasoning_efforts: Option<Vec<String>>,
    pub native_responses: bool,
    pub native_anthropic: bool,
}
