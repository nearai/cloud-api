use crate::models::*;
use utoipa::openapi::security::{HttpAuthScheme, HttpBuilder, SecurityScheme};
use utoipa::{Modify, OpenApi};

/// OpenAPI documentation configuration
#[derive(OpenApi)]
#[openapi(
    info(
        title = "NEAR AI Cloud API",
        description = "A comprehensive cloud API for AI model inference, conversation management, and organization administration.\n\n## Authentication\n\nThis API supports two authentication methods:\n\n1. **Session Token (User Authentication)**: Use `Authorization: Bearer <session_token>` with a session token obtained from OAuth login\n2. **API Key (Programmatic Access)**: Use `Authorization: Bearer sk_<api_key>` with an API key (prefix: `sk_`)\n\nClick the **Authorize** button above to configure authentication.",
        version = "1.0.0",
        contact(
            name = "NEAR AI Team",
            email = "support@near.ai"
        ),
        license(
            name = "MIT",
        )
    ),
    paths(
        // Chat completion endpoints
        crate::routes::completions::chat_completions,
        crate::routes::completions::completions,
        crate::routes::completions::models,
        // Organization endpoints  
        crate::routes::organizations::list_organizations,
        crate::routes::organizations::create_organization,
        crate::routes::organizations::get_organization,
        crate::routes::organizations::update_organization,
        crate::routes::organizations::delete_organization,
        // Conversation endpoints
        crate::routes::conversations::create_conversation,
        crate::routes::conversations::get_conversation,
        crate::routes::conversations::update_conversation,
        crate::routes::conversations::delete_conversation,
        crate::routes::conversations::list_conversation_items,
        // Response endpoints
        crate::routes::responses::create_response,
        // Attestation endpoints  
        crate::routes::attestation::get_signature,
        crate::routes::attestation::get_attestation_report,
        crate::routes::attestation::quote,
        // Model endpoints
        crate::routes::models::list_models,
        crate::routes::models::get_model_by_name,
        // Admin endpoints
        crate::routes::admin::batch_upsert_models,
        crate::routes::admin::delete_model,
        crate::routes::admin::get_model_pricing_history,
        crate::routes::admin::update_organization_limits,
        crate::routes::admin::get_organization_limits_history,
        crate::routes::admin::list_users,
        // Workspace endpoints
        crate::routes::workspaces::create_workspace,
        crate::routes::workspaces::list_organization_workspaces,
        crate::routes::workspaces::get_workspace,
        crate::routes::workspaces::update_workspace,
        crate::routes::workspaces::delete_workspace,
        crate::routes::workspaces::create_workspace_api_key,
        crate::routes::workspaces::list_workspace_api_keys,
        crate::routes::workspaces::revoke_workspace_api_key,
        crate::routes::workspaces::update_api_key_spend_limit,
        crate::routes::workspaces::update_workspace_api_key,
        // Organization Members endpoints
        crate::routes::organization_members::add_organization_member,
        crate::routes::organization_members::invite_organization_member_by_email,
        crate::routes::organization_members::update_organization_member,
        crate::routes::organization_members::remove_organization_member,
        crate::routes::organization_members::list_organization_members,
        // Users endpoints
        crate::routes::users::get_current_user,
        crate::routes::users::update_current_user_profile,
        crate::routes::users::get_user_sessions,
        crate::routes::users::revoke_user_session,
        crate::routes::users::revoke_all_user_sessions,
        crate::routes::users::list_user_invitations,
        crate::routes::users::accept_invitation,
        crate::routes::users::decline_invitation,
        // Invitation endpoints (token-based)
        crate::routes::users::get_invitation_by_token,
        crate::routes::users::accept_invitation_by_token,
        // Usage endpoints
        crate::routes::usage::get_organization_balance,
        crate::routes::usage::get_organization_usage_history,
        crate::routes::usage::get_api_key_usage_history,
    ),
    components(
        schemas(
            // Core API models
            ChatCompletionRequest, ChatCompletionResponse, Message,
            CompletionRequest, ModelsResponse, ModelInfo, ErrorResponse,
            // Organization models
            CreateOrganizationRequest, OrganizationResponse,
            UpdateOrganizationRequest, CreateApiKeyRequest, ApiKeyResponse,
            UpdateApiKeySpendLimitRequest, UpdateApiKeyRequest,
            // Workspace models
            crate::routes::workspaces::CreateWorkspaceRequest,
            crate::routes::workspaces::UpdateWorkspaceRequest,
            crate::routes::workspaces::WorkspaceResponse,
            // Organization Members models
            AddOrganizationMemberRequest,
            InvitationEntry,
            InviteOrganizationMemberByEmailRequest,
            InvitationResult,
            InviteOrganizationMemberByEmailResponse,
            UpdateOrganizationMemberRequest,
            OrganizationMemberResponse,
            PublicOrganizationMemberResponse,
            AdminOrganizationMemberResponse,
            MemberRole,
            // Organization Invitation models
            InvitationStatus,
            OrganizationInvitationResponse,
            OrganizationInvitationWithOrgResponse,
            AcceptInvitationResponse,
            // Users models
            UserResponse,
            SessionResponse,
            PublicUserResponse,
            AdminUserResponse,
            crate::routes::users::UpdateUserProfileRequest,
            // Conversation models
            CreateConversationRequest, ConversationObject,
            UpdateConversationRequest, ConversationDeleteResult, ConversationItemList,
            // Response models
            CreateResponseRequest, ResponseObject,
            // Attestation models
            crate::routes::attestation::SignatureResponse,
            crate::routes::attestation::AttestationResponse,
            crate::routes::attestation::VerifyRequest,
            crate::routes::attestation::Evidence,
            crate::routes::attestation::NvidiaPayload,
            crate::routes::attestation::Attestation,
            crate::routes::attestation::QuoteResponse,
            crate::routes::attestation::ErrorResponse,
            // Model pricing models
            ModelListResponse, ModelWithPricing, DecimalPrice, DecimalPriceRequest, ModelMetadata,
            UpdateModelApiRequest, ModelPricingHistoryEntry, ModelPricingHistoryResponse,
            // Organization limits models (Admin)
            UpdateOrganizationLimitsRequest, UpdateOrganizationLimitsResponse, SpendLimit, SpendLimitRequest,
            OrgLimitsHistoryEntry, OrgLimitsHistoryResponse,
            // User models (Admin)
            ListUsersResponse, AdminUserResponse,
            // Usage tracking models
            crate::routes::usage::OrganizationBalanceResponse,
            crate::routes::usage::UsageHistoryResponse,
            crate::routes::usage::UsageHistoryEntryResponse,
        ),
    ),
    modifiers(&SecurityAddon)
    // No servers - let client determine the URL dynamically
)]
pub struct ApiDoc;

