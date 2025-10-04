use crate::models::*;
use utoipa::openapi::security::{ApiKey, ApiKeyValue, HttpAuthScheme, HttpBuilder, SecurityScheme};
use utoipa::{Modify, OpenApi};

/// OpenAPI documentation configuration
#[derive(OpenApi)]
#[openapi(
    info(
        title = "Platform API",
        description = "A comprehensive platform API for AI model inference, conversation management, and organization administration.\n\n## Authentication\n\nThis API supports two authentication methods:\n\n1. **Bearer Token (Session)**: Use `Authorization: Bearer <uuid>` with a session token\n2. **API Key**: Use `Authorization: Bearer sk_<key>` with an API key\n\nClick the **Authorize** button above to configure authentication.",
        version = "1.0.0",
        contact(
            name = "Platform API Team",
            email = "api-support@example.com"
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
        crate::routes::completions::quote,
        // Organization endpoints  
        crate::routes::organizations::list_organizations,
        crate::routes::organizations::create_organization,
        crate::routes::organizations::get_organization,
        crate::routes::organizations::update_organization,
        crate::routes::organizations::delete_organization,
        crate::routes::organizations::create_organization_api_key,
        crate::routes::organizations::list_organization_api_keys,
        // Conversation endpoints
        crate::routes::conversations::create_conversation,
        crate::routes::conversations::get_conversation,
        crate::routes::conversations::update_conversation,
        crate::routes::conversations::delete_conversation,
        crate::routes::conversations::list_conversations,
        crate::routes::conversations::list_conversation_items,
        // Response endpoints
        crate::routes::responses::create_response,
        // Attestation endpoints  
        crate::routes::attestation::get_signature,
        crate::routes::attestation::get_attestation_report,
        crate::routes::attestation::verify_attestation,
        // Model endpoints
        crate::routes::models::list_models,
        crate::routes::models::get_model_by_name,
        // Admin endpoints
        crate::routes::admin::batch_upsert_models,
        crate::routes::admin::get_model_pricing_history,
        crate::routes::admin::update_organization_limits,
    ),
    components(
        schemas(
            // Core API models
            ChatCompletionRequest, ChatCompletionResponse, Message,
            CompletionRequest, QuoteResponse, GatewayQuote, ServiceAllowlistEntry, BuildInfo,
            ModelsResponse, ModelInfo, ErrorResponse,
            // Organization models
            CreateOrganizationRequest, OrganizationResponse,
            UpdateOrganizationRequest, CreateApiKeyRequest, ApiKeyResponse,
            // Conversation models
            CreateConversationRequest, ConversationObject, ConversationList,
            UpdateConversationRequest, ConversationDeleteResult, ConversationItemList,
            // Response models
            CreateResponseRequest, ResponseObject,
            // Attestation models
            crate::routes::attestation::SignatureResponse,
            crate::routes::attestation::AttestationResponse,
            crate::routes::attestation::VerifyRequest,
            crate::routes::attestation::VerifyResponse,
            crate::routes::attestation::Evidence,
            crate::routes::attestation::NvidiaPayload,
            crate::routes::attestation::Attestation,
            // Model pricing models
            ModelListResponse, ModelWithPricing, DecimalPrice, ModelMetadata,
            UpdateModelApiRequest, ModelPricingHistoryEntry, ModelPricingHistoryResponse,
            // Organization limits models (Admin)
            UpdateOrganizationLimitsRequest, UpdateOrganizationLimitsResponse, SpendLimit,
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
            // Bearer token authentication (JWT/session tokens)
            components.add_security_scheme(
                "bearer",
                SecurityScheme::Http(
                    HttpBuilder::new()
                        .scheme(HttpAuthScheme::Bearer)
                        .bearer_format("JWT")
                        .build(),
                ),
            );
            // API Key authentication
            components.add_security_scheme(
                "api_key",
                SecurityScheme::ApiKey(ApiKey::Header(ApiKeyValue::new("X-API-Key"))),
            );
        }

        // Set global security requirement - endpoints need at least one of these
        openapi.security = Some(vec![
            // Allow Bearer token
            utoipa::openapi::security::SecurityRequirement::new("bearer", Vec::<String>::new()),
            // OR API key
            utoipa::openapi::security::SecurityRequirement::new("api_key", Vec::<String>::new()),
        ]);
    }
}

// Server URL will be determined dynamically on the client side
