use crate::middleware::{auth_middleware, AuthState};
use axum::{
    middleware::from_fn_with_state,
    routing::{delete, get, post, put},
    Router,
};
use database::Database;
use services::{
    auth::AuthServiceTrait,
    completions::CompletionService,
    mcp::McpClientManager,
    models::ModelsService,
    organization::{ports::OrganizationRepository, OrganizationService},
};
use std::sync::Arc;

/// Application state shared across all route handlers
#[derive(Clone)]
pub struct AppState {
    pub db: Arc<Database>, // Still need DB for now, other routes depend on it
    pub organization_service: Arc<OrganizationService>, // New clean service layer
    pub mcp_manager: Arc<McpClientManager>,
    pub completion_service: Arc<dyn CompletionService>,
    pub models_service: Arc<dyn ModelsService>,
    pub auth_service: Arc<dyn AuthServiceTrait>,
}

// Import route handlers
use crate::routes::{mcp_connectors::*, organization_members::*, organizations::*, users::*};

/// Build the complete API router with all management endpoints
pub fn build_management_router(app_state: AppState, auth_state: AuthState) -> Router {
    // Organization routes
    let org_routes = Router::new()
        .route("/", get(list_organizations).post(create_organization))
        .route(
            "/{id}",
            get(get_organization)
                .put(update_organization)
                .delete(delete_organization),
        )
        .route(
            "/{id}/api-keys",
            get(list_organization_api_keys).post(create_organization_api_key),
        )
        .route("/{id}/api-keys/{key_id}", delete(revoke_api_key))
        // Organization member management
        .route(
            "/{id}/members",
            get(list_organization_members).post(add_organization_member),
        )
        .route(
            "/{id}/members/{user_id}",
            put(update_organization_member).delete(remove_organization_member),
        )
        // MCP Connector management
        .route(
            "/{id}/mcp-connectors",
            get(list_mcp_connectors).post(create_mcp_connector),
        )
        .route(
            "/{id}/mcp-connectors/{connector_id}",
            get(get_mcp_connector)
                .put(update_mcp_connector)
                .delete(delete_mcp_connector),
        )
        .route(
            "/{id}/mcp-connectors/{connector_id}/test",
            post(test_mcp_connector),
        )
        .route(
            "/{id}/mcp-connectors/{connector_id}/tools",
            get(list_mcp_tools),
        )
        .route(
            "/{id}/mcp-connectors/{connector_id}/tools/call",
            post(call_mcp_tool),
        )
        .route(
            "/{id}/mcp-connectors/{connector_id}/usage",
            get(get_mcp_usage_logs),
        );

    // User routes
    let user_routes = Router::new()
        .route("/me", get(get_current_user))
        .route("/me/profile", put(update_current_user_profile))
        .route("/me/organizations", get(get_user_organizations))
        .route("/", get(list_users))
        .route("/{id}", get(get_user))
        .route("/{id}/organizations", get(get_user_organizations_by_id))
        .route(
            "/{id}/sessions",
            get(get_user_sessions).delete(revoke_all_user_sessions),
        )
        .route("/{id}/sessions/{session_id}", delete(revoke_user_session));

    // Combine all routes with auth middleware
    Router::new()
        .nest("/organizations", org_routes)
        .nest("/users", user_routes)
        .with_state(app_state)
        .layer(from_fn_with_state(auth_state, auth_middleware))
}

// Revoke an API key
pub async fn revoke_api_key(
    state: axum::extract::State<AppState>,
    user: axum::extract::Extension<crate::middleware::AuthenticatedUser>,
    axum::extract::Path((org_id, api_key_id)): axum::extract::Path<(uuid::Uuid, uuid::Uuid)>,
) -> Result<axum::http::StatusCode, axum::http::StatusCode> {
    use tracing::{debug, error};

    debug!(
        "Revoking API key: {} in organization: {} by user: {}",
        api_key_id, org_id, user.0 .0.id
    );

    // Get the API key to check ownership and validate it belongs to the specified organization
    match state.db.api_keys.get_by_id(api_key_id).await {
        Ok(Some(api_key)) => {
            // Validate that the API key belongs to the specified organization
            if api_key.organization_id != org_id {
                return Err(axum::http::StatusCode::NOT_FOUND);
            }

            // Check if user has permission to revoke this key
            // Must be the creator, org owner/admin
            if api_key.created_by_user_id != user.0 .0.id {
                // Check org membership
                match state
                    .db
                    .organizations
                    .get_member(org_id, user.0 .0.id)
                    .await
                {
                    Ok(Some(member)) => {
                        if !member.role.can_manage_api_keys() {
                            return Err(axum::http::StatusCode::FORBIDDEN);
                        }
                    }
                    Ok(None) => return Err(axum::http::StatusCode::FORBIDDEN),
                    Err(e) => {
                        error!("Failed to check organization membership: {}", e);
                        return Err(axum::http::StatusCode::INTERNAL_SERVER_ERROR);
                    }
                }
            }

            // Revoke the key
            match state.db.api_keys.revoke(api_key_id).await {
                Ok(true) => Ok(axum::http::StatusCode::NO_CONTENT),
                Ok(false) => Err(axum::http::StatusCode::NOT_FOUND),
                Err(e) => {
                    error!("Failed to revoke API key: {}", e);
                    Err(axum::http::StatusCode::INTERNAL_SERVER_ERROR)
                }
            }
        }
        Ok(None) => Err(axum::http::StatusCode::NOT_FOUND),
        Err(e) => {
            error!("Failed to get API key: {}", e);
            Err(axum::http::StatusCode::INTERNAL_SERVER_ERROR)
        }
    }
}