/// Security configuration for OpenAPI
pub struct SecurityAddon;

impl Modify for SecurityAddon {
    fn modify(&self, openapi: &mut utoipa::openapi::OpenApi) {
        if let Some(components) = openapi.components.as_mut() {
            // Session Token authentication (User authentication via OAuth)
            components.add_security_scheme(
                "session_token",
                SecurityScheme::Http(
                    HttpBuilder::new()
                        .scheme(HttpAuthScheme::Bearer)
                        .bearer_format("session_token")
                        .description(Some("Session token obtained from OAuth login (Authorization: Bearer <session_token>)"))
                        .build(),
                ),
            );
            // API Key authentication (Programmatic access)
            components.add_security_scheme(
                "api_key",
                SecurityScheme::Http(
                    HttpBuilder::new()
                        .scheme(HttpAuthScheme::Bearer)
                        .bearer_format("api_key")
                        .description(Some(
                            "API key for programmatic access (Authorization: Bearer sk_<api_key>)",
                        ))
                        .build(),
                ),
            );
        }

        // Set global security requirement - endpoints need at least one of these
        openapi.security = Some(vec![
            // Allow Session Token
            utoipa::openapi::security::SecurityRequirement::new(
                "session_token",
                Vec::<String>::new(),
            ),
            // OR API key
            utoipa::openapi::security::SecurityRequirement::new("api_key", Vec::<String>::new()),
        ]);
    }
}

// Server URL will be determined dynamically on the client side
