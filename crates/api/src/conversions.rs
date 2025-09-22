use services::UserId;

use crate::{middleware::AuthenticatedUser, models::*};

impl From<&crate::models::Message> for services::ChatMessage {
    fn from(msg: &crate::models::Message) -> Self {
        Self {
            role: match msg.role.as_str() {
                "system" => services::MessageRole::System,
                "user" => services::MessageRole::User,
                "assistant" => services::MessageRole::Assistant,
                "tool" => services::MessageRole::Tool,
                _ => services::MessageRole::User, // Default to user for unknown roles
            },
            content: Some(msg.content.clone()),
            name: msg.name.clone(),
            tool_call_id: None,
            tool_calls: None,
        }
    }
}

impl From<&ChatCompletionRequest> for services::ChatCompletionParams {
    fn from(req: &ChatCompletionRequest) -> Self {
        Self {
            model: req.model.clone(),
            messages: req.messages.iter().map(|m| m.into()).collect(),
            max_tokens: req.max_tokens,
            temperature: req.temperature,
            top_p: req.top_p,
            stop: req.stop.clone(),
            stream: req.stream,
            tools: None, // TODO: Add tools support to API request
            max_completion_tokens: req.max_tokens,
            n: req.n,
            frequency_penalty: req.frequency_penalty,
            presence_penalty: req.presence_penalty,
            logit_bias: None,
            logprobs: None,
            top_logprobs: None,
            user: None,
            response_format: None,
            seed: None,
            tool_choice: None,
            parallel_tool_calls: None,
            metadata: None,
            store: None,
            stream_options: None,
        }
    }
}

impl From<&CompletionRequest> for services::CompletionParams {
    fn from(req: &CompletionRequest) -> Self {
        Self {
            model: req.model.clone(),
            prompt: req.prompt.clone(),
            max_tokens: req.max_tokens,
            temperature: req.temperature,
            top_p: req.top_p,
            n: req.n,
            stream: req.stream,
            stop: req.stop.clone(),
            frequency_penalty: req.frequency_penalty,
            presence_penalty: req.presence_penalty,
            logit_bias: None,
            logprobs: req.logprobs,
            echo: req.echo,
            best_of: req.best_of,
            seed: None,
            user: None,
            suffix: None,
            stream_options: None,
        }
    }
}

impl From<&services::ChatMessage> for crate::models::Message {
    fn from(msg: &services::ChatMessage) -> Self {
        Self {
            role: match msg.role {
                services::MessageRole::System => "system".to_string(),
                services::MessageRole::User => "user".to_string(),
                services::MessageRole::Assistant => "assistant".to_string(),
                services::MessageRole::Tool => "tool".to_string(),
            },
            content: msg.content.clone().unwrap_or_default(),
            name: msg.name.clone(),
        }
    }
}

fn finish_reason_to_string(reason: &services::FinishReason) -> String {
    match reason {
        services::FinishReason::Stop => "stop".to_string(),
        services::FinishReason::Length => "length".to_string(),
        services::FinishReason::ContentFilter => "content_filter".to_string(),
    }
}

impl From<&services::TokenUsage> for crate::models::Usage {
    fn from(usage: &services::TokenUsage) -> Self {
        Self {
            input_tokens: usage.prompt_tokens,
            input_tokens_details: Some(InputTokensDetails { cached_tokens: 0 }),
            output_tokens: usage.completion_tokens,
            output_tokens_details: Some(OutputTokensDetails {
                reasoning_tokens: 0,
            }),
            total_tokens: usage.total_tokens,
        }
    }
}

// Note: ChatCompletionResult and CompletionResult types no longer exist
// since the service only supports streaming. Response construction is handled
// directly in the route handlers by collecting stream events.

impl From<services::CompletionError> for crate::models::ErrorResponse {
    fn from(err: services::CompletionError) -> Self {
        match err {
            services::CompletionError::InvalidModel(msg) => ErrorResponse::with_param(
                msg,
                "invalid_request_error".to_string(),
                "model".to_string(),
            ),
            services::CompletionError::InvalidParams(msg) => {
                ErrorResponse::new(msg, "invalid_request_error".to_string())
            }
            services::CompletionError::RateLimitExceeded => ErrorResponse::new(
                "Rate limit exceeded".to_string(),
                "rate_limit_exceeded".to_string(),
            ),
            services::CompletionError::ProviderError(msg) => ErrorResponse::new(
                format!("Provider error: {}", msg),
                "provider_error".to_string(),
            ),
            services::CompletionError::InternalError(msg) => ErrorResponse::new(
                format!("Internal server error: {}", msg),
                "internal_error".to_string(),
            ),
        }
    }
}

pub fn generate_completion_id() -> String {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};
    use std::time::{SystemTime, UNIX_EPOCH};

    let mut hasher = DefaultHasher::new();
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_millis()
        .hash(&mut hasher);

    format!("{:x}", hasher.finish())
}

