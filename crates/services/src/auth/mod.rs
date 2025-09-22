pub mod oauth;
pub mod ports;
pub mod user_service;

pub use oauth::OAuthManager;
pub use ports::*;
use tracing::debug;
pub use user_service::UserServiceImpl;

use chrono::Utc;
use std::sync::Arc;
use uuid::Uuid;

use crate::organization::OrganizationRepository;

impl AuthService {
    pub fn new(
        user_repository: Arc<dyn UserRepository>,
        session_repository: Arc<dyn SessionRepository>,
        api_key_repository: Arc<dyn ApiKeyRepository>,
        organization_repository: Arc<dyn OrganizationRepository>,
    ) -> Self {
        Self {
            user_repository,
            session_repository,
            api_key_repository,
            organization_repository,
        }
    }

    /// Create a new session for a user
    pub async fn create_session(
        &self,
        user_id: UserId,
        ip_address: Option<String>,
        user_agent: Option<String>,
        expires_in_hours: i64,
    ) -> Result<(Session, String), AuthError> {
        self.session_repository
            .create(user_id, ip_address, user_agent, expires_in_hours)
            .await
            .map_err(|e| AuthError::InternalError(format!("Failed to create session: {}", e)))
    }

    /// Validate a session token and return the session
    pub async fn validate_session_token(
        &self,
        session_token: Uuid,
    ) -> Result<Option<Session>, AuthError> {
        self.session_repository
            .validate(session_token)
            .await
            .map_err(|e| AuthError::InternalError(format!("Failed to validate session: {}", e)))
    }

    /// Validate a session token and return the associated user
    pub async fn validate_session(&self, session_token: Uuid) -> Result<User, AuthError> {
        let session = self
            .validate_session_token(session_token)
            .await?
            .ok_or(AuthError::SessionNotFound)?;

        debug!("Session: {:?}", session);
        // Check if session is expired (validation already handles this, but keep for clarity)
        if session.expires_at < Utc::now() {
            return Err(AuthError::SessionNotFound);
        }

        // Get the user
        self.user_repository
            .get_by_id(session.user_id)
            .await
            .map_err(|e| AuthError::InternalError(format!("Failed to get user: {}", e)))?
            .ok_or(AuthError::UserNotFound)
    }

    /// Logout (revoke session)
    pub async fn logout(&self, session_id: SessionId) -> Result<bool, AuthError> {
        self.session_repository
            .revoke(session_id)
            .await
            .map_err(|e| AuthError::InternalError(format!("Failed to revoke session: {}", e)))
    }

    /// Get or create user from OAuth data
    pub async fn get_or_create_oauth_user(
        &self,
        oauth_info: OAuthUserInfo,
    ) -> Result<User, AuthError> {
        // Check if user already exists
        let existing_user = self
            .user_repository
            .get_by_email(&oauth_info.email)
            .await
            .map_err(|e| {
                AuthError::InternalError(format!("Failed to check existing user: {}", e))
            })?;

        if let Some(user) = existing_user {
            // Update last login
            self.user_repository
                .update_last_login(user.id.clone())
                .await
                .map_err(|e| {
                    AuthError::InternalError(format!("Failed to update last login: {}", e))
                })?;

            // Update user info if changed
            if user.display_name != oauth_info.display_name
                || user.avatar_url != oauth_info.avatar_url
            {
                self.user_repository
                    .update(
                        user.id.clone(),
                        oauth_info.display_name,
                        oauth_info.avatar_url,
                    )
                    .await
                    .map_err(|e| {
                        AuthError::InternalError(format!("Failed to update user: {}", e))
                    })?;
            }

            return Ok(user);
        }

        // Create new user
        self.user_repository
            .create_from_oauth(
                oauth_info.email,
                oauth_info.username,
                oauth_info.display_name,
                oauth_info.avatar_url,
                oauth_info.provider,
                oauth_info.provider_user_id,
            )
            .await
            .map_err(|e| AuthError::InternalError(format!("Failed to create user: {}", e)))
    }

    /// Clean up expired sessions
    pub async fn cleanup_expired_sessions(&self) -> Result<usize, AuthError> {
        self.session_repository
            .cleanup_expired()
            .await
            .map_err(|e| AuthError::InternalError(format!("Failed to cleanup sessions: {}", e)))
    }

    /// Validate an API key and return the associated user
    pub async fn validate_api_key(&self, api_key: String) -> Result<User, AuthError> {
        // Validate the API key
        let api_key_info = self
            .api_key_repository
            .validate(api_key)
            .await
            .map_err(|e| AuthError::InternalError(format!("Failed to validate API key: {}", e)))?
            .ok_or(AuthError::Unauthorized)?;

        // Check if the organization is active
        let organization = self
            .organization_repository
            .get_by_id(api_key_info.organization_id.0)
            .await
            .map_err(|e| AuthError::InternalError(format!("Failed to get organization: {}", e)))?
            .ok_or(AuthError::Unauthorized)?;

        if !organization.is_active {
            return Err(AuthError::Unauthorized);
        }

        // Get the user who created the API key
        self.user_repository
            .get_by_id(api_key_info.created_by_user_id)
            .await
            .map_err(|e| AuthError::InternalError(format!("Failed to get user: {}", e)))?
            .ok_or(AuthError::UserNotFound)
    }

    /// Create an API key for an organization with proper permission checking
    pub async fn create_organization_api_key(
        &self,
        organization_id: crate::organization::OrganizationId,
        requester_id: UserId,
        mut request: CreateApiKeyRequest,
    ) -> Result<ApiKey, AuthError> {
        // Check if requester has permission to create API keys for this organization
        let member = self
            .organization_repository
            .get_member(organization_id.0, requester_id.0)
            .await
            .map_err(|e| {
                AuthError::InternalError(format!("Failed to check organization membership: {}", e))
            })?
            .ok_or(AuthError::Unauthorized)?;

        // Check if the user has permission to manage API keys
        if !member.role.can_manage_api_keys() {
            return Err(AuthError::Unauthorized);
        }

        // Ensure the request has the correct organization_id and created_by_user_id
        request.organization_id = organization_id;
        request.created_by_user_id = requester_id;

        // Create the API key
        self.api_key_repository
            .create(request)
            .await
            .map_err(|e| AuthError::InternalError(format!("Failed to create API key: {}", e)))
    }

    /// Check if a user can manage API keys for an organization
    pub async fn can_manage_organization_api_keys(
        &self,
        organization_id: crate::organization::OrganizationId,
        user_id: UserId,
    ) -> Result<bool, AuthError> {
        // Check if user has permission to create API keys for this organization
        let member = self
            .organization_repository
            .get_member(organization_id.0, user_id.0)
            .await
            .map_err(|e| {
                AuthError::InternalError(format!("Failed to check organization membership: {}", e))
            })?;

        Ok(member.map_or(false, |m| m.role.can_manage_api_keys()))
    }

    /// List API keys for an organization with proper permission checking
    pub async fn list_organization_api_keys(
        &self,
        organization_id: crate::organization::OrganizationId,
        requester_id: UserId,
    ) -> Result<Vec<ApiKey>, AuthError> {
        // Check if requester is a member of the organization
        let _member = self
            .organization_repository
            .get_member(organization_id.0, requester_id.0)
            .await
            .map_err(|e| {
                AuthError::InternalError(format!("Failed to check organization membership: {}", e))
            })?
            .ok_or(AuthError::Unauthorized)?;

        // List API keys for the organization
        self.api_key_repository
            .list_by_organization(organization_id)
            .await
            .map_err(|e| AuthError::InternalError(format!("Failed to list API keys: {}", e)))
    }
}
