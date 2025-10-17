pub mod oauth;
pub mod ports;

pub use oauth::OAuthManager;
pub use ports::*;
use tracing::debug;

use chrono::Utc;
use std::sync::Arc;

use crate::organization::OrganizationRepository;
use crate::workspace::{ApiKey, ApiKeyRepository, WorkspaceId, WorkspaceRepository};
use async_trait::async_trait;

#[async_trait]
impl AuthServiceTrait for AuthService {
    async fn create_session(
        &self,
        user_id: UserId,
        ip_address: Option<String>,
        user_agent: Option<String>,
        expires_in_hours: i64,
    ) -> Result<(Session, String), AuthError> {
        self.session_repository
            .create(user_id, ip_address, user_agent, expires_in_hours)
            .await
            .map_err(|e| AuthError::InternalError(format!("Failed to create session: {e}")))
    }

    async fn validate_session_token(
        &self,
        session_token: SessionToken,
    ) -> Result<Option<Session>, AuthError> {
        self.session_repository
            .validate(session_token)
            .await
            .map_err(|e| AuthError::InternalError(format!("Failed to validate session: {e}")))
    }

    async fn validate_session(&self, session_token: SessionToken) -> Result<User, AuthError> {
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
            .map_err(|e| AuthError::InternalError(format!("Failed to get user: {e}")))?
            .ok_or(AuthError::UserNotFound)
    }

    async fn get_user_by_id(&self, user_id: UserId) -> Result<User, AuthError> {
        self.user_repository
            .get_by_id(user_id)
            .await
            .map_err(|e| AuthError::InternalError(format!("Failed to get user: {e}")))?
            .ok_or(AuthError::UserNotFound)
    }

    async fn logout(&self, session_id: SessionId) -> Result<bool, AuthError> {
        self.session_repository
            .revoke(session_id)
            .await
            .map_err(|e| AuthError::InternalError(format!("Failed to revoke session: {e}")))
    }

    async fn get_or_create_oauth_user(&self, oauth_info: OAuthUserInfo) -> Result<User, AuthError> {
        use crate::organization::OrganizationId;
        use rand::Rng;

        // Check if user already exists
        let existing_user = self
            .user_repository
            .get_by_email(&oauth_info.email)
            .await
            .map_err(|e| AuthError::InternalError(format!("Failed to check existing user: {e}")))?;

        if let Some(user) = existing_user {
            // Update last login
            self.user_repository
                .update_last_login(user.id.clone())
                .await
                .map_err(|e| {
                    AuthError::InternalError(format!("Failed to update last login: {e}"))
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
                    .map_err(|e| AuthError::InternalError(format!("Failed to update user: {e}")))?;
            }

            return Ok(user);
        }

        // Create new user
        let new_user = self
            .user_repository
            .create_from_oauth(
                oauth_info.email.clone(),
                oauth_info.username.clone(),
                oauth_info.display_name.clone(),
                oauth_info.avatar_url.clone(),
                oauth_info.provider.clone(),
                oauth_info.provider_user_id.clone(),
            )
            .await
            .map_err(|e| AuthError::InternalError(format!("Failed to create user: {e}")))?;

        // Create default organization and workspace for new user
        debug!(
            "Creating default organization and workspace for new user: {}",
            new_user.email
        );

        // Generate organization name from user email with random suffix
        let org_name = {
            let username = oauth_info.email.split('@').next().unwrap_or("user");
            const CHARSET: &[u8] = b"abcdefghijklmnopqrstuvwxyz0123456789";
            let mut rng = rand::thread_rng();
            let suffix: String = (0..4)
                .map(|_| {
                    let idx = rng.gen_range(0..CHARSET.len());
                    CHARSET[idx] as char
                })
                .collect();
            format!("{username}-org-{suffix}")
        }; // rng is dropped here

        // Create organization
        match self
            .organization_service
            .create_organization(org_name.clone(), None, new_user.id.clone())
            .await
        {
            Ok(organization) => {
                debug!(
                    "Created default organization: {} for user: {}",
                    organization.id.0, new_user.email
                );

                // Create default workspace
                let workspace_result = self
                    .workspace_repository
                    .create(
                        "default".to_string(),
                        "default".to_string(),
                        Some(format!("Default workspace for {org_name}")),
                        OrganizationId(organization.id.0),
                        new_user.id.clone(),
                    )
                    .await;

                match workspace_result {
                    Ok(workspace) => {
                        debug!(
                            "Created default workspace: {} for user: {}",
                            workspace.id.0, new_user.email
                        );
                    }
                    Err(e) => {
                        // Log error but don't fail user creation
                        tracing::error!(
                            "Failed to create default workspace for new user {}: {}",
                            new_user.email,
                            e
                        );
                    }
                }
            }
            Err(e) => {
                // Log error but don't fail user creation
                tracing::error!(
                    "Failed to create default organization for new user {}: {}",
                    new_user.email,
                    e
                );
            }
        }

        Ok(new_user)
    }

    async fn cleanup_expired_sessions(&self) -> Result<usize, AuthError> {
        self.session_repository
            .cleanup_expired()
            .await
            .map_err(|e| AuthError::InternalError(format!("Failed to cleanup sessions: {e}")))
    }

    async fn validate_api_key(&self, api_key: String) -> Result<ApiKey, AuthError> {
        debug!("Validating API key: {}", api_key);
        self.api_key_repository
            .validate(api_key)
            .await
            .map_err(|e| AuthError::InternalError(format!("Failed to validate API key: {e}")))?
            .ok_or(AuthError::Unauthorized)
    }

    async fn can_manage_workspace_api_keys(
        &self,
        workspace_id: WorkspaceId,
        user_id: UserId,
    ) -> Result<bool, AuthError> {
        // Get workspace to find the parent organization
        let workspace = self
            .workspace_repository
            .get_by_id(workspace_id)
            .await
            .map_err(|e| AuthError::InternalError(format!("Failed to get workspace: {e}")))?
            .ok_or(AuthError::Unauthorized)?;

        // Check if user has permission to create API keys for this workspace's organization
        let member = self
            .organization_repository
            .get_member(workspace.organization_id.0, user_id.0)
            .await
            .map_err(|e| {
                AuthError::InternalError(format!("Failed to check organization membership: {e}"))
            })?;

        Ok(member.is_some_and(|m| m.role.can_manage_api_keys()))
    }
}

impl AuthService {
    pub fn new(
        user_repository: Arc<dyn UserRepository>,
        session_repository: Arc<dyn SessionRepository>,
        api_key_repository: Arc<dyn ApiKeyRepository>,
        organization_repository: Arc<dyn OrganizationRepository>,
        workspace_repository: Arc<dyn WorkspaceRepository>,
        organization_service: Arc<dyn crate::organization::OrganizationServiceTrait>,
    ) -> Self {
        Self {
            user_repository,
            session_repository,
            api_key_repository,
            organization_repository,
            workspace_repository,
            organization_service,
        }
    }

    /// Clean up expired sessions
    pub async fn cleanup_expired_sessions(&self) -> Result<usize, AuthError> {
        self.session_repository
            .cleanup_expired()
            .await
            .map_err(|e| AuthError::InternalError(format!("Failed to cleanup sessions: {e}")))
    }
}