pub fn current_unix_timestamp() -> u64 {
    use std::time::{SystemTime, UNIX_EPOCH};

    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs()
}

// Organization-related conversions helper functions

/// Convert services Organization to API OrganizationResponse
pub fn services_org_to_api_org(
    org: services::organization::ports::Organization,
) -> crate::models::OrganizationResponse {
    crate::models::OrganizationResponse {
        id: org.id.0.to_string(),
        name: org.name,
        description: org.description,
        owner_id: org.owner_id.0.to_string(),
        settings: org.settings,
        is_active: org.is_active,
        created_at: org.created_at,
        updated_at: org.updated_at,
    }
}

/// Convert API CreateOrganizationRequest to services request
pub fn api_create_org_req_to_services(
    req: crate::models::CreateOrganizationRequest,
) -> services::organization::ports::CreateOrganizationRequest {
    services::organization::ports::CreateOrganizationRequest {
        name: req.name,
        display_name: req.display_name,
        description: req.description,
    }
}

/// Convert API UpdateOrganizationRequest to services request
pub fn api_update_org_req_to_services(
    req: crate::models::UpdateOrganizationRequest,
) -> services::organization::ports::UpdateOrganizationRequest {
    services::organization::ports::UpdateOrganizationRequest {
        display_name: req.display_name,
        description: req.description,
        rate_limit: req.rate_limit,
        settings: req.settings,
    }
}

// Legacy DB conversion functions - TODO: Remove once all routes migrated
pub fn db_create_org_req_to_services(
    req: database::CreateOrganizationRequest,
) -> services::organization::ports::CreateOrganizationRequest {
    services::organization::ports::CreateOrganizationRequest {
        name: req.name,
        display_name: Some(req.display_name),
        description: req.description,
    }
}

pub fn db_update_org_req_to_services(
    req: database::UpdateOrganizationRequest,
) -> services::organization::ports::UpdateOrganizationRequest {
    services::organization::ports::UpdateOrganizationRequest {
        display_name: req.display_name,
        description: req.description,
        rate_limit: req.rate_limit,
        settings: req.settings,
    }
}

/// Convert API MemberRole to services MemberRole
pub fn api_role_to_services_role(
    role: crate::models::MemberRole,
) -> services::organization::ports::MemberRole {
    match role {
        crate::models::MemberRole::Owner => services::organization::ports::MemberRole::Owner,
        crate::models::MemberRole::Admin => services::organization::ports::MemberRole::Admin,
        crate::models::MemberRole::Member => services::organization::ports::MemberRole::Member,
    }
}

/// Convert services MemberRole to API MemberRole
pub fn services_role_to_api_role(
    role: services::organization::ports::MemberRole,
) -> crate::models::MemberRole {
    match role {
        services::organization::ports::MemberRole::Owner => crate::models::MemberRole::Owner,
        services::organization::ports::MemberRole::Admin => crate::models::MemberRole::Admin,
        services::organization::ports::MemberRole::Member => crate::models::MemberRole::Member,
    }
}

// Legacy DB role conversion functions - TODO: Remove once all routes migrated
pub fn db_role_to_member_role(
    role: database::OrganizationRole,
) -> services::organization::ports::MemberRole {
    match role {
        database::OrganizationRole::Owner => services::organization::ports::MemberRole::Owner,
        database::OrganizationRole::Admin => services::organization::ports::MemberRole::Admin,
        database::OrganizationRole::Member => services::organization::ports::MemberRole::Member,
    }
}

pub fn member_role_to_db_role(
    role: services::organization::ports::MemberRole,
) -> database::OrganizationRole {
    match role {
        services::organization::ports::MemberRole::Owner => database::OrganizationRole::Owner,
        services::organization::ports::MemberRole::Admin => database::OrganizationRole::Admin,
        services::organization::ports::MemberRole::Member => database::OrganizationRole::Member,
    }
}

/// Convert API AddOrganizationMemberRequest to services request
pub fn api_add_member_req_to_services(
    req: crate::models::AddOrganizationMemberRequest,
    user_id: uuid::Uuid,
) -> services::organization::ports::AddOrganizationMemberRequest {
    services::organization::ports::AddOrganizationMemberRequest {
        user_id,
        role: api_role_to_services_role(req.role),
    }
}

/// Convert API UpdateOrganizationMemberRequest to services request
pub fn api_update_member_req_to_services(
    req: crate::models::UpdateOrganizationMemberRequest,
) -> services::organization::ports::UpdateOrganizationMemberRequest {
    services::organization::ports::UpdateOrganizationMemberRequest {
        role: api_role_to_services_role(req.role),
    }
}

// Legacy DB member conversion functions - TODO: Remove once all routes migrated
pub fn db_add_member_req_to_services(
    req: database::AddOrganizationMemberRequest,
) -> services::organization::ports::AddOrganizationMemberRequest {
    services::organization::ports::AddOrganizationMemberRequest {
        user_id: req.user_id,
        role: db_role_to_member_role(req.role),
    }
}

