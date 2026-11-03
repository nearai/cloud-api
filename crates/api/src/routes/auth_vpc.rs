use crate::{
    consts::MAX_SIGNATURE_LENGTH,
    routes::{
        api::AppState,
        common::{validate_max_length, validate_non_empty_field},
    },
};
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

pub const VPC_AUTH_PROVIDER: &str = "vpc";
pub const MAX_VPC_CLIENT_ID_LENGTH: usize = 64;

fn validate_vpc_client_id(client_id: &str) -> Result<(), String> {
    if client_id.is_empty()
        || client_id.len() > MAX_VPC_CLIENT_ID_LENGTH
        || !client_id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'))
    {
        return Err(
            "client_id must be 1-64 characters of ASCII letters, digits, '.', '_' or '-'"
                .to_string(),
        );
    }
    Ok(())
}

#[derive(Debug, Deserialize)]
pub struct VpcLoginRequest {
    pub timestamp: i64,
    pub signature: String,
    pub client_id: String,
}

impl VpcLoginRequest {
    pub fn validate(&self) -> Result<(), String> {
        // Basic sanity checks to avoid extremely large inputs
        validate_vpc_client_id(&self.client_id)?;

        validate_non_empty_field(&self.signature, "signature")?;
        validate_max_length(&self.signature, "signature", MAX_SIGNATURE_LENGTH)?;
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
        email_verified: false,
        email_authoritative: false,
        provider: VPC_AUTH_PROVIDER.to_string(),
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
        .list_organizations_for_user(
            user.id.clone(),
            1,
            0,
            Some(services::organization::OrganizationOrderBy::JoinedAt),
            Some(services::organization::OrganizationOrderDirection::Asc),
        )
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_vpc_client_id_validation() {
        for client_id in ["chat-api-client", "a.z_9", &"a".repeat(64)] {
            let request = VpcLoginRequest {
                timestamp: 0,
                signature: "signature".to_string(),
                client_id: client_id.to_string(),
            };
            assert_eq!(request.validate(), Ok(()), "{client_id:?}");
        }

        for client_id in [
            "",
            "probe-i161413@near.ai",
            "a b",
            "a/b",
            "a\nb",
            "ä",
            &"a".repeat(65),
        ] {
            let request = VpcLoginRequest {
                timestamp: 0,
                signature: "signature".to_string(),
                client_id: client_id.to_string(),
            };
            assert_eq!(
                request.validate(),
                Err(
                    "client_id must be 1-64 characters of ASCII letters, digits, '.', '_' or '-'"
                        .to_string()
                ),
                "{client_id:?} must be rejected"
            );
        }
    }
}
