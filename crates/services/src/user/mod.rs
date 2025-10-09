use crate::auth::ports::{Session, SessionId, SessionRepository, User, UserId, UserRepository};
use crate::organization::OrganizationServiceTrait;
use crate::workspace::{CreateApiKeyRequest, WorkspaceRepository, WorkspaceServiceTrait};
use async_trait::async_trait;
use std::sync::Arc;
use tracing::{debug, error};

pub mod ports;
pub use ports::*;

/// UserService handles user profile and session management operations
pub struct UserService {
    user_repository: Arc<dyn UserRepository>,
    session_repository: Arc<dyn SessionRepository>,
    organization_service: Arc<dyn OrganizationServiceTrait>,
    workspace_repository: Arc<dyn WorkspaceRepository>,
    workspace_service: Arc<dyn WorkspaceServiceTrait>,
}

impl UserService {
    pub fn new(
        user_repository: Arc<dyn UserRepository>,
        session_repository: Arc<dyn SessionRepository>,
        organization_service: Arc<dyn OrganizationServiceTrait>,
        workspace_repository: Arc<dyn WorkspaceRepository>,
        workspace_service: Arc<dyn WorkspaceServiceTrait>,
    ) -> Self {
        Self {
            user_repository,
            session_repository,
            organization_service,
            workspace_repository,
            workspace_service,
        }
    }

    /// Generate an organization name from user email with random suffix
    /// e.g., "alice@example.com" -> "alice-org-x7k2"
    fn generate_org_name_from_email(email: &str) -> String {
        use rand::Rng;
        const CHARSET: &[u8] = b"abcdefghijklmnopqrstuvwxyz0123456789";
        let mut rng = rand::thread_rng();

        let username = email.split('@').next().unwrap_or("user");

        // Generate a 4-character random suffix
        let suffix: String = (0..4)
            .map(|_| {
                let idx = rng.gen_range(0..CHARSET.len());
                CHARSET[idx] as char
            })
            .collect();

        format!("{}-org-{}", username, suffix)
    }

    /// Generate a workspace name
    fn generate_workspace_name() -> String {
        "default".to_string()
    }

    /// Generate an API key name from user email
    fn generate_api_key_name(email: &str) -> String {
        let username = email.split('@').next().unwrap_or("user");
        format!("{}'s API Key", username)
    }
}

#[async_trait]
impl UserServiceTrait for UserService {
    async fn get_user(&self, user_id: UserId) -> Result<User, UserServiceError> {
        debug!("Getting user: {}", user_id);

        self.user_repository
            .get_by_id(user_id)
            .await
            .map_err(|e| UserServiceError::InternalError(format!("Failed to get user: {}", e)))?
            .ok_or(UserServiceError::UserNotFound)
    }

    async fn update_profile(
        &self,
        user_id: UserId,
        display_name: Option<String>,
        avatar_url: Option<String>,
    ) -> Result<User, UserServiceError> {
        debug!("Updating profile for user: {}", user_id);

        self.user_repository
            .update(user_id, display_name, avatar_url)
            .await
            .map_err(|e| {
                UserServiceError::InternalError(format!("Failed to update profile: {}", e))
            })?
            .ok_or(UserServiceError::UserNotFound)
    }

    async fn get_user_sessions(&self, user_id: UserId) -> Result<Vec<Session>, UserServiceError> {
        debug!("Getting sessions for user: {}", user_id);

        self.session_repository
            .list_by_user(user_id)
            .await
            .map_err(|e| {
                UserServiceError::InternalError(format!("Failed to get user sessions: {}", e))
            })
    }

    async fn revoke_session(
        &self,
        user_id: UserId,
        session_id: SessionId,
    ) -> Result<bool, UserServiceError> {
        debug!("Revoking session: {} for user: {}", session_id, user_id);

        // Verify the session belongs to the user
        let session = self
            .session_repository
            .get_by_id(session_id.clone())
            .await
            .map_err(|e| UserServiceError::InternalError(format!("Failed to get session: {}", e)))?
            .ok_or(UserServiceError::SessionNotFound)?;

        if session.user_id != user_id {
            return Err(UserServiceError::Unauthorized(
                "Session does not belong to user".to_string(),
            ));
        }

        self.session_repository
            .revoke(session_id)
            .await
            .map_err(|e| {
                UserServiceError::InternalError(format!("Failed to revoke session: {}", e))
            })
    }

    async fn revoke_all_sessions(&self, user_id: UserId) -> Result<usize, UserServiceError> {
        debug!("Revoking all sessions for user: {}", user_id);

        self.session_repository
            .revoke_all_for_user(user_id)
            .await
            .map_err(|e| {
                UserServiceError::InternalError(format!(
                    "Failed to revoke all user sessions: {}",
                    e
                ))
            })
    }

    async fn quick_setup(&self, user_id: UserId) -> Result<QuickSetupResult, UserServiceError> {
        debug!("Quick setup for user: {}", user_id);

        // Get user to extract email
        let user = self.get_user(user_id.clone()).await?;

        // Generate names from user email
        let org_name = Self::generate_org_name_from_email(&user.email);
        let workspace_name = Self::generate_workspace_name();
        let api_key_name = Self::generate_api_key_name(&user.email);

        debug!(
            "Creating organization: {}, workspace: {}, api_key: {}",
            org_name, workspace_name, api_key_name
        );

        // Step 1: Create organization
        let organization = self
            .organization_service
            .create_organization(org_name.clone(), None, user_id.clone())
            .await
            .map_err(|e| {
                error!("Failed to create organization: {}", e);
                if e.to_string().contains("duplicate key")
                    || e.to_string().contains("already exists")
                {
                    UserServiceError::OrganizationAlreadyExists
                } else {
                    UserServiceError::InternalError(format!("Failed to create organization: {}", e))
                }
            })?;

        debug!("Created organization: {}", organization.id.0);

        // Step 2: Create workspace
        let workspace = self
            .workspace_repository
            .create(
                workspace_name.clone(),
                workspace_name.clone(),
                Some(format!("Default workspace for {}", org_name)),
                organization.id.clone(),
                user_id.clone(),
            )
            .await
            .map_err(|e| {
                error!("Failed to create workspace: {}", e);
                UserServiceError::InternalError(format!("Failed to create workspace: {}", e))
            })?;

        debug!("Created workspace: {}", workspace.id.0);

        // Step 3: Create API key
        let create_key_request = CreateApiKeyRequest {
            name: Some(api_key_name),
            workspace_id: workspace.id.clone(),
            created_by_user_id: user_id.clone(),
            expires_at: None,
        };

        let api_key = self
            .workspace_service
            .create_api_key(create_key_request)
            .await
            .map_err(|e| {
                error!("Failed to create API key: {}", e);
                UserServiceError::InternalError(format!("Failed to create API key: {}", e))
            })?;

        debug!("Created API key: {:?}", api_key.id);

        Ok(QuickSetupResult {
            organization,
            workspace,
            api_key,
        })
    }
}