pub fn db_update_member_req_to_services(
    req: database::UpdateOrganizationMemberRequest,
) -> services::organization::ports::UpdateOrganizationMemberRequest {
    services::organization::ports::UpdateOrganizationMemberRequest {
        role: db_role_to_member_role(req.role),
    }
}

/// Convert services OrganizationMember to API response
pub fn services_member_to_api_member(
    member: services::organization::ports::OrganizationMember,
) -> crate::models::OrganizationMemberResponse {
    crate::models::OrganizationMemberResponse {
        organization_id: member.organization_id.0.to_string(),
        user_id: member.user_id.0.to_string(),
        role: services_role_to_api_role(member.role),
        joined_at: member.joined_at,
    }
}

// Legacy DB member conversion function - TODO: Remove once all routes migrated
pub fn services_member_to_db_member(
    member: services::organization::ports::OrganizationMember,
) -> database::OrganizationMember {
    database::OrganizationMember {
        id: uuid::Uuid::new_v4(), // Generate new ID for database
        organization_id: member.organization_id.0,
        user_id: member.user_id.0,
        role: member_role_to_db_role(member.role),
        joined_at: member.joined_at,
        invited_by: None, // Not available in ports model
    }
}

/// Convert API CreateApiKeyRequest to services request
pub fn api_key_req_to_services(
    req: crate::models::CreateApiKeyRequest,
    organization_id: services::organization::OrganizationId,
    created_by_user_id: services::UserId,
) -> services::auth::CreateApiKeyRequest {
    services::auth::CreateApiKeyRequest {
        name: req.name,
        account_type: req.account_type,
        expires_at: req.expires_at,
        organization_id: organization_id,
        created_by_user_id: created_by_user_id,
    }
}

/// Convert services ApiKey to API response
pub fn services_api_key_to_api_response(
    api_key: services::auth::ApiKey,
) -> crate::models::ApiKeyResponse {
    crate::models::ApiKeyResponse {
        id: api_key.id.0,                 // ApiKeyId contains a String
        name: Some(api_key.name.clone()), // Convert String to Option<String>
        key_prefix: format!("{}...", &api_key.name[..4.min(api_key.name.len())]), // Create key prefix from name
        organization_id: api_key.organization_id.0.to_string(),
        created_by_user_id: api_key.created_by_user_id.0.to_string(),
        account_type: api_key.account_type,
        created_at: api_key.created_at,
        last_used_at: api_key.last_used_at,
        expires_at: api_key.expires_at,
    }
}

/// Convert AuthenticatedUser to services UserId
pub fn authenticated_user_to_user_id(user: AuthenticatedUser) -> services::UserId {
    services::UserId(user.0.id)
}

/// Convert services User to API UserResponse
pub fn services_user_to_api_response(user: services::auth::User) -> crate::models::UserResponse {
    crate::models::UserResponse {
        id: user.id.0.to_string(), // UserId contains a Uuid
        email: user.email,
        username: Some(user.username), // Convert String to Option<String>
        display_name: user.display_name,
        avatar_url: user.avatar_url,
        created_at: user.created_at,
        last_login_at: user.last_login, // Field is called last_login, not last_login_at
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_message_conversion() {
        let http_msg = crate::models::Message {
            role: "user".to_string(),
            content: "Hello".to_string(),
            name: None,
        };

        let domain_msg: services::ChatMessage = (&http_msg).into();
        assert!(matches!(domain_msg.role, services::MessageRole::User));
        assert_eq!(domain_msg.content, Some("Hello".to_string()));

        let back_to_http: crate::models::Message = (&domain_msg).into();
        assert_eq!(back_to_http.role, "user");
        assert_eq!(back_to_http.content, "Hello");
    }

    #[test]
    fn test_chat_completion_request_conversion() {
        let http_req = ChatCompletionRequest {
            model: "gpt-3.5-turbo".to_string(),
            messages: vec![crate::models::Message {
                role: "user".to_string(),
                content: "Test message".to_string(),
                name: None,
            }],
            max_tokens: Some(100),
            temperature: Some(0.7),
            top_p: Some(1.0),
            n: Some(1),
            stream: None,
            stop: Some(vec!["\\n".to_string()]),
            presence_penalty: None,
            frequency_penalty: None,
        };

        let domain_params: services::ChatCompletionParams = (&http_req).into();
        assert_eq!(domain_params.model, "gpt-3.5-turbo");
        assert_eq!(domain_params.messages.len(), 1);
        assert_eq!(domain_params.max_tokens, Some(100));
        assert_eq!(domain_params.temperature, Some(0.7));
        assert_eq!(domain_params.stop, Some(vec!["\\n".to_string()]));
    }
}
