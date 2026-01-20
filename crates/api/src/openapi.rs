use crate::models::*;
use utoipa::openapi::security::{HttpAuthScheme, HttpBuilder, SecurityScheme};
use utoipa::{Modify, OpenApi};

/// OpenAPI documentation configuration
#[derive(OpenApi)]
#[openapi(
    info(
        title = "NEAR AI Cloud API",
        description = "A comprehensive cloud API for AI model inference, conversation management, and organization administration.\n\n## Authentication\n\nThis API supports three authentication methods:\n\n1. **Access Token (JWT)**: Use `Authorization: Bearer <jwt_token>` with a short-lived JWT access token for most API endpoints. Obtain this by calling POST /users/me/access_tokens with a refresh token.\n2. **Refresh Token**: Use `Authorization: Bearer <refresh_token>` (prefix: `rt_`) only with POST /users/me/access_tokens to create new JWT access tokens. Obtained from OAuth login.\n3. **API Key (Programmatic Access)**: Use `Authorization: Bearer sk-<api_key>` with an API key (prefix: `sk-`)\n\nClick the **Authorize** button above to configure authentication.",
        version = "1.0.0",
        contact(
            name = "NEAR AI Team",
            email = "support@near.ai"
        ),
        license(
            name = "MIT",
        )
    ),
    tags(
        (name = "Chat", description = "Chat completion endpoints for AI model inference"),
        (name = "Images", description = "Image generation endpoints"),
        (name = "Models", description = "Public model catalog and information"),
        (name = "Conversations", description = "Conversation management"),
        (name = "Responses", description = "Response handling and streaming"),
        (name = "Organizations", description = "Organization management"),
        (name = "Organization Members", description = "Organization member and invitation management"),
        (name = "Workspaces", description = "Workspace and API key management"),
        (name = "Files", description = "File upload and management"),
        (name = "Users", description = "User profile and token management"),
        (name = "Invitations", description = "Token-based invitation handling"),
        (name = "Usage", description = "Usage tracking and billing information"),
        (name = "Billing", description = "Billing costs endpoint (HuggingFace integration)"),
        (name = "Health", description = "Health check endpoints"),
        (name = "Attestation", description = "Attestation and verification endpoints"),
        (name = "Admin", description = "Administrative endpoints (admin access required)"),
    ),
    paths(
        // Chat completion endpoints (most important for users)
        crate::routes::completions::chat_completions,
        crate::routes::completions::image_generations,
        // crate::routes::completions::completions,
        crate::routes::completions::models,
        // Model endpoints (public model catalog)
        crate::routes::models::list_models,
        crate::routes::models::get_model_by_name,
        // Conversation endpoints
        crate::routes::conversations::create_conversation,
        crate::routes::conversations::get_conversation,
        crate::routes::conversations::update_conversation,
        crate::routes::conversations::delete_conversation,
        crate::routes::conversations::pin_conversation,
        crate::routes::conversations::unpin_conversation,
        crate::routes::conversations::archive_conversation,
        crate::routes::conversations::unarchive_conversation,
        crate::routes::conversations::clone_conversation,
        crate::routes::conversations::list_conversation_items,
        crate::routes::conversations::create_conversation_items,
        // Response endpoints
        crate::routes::responses::create_response,
        crate::routes::responses::get_response,
        crate::routes::responses::delete_response,
        crate::routes::responses::cancel_response,
        crate::routes::responses::list_input_items,
        // Organization endpoints
        crate::routes::organizations::list_organizations,
        crate::routes::organizations::create_organization,
        crate::routes::organizations::get_organization,
        crate::routes::organizations::update_organization,
        crate::routes::organizations::delete_organization,
        // Organization Members endpoints
        crate::routes::organization_members::add_organization_member,
        crate::routes::organization_members::invite_organization_member_by_email,
        crate::routes::organization_members::update_organization_member,
        crate::routes::organization_members::remove_organization_member,
        crate::routes::organization_members::list_organization_members,
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
        // Files endpoints
        crate::routes::files::upload_file,
        crate::routes::files::list_files,
        crate::routes::files::get_file,
        crate::routes::files::delete_file,
        crate::routes::files::get_file_content,
        // Users endpoints
        crate::routes::users::get_current_user,
        crate::routes::users::update_current_user_profile,
        crate::routes::users::get_user_refresh_tokens,
        crate::routes::users::revoke_user_refresh_token,
        crate::routes::users::revoke_all_user_tokens,
        crate::routes::users::create_access_token,
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
        // Billing endpoints (HuggingFace integration)
        crate::routes::billing::get_billing_costs,
        // Admin endpoints (less frequently used)
        crate::routes::admin::list_models,
        crate::routes::admin::batch_upsert_models,
        crate::routes::admin::delete_model,
        crate::routes::admin::get_model_history,
        crate::routes::admin::update_organization_limits,
        crate::routes::admin::get_organization_limits_history,
        crate::routes::admin::update_organization_concurrent_limit,
        crate::routes::admin::get_organization_concurrent_limit,
        crate::routes::admin::get_organization_metrics,
        crate::routes::admin::get_platform_metrics,
        crate::routes::admin::get_organization_timeseries,
        crate::routes::admin::list_users,
        crate::routes::admin::create_admin_access_token,
        crate::routes::admin::list_admin_access_tokens,
        crate::routes::admin::delete_admin_access_token,
        // Health check endpoint
        crate::routes::health::health_check,
        // Attestation endpoints
        crate::routes::attestation::get_signature,
        crate::routes::attestation::get_attestation_report,
    ),
    components(
        schemas(
            // Health check models
            crate::routes::health::HealthResponse,
            // Core API models
            ChatCompletionRequest, ChatCompletionResponse, Message,
            CompletionRequest, ModelsResponse, ModelInfo, ModelPricing, ErrorResponse,
            // Image generation models
            ImageGenerationRequest, ImageGenerationResponse, ImageData,
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
            RefreshTokenResponse,
            AccessAndRefreshTokenResponse,
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
            crate::routes::attestation::QuoteResponse,
            crate::routes::attestation::ErrorResponse,
            // Model pricing models
            ModelListResponse, ModelWithPricing, AdminModelListResponse, AdminModelWithPricing,
            DecimalPrice, DecimalPriceRequest, ModelMetadata,
            UpdateModelApiRequest, ModelHistoryEntry, ModelHistoryResponse,
            // Organization limits models (Admin)
            UpdateOrganizationLimitsRequest, UpdateOrganizationLimitsResponse, SpendLimit, SpendLimitRequest,
            OrgLimitsHistoryEntry, OrgLimitsHistoryResponse,
            // Organization concurrent limit models (Admin)
            UpdateOrganizationConcurrentLimitRequest, UpdateOrganizationConcurrentLimitResponse,
            GetOrganizationConcurrentLimitResponse,
            // User models (Admin)
            ListUsersResponse, AdminUserResponse,
            // Admin access token models
            CreateAdminAccessTokenRequest, AdminAccessTokenResponse,
            // Usage tracking models
            crate::routes::usage::OrganizationBalanceResponse,
            crate::routes::usage::UsageHistoryResponse,
            crate::routes::usage::UsageHistoryEntryResponse,
            // Billing models (HuggingFace integration)
            crate::routes::billing::BillingCostsRequest,
            crate::routes::billing::BillingCostsResponse,
            crate::routes::billing::RequestCost,
            // File models
            FileUploadResponse, ExpiresAfter, FileListResponse, FileDeleteResponse,
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
            // Access Token (JWT) authentication - primary method for user authentication
            components.add_security_scheme(
                "session_token",
                SecurityScheme::Http(
                    HttpBuilder::new()
                        .scheme(HttpAuthScheme::Bearer)
                        .bearer_format("JWT")
                        .description(Some("JWT access token for user authentication (Authorization: Bearer <jwt_token>). Create via POST /users/me/access_tokens."))
                        .build(),
                ),
            );
            // Refresh Token authentication - only for creating access tokens
            components.add_security_scheme(
                "refresh_token",
                SecurityScheme::Http(
                    HttpBuilder::new()
                        .scheme(HttpAuthScheme::Bearer)
                        .bearer_format("refresh_token")
                        .description(Some("Long-lived refresh token from OAuth login (Authorization: Bearer rt_<token>). Use only with POST /users/me/access_tokens."))
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
                            "API key for programmatic access (Authorization: Bearer sk-<api_key>)",
                        ))
                        .build(),
                ),
            );
        }

        // Set global security requirement - endpoints need at least one of these
        openapi.security = Some(vec![
            // Allow JWT Access Token (primary for users)
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
