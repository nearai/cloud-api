use crate::middleware::{auth_middleware, AuthState};
use axum::{
    middleware::from_fn_with_state,
    routing::{delete, get, put},
    Router,
};
use services::{
    attestation::ports::AttestationServiceTrait, auth::AuthServiceTrait,
    completions::CompletionServiceTrait, mcp::McpClientManager, models::ModelsServiceTrait,
    organization::OrganizationServiceTrait, workspace::WorkspaceServiceTrait,
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
    pub user_service: Arc<dyn services::user::UserServiceTrait + Send + Sync>,
}

// Import route handlers
use crate::routes::{
    organization_members::*,
    organizations::*,
    users::{
        accept_invitation, decline_invitation, get_current_user, get_user_sessions,
        list_user_invitations, revoke_all_user_sessions, revoke_user_session,
        update_current_user_profile,
    },
};

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
        // Organization member management
        .route(
            "/{id}/members",
            get(list_organization_members).post(add_organization_member),
        )
        .route(
            "/{id}/members/invite-by-email",
            axum::routing::post(invite_organization_member_by_email),
        )
        .route(
            "/{id}/members/{user_id}",
            put(update_organization_member).delete(remove_organization_member),
        )
        // // MCP Connector management
        // .route(
        //     "/{id}/mcp-connectors",
        //     get(list_mcp_connectors).post(create_mcp_connector),
        // )
        // .route(
        //     "/{id}/mcp-connectors/{connector_id}",
        //     get(get_mcp_connector)
        //         .put(update_mcp_connector)
        //         .delete(delete_mcp_connector),
        // )
        // .route(
        //     "/{id}/mcp-connectors/{connector_id}/test",
        //     post(test_mcp_connector),
        // )
        // .route(
        //     "/{id}/mcp-connectors/{connector_id}/tools",
        //     get(list_mcp_tools),
        // )
        // .route(
        //     "/{id}/mcp-connectors/{connector_id}/tools/call",
        //     post(call_mcp_tool),
        // )
        // .route(
        //     "/{id}/mcp-connectors/{connector_id}/usage",
        //     get(get_mcp_usage_logs),
        // )
        // Usage tracking routes
        .route(
            "/{id}/usage/balance",
            get(crate::routes::usage::get_organization_balance),
        )
        .route(
            "/{id}/usage/history",
            get(crate::routes::usage::get_organization_usage_history),
        );

    // User routes
    let user_routes = Router::new()
        .route("/me", get(get_current_user))
        .route("/me/profile", put(update_current_user_profile))
        .route("/me/invitations", get(list_user_invitations))
        .route(
            "/me/invitations/{invitation_id}/accept",
            axum::routing::post(accept_invitation),
        )
        .route(
            "/me/invitations/{invitation_id}/decline",
            axum::routing::post(decline_invitation),
        )
        .route(
            "/me/sessions",
            get(get_user_sessions).delete(revoke_all_user_sessions),
        )
        .route("/me/sessions/{session_id}", delete(revoke_user_session));

    // Combine all routes with auth middleware
    Router::new()
        .nest("/organizations", org_routes)
        .nest("/users", user_routes)
        .with_state(app_state)
        .layer(from_fn_with_state(auth_state, auth_middleware))
}
