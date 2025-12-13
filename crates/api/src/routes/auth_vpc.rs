use crate::routes::api::AppState;
use axum::{
    extract::State,
    http::StatusCode,
    response::{IntoResponse, Json},
};
use serde::{Deserialize, Serialize};
use services::{
    auth::{ports::OAuthUserInfo, Session},
    organization::ports::Organization,
    workspace::ports::Workspace,
};

#[derive(Debug, Deserialize)]
pub struct VpcLoginRequest {
    pub timestamp: i64,
    pub signature: String,
    pub client_id: String,
}

impl VpcLoginRequest {
    pub fn validate(&self) -> Result<(), String> {
        // Basic sanity checks to avoid extremely large inputs
        if self.client_id.trim().is_empty() {
            return Err("client_id cannot be empty".to_string());
        }
        if self.client_id.len() > 255 {
            return Err("client_id is too long (max 255 characters)".to_string());
        }
        if self.signature.trim().is_empty() {
            return Err("signature cannot be empty".to_string());
        }
        if self.signature.len() > 4096 {
            return Err("signature is too long (max 4096 characters)".to_string());
        }
        Ok(())
    }
}

#[derive(Debug, Serialize, Deserialize)]
pub struct VpcLoginResponse {
    pub access_token: String,
    pub session: Session,
    pub refresh_token: String,
    pub api_key: String,
    pub organization: Organization,
    pub workspace: Workspace,
}

pub async fn vpc_login(
    State(state): State<AppState>,
    Json(payload): Json<VpcLoginRequest>,
) -> Result<impl IntoResponse, (StatusCode, String)> {
    // Basic payload validation to prevent obviously bad or oversized input
    payload
        .validate()
        .map_err(|e| (StatusCode::BAD_REQUEST, e))?;

    // Verify VPC signature
    let valid = state
        .attestation_service
        .verify_vpc_signature(payload.timestamp, payload.signature.clone())
        .await
        .map_err(|e| {
            tracing::error!("VPC signature verification failed: {e:?}");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                "Verification error".to_string(),
            )
        })?;

    if !valid {
        return Err((
            StatusCode::UNAUTHORIZED,
            "Invalid VPC signature".to_string(),
        ));
    }

    // Get or create Chat API user with deterministic mapping
    let provider_user_id = format!("vpc:{}", payload.client_id);
    let email = format!("{}@vpc.internal.near.ai", payload.client_id);
    let username = payload.client_id.clone();

    let user_info = OAuthUserInfo {
        provider: "vpc".to_string(),
        provider_user_id,
        email,
        username,
        display_name: None,
        avatar_url: None,
    };

    let user = state
        .auth_service
        .get_or_create_oauth_user(user_info)
        .await
        .map_err(|e| {
            tracing::error!("Failed to get/create Chat API user: {e:?}");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                "User creation error".to_string(),
            )
        })?;

    // Create session
    let (access_token, session, refresh_token) = state
        .auth_service
        .create_session(
            user.id.clone(),
            None,
            "VPC/1.0".to_string(),
            state.config.auth.encoding_key.clone(),
            24,
            24 * 30,
        )
        .await
        .map_err(|e| {
            tracing::error!("Failed to create session: {e:?}");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                "Session creation error".to_string(),
            )
        })?;

    // Create unbound API key for this session
    // 1. Get default organization for user
    let orgs = state
        .organization_service
        .list_organizations_for_user(user.id.clone(), 1, 0, None, None)
        .await
        .map_err(|e| {
            tracing::error!("Failed to list organizations for user: {e:?}");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                "Organization lookup error".to_string(),
            )
        })?;

    let org = orgs.first().ok_or_else(|| {
        tracing::error!("User has no organizations");
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            "No organization found".to_string(),
        )
    })?;

    // 2. Get default workspace for organization
    let workspaces = state
        .workspace_service
        .list_workspaces_for_organization(org.id.clone(), user.id.clone())
        .await
        .map_err(|e| {
            tracing::error!("Failed to list workspaces: {e:?}");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                "Workspace lookup error".to_string(),
            )
        })?;

    let workspace = workspaces.first().ok_or_else(|| {
        tracing::error!("Organization has no workspaces");
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            "No workspace found".to_string(),
        )
    })?;

    // 3. Create API key
    let api_key_name = format!("VPC Key - {}", payload.client_id);
    let api_key = state
        .workspace_service
        .create_api_key(services::workspace::ports::CreateApiKeyRequest {
            name: api_key_name,
            workspace_id: workspace.id.clone(),
            created_by_user_id: user.id,
            expires_at: None,  // Unbound expiry
            spend_limit: None, // Unbound spend limit
        })
        .await
        .map_err(|e| {
            tracing::error!("Failed to create API key: {e:?}");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                "API key creation error".to_string(),
            )
        })?;

    let key = api_key.key.ok_or_else(|| {
        tracing::error!("API key was created but key value is missing");
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            "API key creation error".to_string(),
        )
    })?;

    Ok(Json(VpcLoginResponse {
        access_token,
        session,
        refresh_token,
        api_key: key,
        organization: org.clone(),
        workspace: workspace.clone(),
    }))
}
