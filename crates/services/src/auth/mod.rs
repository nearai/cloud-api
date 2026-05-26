pub mod near;
pub mod oauth;
pub mod ports;

pub use near::NearAuthService;
pub use oauth::OAuthManager;
pub use ports::*;
use tracing::{debug, error, info, warn};

use crate::common::{hash_api_key, is_valid_api_key_format};
use crate::organization::OrganizationRepository;
use crate::workspace::{ApiKey, ApiKeyRepository, WorkspaceId, WorkspaceRepository};
use async_trait::async_trait;
use bloomfilter::Bloom;
use chrono::Utc;
use moka::future::Cache;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::RwLock;

pub const MAX_USER_AGENT_LEN: usize = 4096;

const API_KEY_CACHE_MAX_CAPACITY: u64 = 10_000;
const API_KEY_CACHE_TTL_SECS: u64 = 30;
const BLOOM_FILTER_ITEMS: usize = 10_000_000;
const BLOOM_FILTER_FP_RATE: f64 = 0.001;
const BLOOM_FILTER_SYNC_INTERVAL_SECS: u64 = 10;
const BLOOM_FILTER_FULL_REBUILD_INTERVAL_SECS: u64 = 60 * 60;

#[async_trait]
impl AuthServiceTrait for AuthService {
    async fn create_session(
        &self,
        user_id: UserId,
        ip_address: Option<String>,
        user_agent: String,
        encoding_key: String,
        expires_in_hours: i64,
        refresh_expires_in_hours: i64,
    ) -> Result<(String, Session, String), AuthError> {
        let user_agent = user_agent.trim().to_string();
        if user_agent.is_empty() {
            return Err(AuthError::InvalidUserAgent);
        }
        if user_agent.len() > MAX_USER_AGENT_LEN {
            return Err(AuthError::UserAgentTooLong(MAX_USER_AGENT_LEN));
        }

        let access_token =
            self.create_session_access_token(user_id.clone(), encoding_key, expires_in_hours)?;

        let (refresh_session, refresh_token) = self
            .session_repository
            .create(user_id, ip_address, user_agent, refresh_expires_in_hours)
            .await
            .map_err(|e| AuthError::InternalError(format!("Failed to create session: {e}")))?;

        Ok((access_token, refresh_session, refresh_token))
    }

    fn create_session_access_token(
        &self,
        user_id: UserId,
        encoding_key: String,
        expires_in_hours: i64,
    ) -> Result<String, AuthError> {
        let expiration = chrono::Utc::now() + chrono::Duration::hours(expires_in_hours);

        let claims = AccessTokenClaims {
            sub: user_id,
            exp: expiration.timestamp(),
            iat: chrono::Utc::now().timestamp(),
        };

        jsonwebtoken::encode(
            &jsonwebtoken::Header::default(),
            &claims,
            &jsonwebtoken::EncodingKey::from_secret(encoding_key.as_bytes()),
        )
        .map_err(|e| AuthError::InternalError(format!("Failed to create jwt: {e}")))
    }

    fn validate_session_access_token(
        &self,
        access_token: String,
        encoding_key: String,
    ) -> Result<Option<AccessTokenClaims>, AuthError> {
        let claims = jsonwebtoken::decode::<AccessTokenClaims>(
            access_token,
            &jsonwebtoken::DecodingKey::from_secret(encoding_key.as_bytes()),
            &jsonwebtoken::Validation::new(jsonwebtoken::Algorithm::HS256),
        )
        .map_err(|_| AuthError::SessionNotFound)?;

        if claims.claims.exp < Utc::now().timestamp() {
            return Err(AuthError::SessionNotFound);
        }

        Ok(Some(claims.claims))
    }

    async fn validate_session_access(
        &self,
        access_token: String,
        encoding_key: String,
    ) -> Result<User, AuthError> {
        let claims = self
            .validate_session_access_token(access_token, encoding_key)?
            .ok_or(AuthError::SessionNotFound)?;

        debug!("Claims: {:?}", claims);

        // Get the user
        let user = self
            .user_repository
            .get_by_id(claims.sub)
            .await
            .map_err(|e| AuthError::InternalError(format!("Failed to get user: {e}")))?
            .ok_or(AuthError::UserNotFound)?;

        // Check if token was issued before tokens_revoked_at
        if let Some(revoked_at) = user.tokens_revoked_at {
            let token_issued_at = chrono::DateTime::from_timestamp(claims.iat, 0)
                .ok_or(AuthError::SessionNotFound)?;

            if token_issued_at < revoked_at {
                debug!(
                    "Token issued at {} is before revocation time {}",
                    token_issued_at, revoked_at
                );
                return Err(AuthError::SessionNotFound);
            }
        }

        Ok(user)
    }

    async fn validate_session_refresh_token(
        &self,
        refresh_token: SessionToken,
        user_agent: &str,
    ) -> Result<Option<Session>, AuthError> {
        let user_agent = user_agent.trim();
        if user_agent.is_empty() {
            return Err(AuthError::InvalidUserAgent);
        }
        if user_agent.len() > MAX_USER_AGENT_LEN {
            return Err(AuthError::UserAgentTooLong(MAX_USER_AGENT_LEN));
        }

        self.session_repository
            .validate(refresh_token, user_agent)
            .await
            .map_err(|e| AuthError::InternalError(format!("Failed to validate session: {e}")))
    }

    async fn validate_session_refresh(
        &self,
        refresh_token: SessionToken,
        user_agent: &str,
    ) -> Result<(Session, User), AuthError> {
        let session = self
            .validate_session_refresh_token(refresh_token, user_agent)
            .await?
            .ok_or(AuthError::SessionNotFound)?;

        debug!("Session: {:?}", session);
        // Check if session is expired (validation already handles this, but keep for clarity)
        if session.expires_at < Utc::now() {
            return Err(AuthError::SessionNotFound);
        }

        // Get the user
        let user = self
            .user_repository
            .get_by_id(session.user_id.clone())
            .await
            .map_err(|e| AuthError::InternalError(format!("Failed to get user: {e}")))?
            .ok_or(AuthError::UserNotFound)?;

        Ok((session, user))
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

    async fn rotate_session(
        &self,
        user_id: UserId,
        session_id: SessionId,
        old_token_hash: &str,
        encoding_key: String,
        access_token_expires_in_hours: i64,
        refresh_token_expires_in_hours: i64,
    ) -> Result<(String, Session, String), AuthError> {
        // Create a new access token
        let access_token = self.create_session_access_token(
            user_id.clone(),
            encoding_key,
            access_token_expires_in_hours,
        )?;

        // Rotate the refresh token
        let (rotated_session, new_refresh_token) = self
            .session_repository
            .rotate(session_id, old_token_hash, refresh_token_expires_in_hours)
            .await
            .map_err(|e| {
                let error_msg = e.to_string();
                // Check if the error indicates the token was already rotated or session not found
                if error_msg.contains("already rotated") || error_msg.contains("not found") {
                    AuthError::Unauthorized
                } else {
                    AuthError::InternalError(format!("Failed to rotate session: {e}"))
                }
            })?;

        Ok((access_token, rotated_session, new_refresh_token))
    }

    async fn get_or_create_oauth_user(&self, oauth_info: OAuthUserInfo) -> Result<User, AuthError> {
        use crate::organization::OrganizationId;
        // Look up by provider identity — the stable, unique identifier from the OAuth provider
        let existing_user = self
            .user_repository
            .get_by_provider(&oauth_info.provider, &oauth_info.provider_user_id)
            .await
            .map_err(|e| AuthError::InternalError(format!("Failed to check existing user: {e}")))?;

        if let Some(user) = existing_user {
            self.user_repository
                .update_last_login(user.id.clone())
                .await
                .map_err(|e| {
                    AuthError::InternalError(format!("Failed to update last login: {e}"))
                })?;

            if user.email != oauth_info.email {
                self.user_repository
                    .update_email(user.id.clone(), oauth_info.email)
                    .await
                    .map_err(|e| {
                        AuthError::InternalError(format!("Failed to update user email: {e}"))
                    })?;
            }

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
            use rand::RngExt;
            let username = oauth_info.email.split('@').next().unwrap_or("user");
            const CHARSET: &[u8] = b"abcdefghijklmnopqrstuvwxyz0123456789";
            let mut rng = rand::rng();
            let suffix: String = (0..4)
                .map(|_| {
                    let idx = rng.random_range(0..CHARSET.len());
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
                    Err(_) => {
                        // Log error but don't fail user creation
                        tracing::error!("Failed to create default workspace for new user");
                    }
                }
            }
            Err(_) => {
                // Log error but don't fail user creation
                tracing::error!("Failed to create default organization for new user");
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
        if !is_valid_api_key_format(&api_key) {
            return Err(AuthError::Unauthorized);
        }

        let key_hash = hash_api_key(&api_key);

        if let Some(cached_key) = self.api_key_cache.get(&key_hash).await {
            return Ok(cached_key);
        }

        if self.bloom_filter_ready.load(Ordering::Acquire) {
            let bloom = self.api_key_bloom_filter.read().await;
            if !bloom.check(&key_hash) {
                return Err(AuthError::Unauthorized);
            }
        }

        let validated_key = self
            .api_key_repository
            .validate(api_key)
            .await
            .map_err(|e| AuthError::InternalError(format!("Failed to validate API key: {e}")))?
            .ok_or(AuthError::Unauthorized)?;

        self.api_key_cache
            .insert(key_hash, validated_key.clone())
            .await;

        Ok(validated_key)
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
        let api_key_cache: ApiKeyCache = Cache::builder()
            .max_capacity(API_KEY_CACHE_MAX_CAPACITY)
            .time_to_live(Duration::from_secs(API_KEY_CACHE_TTL_SECS))
            .build();

        let bloom = Bloom::new_for_fp_rate(BLOOM_FILTER_ITEMS, BLOOM_FILTER_FP_RATE)
            .expect("bloom filter creation failed");
        let api_key_bloom_filter: ApiKeyBloomFilter = Arc::new(RwLock::new(bloom));
        let bloom_filter_ready: BloomFilterReady = Arc::new(AtomicBool::new(false));

        Self::spawn_bloom_filter_sync(
            api_key_repository.clone(),
            api_key_bloom_filter.clone(),
            bloom_filter_ready.clone(),
        );

        Self {
            user_repository,
            session_repository,
            api_key_repository,
            organization_repository,
            workspace_repository,
            organization_service,
            api_key_cache,
            api_key_bloom_filter,
            bloom_filter_ready,
        }
    }

    fn spawn_bloom_filter_sync(
        api_key_repository: Arc<dyn ApiKeyRepository>,
        bloom_filter: ApiKeyBloomFilter,
        ready_flag: BloomFilterReady,
    ) {
        /// Fetch all active key hashes, clear the bloom filter, and repopulate it.
        async fn rebuild_bloom(
            repo: &dyn ApiKeyRepository,
            bloom_filter: &ApiKeyBloomFilter,
            ready_flag: &BloomFilterReady,
        ) -> Result<usize, String> {
            let hashes = repo
                .get_all_active_key_hashes()
                .await
                .map_err(|e| e.to_string())?;
            let count = hashes.len();

            let mut bloom = bloom_filter.write().await;
            bloom.clear();
            for hash in hashes {
                bloom.set(&hash);
            }
            drop(bloom);

            ready_flag.store(true, Ordering::Release);
            Ok(count)
        }

        tokio::spawn(async move {
            let mut last_sync = Utc::now();
            let mut last_full_rebuild = Utc::now()
                - chrono::Duration::seconds(BLOOM_FILTER_FULL_REBUILD_INTERVAL_SECS as i64);

            // Initial full build
            match rebuild_bloom(&*api_key_repository, &bloom_filter, &ready_flag).await {
                Ok(count) => {
                    // Keep last_sync at the pre-fetch timestamp to ensure keys created
                    // during the fetch are picked up by the next incremental sync
                    last_full_rebuild = Utc::now();
                    info!("Initialized bloom filter with {} keys", count);
                }
                Err(e) => {
                    error!("Failed to initialize bloom filter; continuing without bloom optimization: {e}");
                }
            }

            loop {
                tokio::time::sleep(Duration::from_secs(BLOOM_FILTER_SYNC_INTERVAL_SECS)).await;
                let now = Utc::now();

                // Periodic full rebuild to remove revoked keys.
                if (now - last_full_rebuild).num_seconds()
                    >= BLOOM_FILTER_FULL_REBUILD_INTERVAL_SECS as i64
                {
                    match rebuild_bloom(&*api_key_repository, &bloom_filter, &ready_flag).await {
                        Ok(count) => {
                            last_full_rebuild = now;
                            last_sync = now;
                            info!("Rebuilt bloom filter with {} keys", count);
                        }
                        Err(e) => warn!("Failed to rebuild bloom filter: {e}"),
                    }
                    continue;
                }

                // Incremental sync (adds only)
                match api_key_repository
                    .get_active_key_hashes_created_after(last_sync)
                    .await
                {
                    Ok(new_hashes) => {
                        if !new_hashes.is_empty() {
                            let mut bloom = bloom_filter.write().await;
                            for hash in &new_hashes {
                                bloom.set(hash);
                            }
                            debug!("Added {} keys to bloom filter", new_hashes.len());
                        }
                        last_sync = now;
                    }
                    Err(e) => warn!("Failed to sync bloom filter: {e}"),
                }
            }
        });
    }

    /// Clean up expired sessions
    pub async fn cleanup_expired_sessions(&self) -> Result<usize, AuthError> {
        self.session_repository
            .cleanup_expired()
            .await
            .map_err(|e| AuthError::InternalError(format!("Failed to cleanup sessions: {e}")))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::common::RepositoryError;
    use crate::organization::{
        AddOrganizationMemberRequest, BatchInvitationResponse, CreateOrganizationRequest,
        InvitationStatus, MemberRole, Organization, OrganizationError, OrganizationId,
        OrganizationInvitation, OrganizationInvitationWithDetails, OrganizationMember,
        OrganizationMemberWithUser, OrganizationOrderBy, OrganizationOrderDirection,
        OrganizationRepository, OrganizationServiceTrait, OrganizationWithRole,
        UpdateOrganizationMemberRequest, UpdateOrganizationRequest,
    };
    use crate::workspace::{
        ApiKey, ApiKeyId, ApiKeyRepository, CreateApiKeyRequest, Workspace, WorkspaceId,
        WorkspaceOrderBy, WorkspaceOrderDirection, WorkspaceRepository,
    };
    use bloomfilter::Bloom;
    use chrono::Utc;
    use std::sync::atomic::AtomicBool;
    use std::sync::Mutex;
    use tokio::sync::RwLock;
    use uuid::Uuid;

    fn make_user(email: &str, provider: &str) -> User {
        User {
            id: UserId(Uuid::new_v4()),
            email: email.to_string(),
            username: email.split('@').next().unwrap().to_string(),
            display_name: Some("Test User".to_string()),
            avatar_url: None,
            auth_provider: provider.to_string(),
            role: UserRole::User,
            is_active: true,
            last_login: None,
            created_at: Utc::now(),
            updated_at: Utc::now(),
            tokens_revoked_at: None,
        }
    }

    struct MockUserRepo {
        user: Mutex<Option<User>>,
        email_updated: Mutex<Option<String>>,
        last_login_updated: Mutex<bool>,
        profile_updated: Mutex<bool>,
    }

    impl MockUserRepo {
        fn with_user(user: User) -> Self {
            Self {
                user: Mutex::new(Some(user)),
                email_updated: Mutex::new(None),
                last_login_updated: Mutex::new(false),
                profile_updated: Mutex::new(false),
            }
        }
    }

    #[async_trait]
    impl UserRepository for MockUserRepo {
        async fn create(
            &self,
            _: String,
            _: String,
            _: Option<String>,
            _: Option<String>,
        ) -> anyhow::Result<User> {
            unimplemented!()
        }
        async fn create_from_oauth(
            &self,
            _: String,
            _: String,
            _: Option<String>,
            _: Option<String>,
            _: String,
            _: String,
        ) -> anyhow::Result<User> {
            unimplemented!()
        }
        async fn get_by_id(&self, _: UserId) -> anyhow::Result<Option<User>> {
            unimplemented!()
        }
        async fn get_by_email(&self, _: &str) -> anyhow::Result<Option<User>> {
            unimplemented!()
        }
        async fn get_by_provider(
            &self,
            auth_provider: &str,
            _provider_user_id: &str,
        ) -> anyhow::Result<Option<User>> {
            let user = self.user.lock().unwrap();
            Ok(user
                .as_ref()
                .filter(|u| u.auth_provider == auth_provider)
                .cloned())
        }
        async fn update(
            &self,
            _: UserId,
            _: Option<String>,
            _: Option<String>,
        ) -> anyhow::Result<Option<User>> {
            *self.profile_updated.lock().unwrap() = true;
            Ok(self.user.lock().unwrap().clone())
        }
        async fn update_email(&self, _: UserId, email: String) -> anyhow::Result<()> {
            *self.email_updated.lock().unwrap() = Some(email);
            Ok(())
        }
        async fn update_last_login(&self, _: UserId) -> anyhow::Result<()> {
            *self.last_login_updated.lock().unwrap() = true;
            Ok(())
        }
        async fn update_tokens_revoked_at(&self, _: UserId) -> anyhow::Result<()> {
            unimplemented!()
        }
        async fn delete(&self, _: UserId) -> anyhow::Result<bool> {
            unimplemented!()
        }
        async fn list(&self, _: i64, _: i64) -> anyhow::Result<Vec<User>> {
            unimplemented!()
        }
    }

    // Stubs for traits required to construct AuthService but not called in returning-user tests.
    struct StubSessionRepo;
    #[async_trait]
    impl SessionRepository for StubSessionRepo {
        async fn create(
            &self,
            _: UserId,
            _: Option<String>,
            _: String,
            _: i64,
        ) -> anyhow::Result<(Session, String)> {
            unimplemented!()
        }
        async fn validate(&self, _: SessionToken, _: &str) -> anyhow::Result<Option<Session>> {
            unimplemented!()
        }
        async fn get_by_id(&self, _: SessionId) -> anyhow::Result<Option<Session>> {
            unimplemented!()
        }
        async fn list_by_user(&self, _: UserId) -> anyhow::Result<Vec<Session>> {
            unimplemented!()
        }
        async fn extend(&self, _: SessionId, _: i64) -> anyhow::Result<bool> {
            unimplemented!()
        }
        async fn rotate(&self, _: SessionId, _: &str, _: i64) -> anyhow::Result<(Session, String)> {
            unimplemented!()
        }
        async fn revoke(&self, _: SessionId) -> anyhow::Result<bool> {
            unimplemented!()
        }
        async fn revoke_all_for_user(&self, _: UserId) -> anyhow::Result<usize> {
            unimplemented!()
        }
        async fn cleanup_expired(&self) -> anyhow::Result<usize> {
            unimplemented!()
        }
    }

    struct StubApiKeyRepo;
    #[async_trait]
    impl ApiKeyRepository for StubApiKeyRepo {
        async fn validate(&self, _: String) -> Result<Option<ApiKey>, RepositoryError> {
            unimplemented!()
        }
        async fn create(&self, _: CreateApiKeyRequest) -> Result<ApiKey, RepositoryError> {
            unimplemented!()
        }
        async fn get_by_id(&self, _: ApiKeyId) -> Result<Option<ApiKey>, RepositoryError> {
            unimplemented!()
        }
        async fn list_by_workspace_paginated(
            &self,
            _: WorkspaceId,
            _: i64,
            _: i64,
        ) -> Result<Vec<ApiKey>, RepositoryError> {
            unimplemented!()
        }
        async fn delete(&self, _: ApiKeyId) -> Result<bool, RepositoryError> {
            unimplemented!()
        }
        async fn update_last_used(&self, _: ApiKeyId) -> Result<(), RepositoryError> {
            unimplemented!()
        }
        async fn update_spend_limit(
            &self,
            _: ApiKeyId,
            _: Option<i64>,
        ) -> Result<ApiKey, RepositoryError> {
            unimplemented!()
        }
        async fn update(
            &self,
            _: ApiKeyId,
            _: Option<String>,
            _: Option<Option<chrono::DateTime<Utc>>>,
            _: Option<Option<i64>>,
            _: Option<bool>,
        ) -> Result<ApiKey, RepositoryError> {
            unimplemented!()
        }
        async fn count_by_workspace(&self, _: WorkspaceId) -> Result<i64, RepositoryError> {
            unimplemented!()
        }
        async fn check_name_duplication(
            &self,
            _: WorkspaceId,
            _: &str,
        ) -> Result<bool, RepositoryError> {
            unimplemented!()
        }
        async fn revoke(&self, _: ApiKeyId) -> Result<bool, RepositoryError> {
            unimplemented!()
        }
        async fn get_all_active_key_hashes(&self) -> Result<Vec<String>, RepositoryError> {
            Ok(vec![])
        }
        async fn get_active_key_hashes_created_after(
            &self,
            _: chrono::DateTime<Utc>,
        ) -> Result<Vec<String>, RepositoryError> {
            Ok(vec![])
        }
    }

    struct StubOrgRepo;
    #[async_trait]
    impl OrganizationRepository for StubOrgRepo {
        async fn create(
            &self,
            _: CreateOrganizationRequest,
            _: Uuid,
        ) -> Result<Organization, RepositoryError> {
            unimplemented!()
        }
        async fn get_by_id(&self, _: Uuid) -> Result<Option<Organization>, RepositoryError> {
            unimplemented!()
        }
        async fn get_by_name(&self, _: &str) -> Result<Option<Organization>, RepositoryError> {
            unimplemented!()
        }
        async fn get_member(
            &self,
            _: Uuid,
            _: Uuid,
        ) -> Result<Option<OrganizationMember>, RepositoryError> {
            unimplemented!()
        }
        async fn update(
            &self,
            _: Uuid,
            _: UpdateOrganizationRequest,
        ) -> Result<Organization, RepositoryError> {
            unimplemented!()
        }
        async fn delete(&self, _: Uuid) -> Result<bool, RepositoryError> {
            unimplemented!()
        }
        async fn add_member(
            &self,
            _: Uuid,
            _: AddOrganizationMemberRequest,
            _: Uuid,
        ) -> Result<OrganizationMember, RepositoryError> {
            unimplemented!()
        }
        async fn update_member(
            &self,
            _: Uuid,
            _: Uuid,
            _: UpdateOrganizationMemberRequest,
        ) -> Result<OrganizationMember, RepositoryError> {
            unimplemented!()
        }
        async fn remove_member(&self, _: Uuid, _: Uuid) -> Result<bool, RepositoryError> {
            unimplemented!()
        }
        async fn list_members_paginated(
            &self,
            _: Uuid,
            _: i64,
            _: i64,
        ) -> Result<Vec<OrganizationMember>, RepositoryError> {
            unimplemented!()
        }
        async fn get_member_count(&self, _: Uuid) -> Result<i64, RepositoryError> {
            unimplemented!()
        }
        async fn count_organizations_by_user(&self, _: Uuid) -> Result<i64, RepositoryError> {
            unimplemented!()
        }
        async fn list_organizations_by_user(
            &self,
            _: Uuid,
            _: i64,
            _: i64,
            _: Option<OrganizationOrderBy>,
            _: Option<OrganizationOrderDirection>,
        ) -> Result<Vec<Organization>, RepositoryError> {
            unimplemented!()
        }

        async fn list_organizations_with_roles_by_user(
            &self,
            _: Uuid,
            _: i64,
            _: i64,
            _: Option<OrganizationOrderBy>,
            _: Option<OrganizationOrderDirection>,
        ) -> Result<Vec<OrganizationWithRole>, RepositoryError> {
            unimplemented!()
        }
    }

    struct StubWorkspaceRepo;
    #[async_trait]
    impl WorkspaceRepository for StubWorkspaceRepo {
        async fn get_by_id(&self, _: WorkspaceId) -> Result<Option<Workspace>, RepositoryError> {
            unimplemented!()
        }
        async fn get_by_name(
            &self,
            _: Uuid,
            _: &str,
        ) -> Result<Option<Workspace>, RepositoryError> {
            unimplemented!()
        }
        async fn get_workspace_with_organization(
            &self,
            _: WorkspaceId,
        ) -> Result<Option<(Workspace, Organization)>, RepositoryError> {
            unimplemented!()
        }
        async fn list_by_organization(
            &self,
            _: OrganizationId,
        ) -> Result<Vec<Workspace>, RepositoryError> {
            unimplemented!()
        }
        async fn list_by_organization_paginated(
            &self,
            _: OrganizationId,
            _: i64,
            _: i64,
            _: Option<WorkspaceOrderBy>,
            _: Option<WorkspaceOrderDirection>,
        ) -> Result<Vec<Workspace>, RepositoryError> {
            unimplemented!()
        }
        async fn create(
            &self,
            _: String,
            _: Option<String>,
            _: OrganizationId,
            _: UserId,
        ) -> Result<Workspace, RepositoryError> {
            unimplemented!()
        }
        async fn update(
            &self,
            _: WorkspaceId,
            _: Option<String>,
            _: Option<String>,
            _: Option<serde_json::Value>,
        ) -> Result<Option<Workspace>, RepositoryError> {
            unimplemented!()
        }
        async fn delete(&self, _: WorkspaceId) -> Result<bool, RepositoryError> {
            unimplemented!()
        }
        async fn count_by_organization(&self, _: OrganizationId) -> Result<i64, RepositoryError> {
            unimplemented!()
        }
    }

    struct StubOrgService;
    #[async_trait]
    impl OrganizationServiceTrait for StubOrgService {
        async fn create_organization(
            &self,
            _: String,
            _: Option<String>,
            _: UserId,
        ) -> Result<Organization, OrganizationError> {
            unimplemented!()
        }
        async fn get_organization(
            &self,
            _: OrganizationId,
        ) -> Result<Organization, OrganizationError> {
            unimplemented!()
        }
        async fn update_organization(
            &self,
            _: OrganizationId,
            _: UserId,
            _: Option<String>,
            _: Option<String>,
            _: Option<i32>,
            _: Option<serde_json::Value>,
        ) -> Result<Organization, OrganizationError> {
            unimplemented!()
        }
        async fn delete_organization(
            &self,
            _: OrganizationId,
            _: UserId,
        ) -> Result<bool, OrganizationError> {
            unimplemented!()
        }
        async fn list_organizations_for_user(
            &self,
            _: UserId,
            _: i64,
            _: i64,
            _: Option<OrganizationOrderBy>,
            _: Option<OrganizationOrderDirection>,
        ) -> Result<Vec<Organization>, OrganizationError> {
            unimplemented!()
        }
        async fn list_organizations_with_roles_for_user(
            &self,
            _: UserId,
            _: i64,
            _: i64,
            _: Option<OrganizationOrderBy>,
            _: Option<OrganizationOrderDirection>,
        ) -> Result<Vec<OrganizationWithRole>, OrganizationError> {
            unimplemented!()
        }
        async fn count_organizations_for_user(&self, _: UserId) -> Result<i64, OrganizationError> {
            unimplemented!()
        }
        async fn add_member(
            &self,
            _: OrganizationId,
            _: UserId,
            _: UserId,
            _: MemberRole,
        ) -> Result<OrganizationMember, OrganizationError> {
            unimplemented!()
        }
        async fn remove_member(
            &self,
            _: OrganizationId,
            _: UserId,
            _: UserId,
        ) -> Result<bool, OrganizationError> {
            unimplemented!()
        }
        async fn update_member_role(
            &self,
            _: OrganizationId,
            _: UserId,
            _: UserId,
            _: MemberRole,
        ) -> Result<OrganizationMember, OrganizationError> {
            unimplemented!()
        }
        async fn is_member(&self, _: OrganizationId, _: UserId) -> Result<bool, OrganizationError> {
            unimplemented!()
        }
        async fn get_user_role(
            &self,
            _: OrganizationId,
            _: UserId,
        ) -> Result<Option<MemberRole>, OrganizationError> {
            unimplemented!()
        }
        async fn get_member_count(
            &self,
            _: OrganizationId,
            _: UserId,
        ) -> Result<i64, OrganizationError> {
            unimplemented!()
        }
        async fn get_organization_by_name(
            &self,
            _: &str,
        ) -> Result<Option<Organization>, OrganizationError> {
            unimplemented!()
        }
        async fn get_members_with_users_paginated(
            &self,
            _: OrganizationId,
            _: UserId,
            _: i64,
            _: i64,
        ) -> Result<Vec<OrganizationMemberWithUser>, OrganizationError> {
            unimplemented!()
        }
        async fn invite_members_by_email(
            &self,
            _: OrganizationId,
            _: UserId,
            _: Vec<(String, MemberRole)>,
        ) -> Result<BatchInvitationResponse, OrganizationError> {
            unimplemented!()
        }
        async fn add_member_validated(
            &self,
            _: OrganizationId,
            _: UserId,
            _: UserId,
            _: MemberRole,
        ) -> Result<OrganizationMember, OrganizationError> {
            unimplemented!()
        }
        async fn update_member_role_validated(
            &self,
            _: OrganizationId,
            _: UserId,
            _: UserId,
            _: MemberRole,
        ) -> Result<OrganizationMember, OrganizationError> {
            unimplemented!()
        }
        async fn remove_member_validated(
            &self,
            _: OrganizationId,
            _: UserId,
            _: UserId,
        ) -> Result<bool, OrganizationError> {
            unimplemented!()
        }
        async fn create_invitations(
            &self,
            _: OrganizationId,
            _: UserId,
            _: Vec<(String, MemberRole)>,
            _: i64,
        ) -> Result<BatchInvitationResponse, OrganizationError> {
            unimplemented!()
        }
        async fn list_user_invitations(
            &self,
            _: &str,
        ) -> Result<Vec<OrganizationInvitationWithDetails>, OrganizationError> {
            unimplemented!()
        }
        async fn get_invitation_by_token(
            &self,
            _: &str,
        ) -> Result<OrganizationInvitation, OrganizationError> {
            unimplemented!()
        }
        async fn accept_invitation_by_token(
            &self,
            _: &str,
            _: UserId,
            _: &str,
        ) -> Result<OrganizationMember, OrganizationError> {
            unimplemented!()
        }
        async fn accept_invitation(
            &self,
            _: Uuid,
            _: UserId,
            _: &str,
        ) -> Result<OrganizationMember, OrganizationError> {
            unimplemented!()
        }
        async fn decline_invitation(&self, _: Uuid, _: &str) -> Result<(), OrganizationError> {
            unimplemented!()
        }
        async fn list_organization_invitations(
            &self,
            _: OrganizationId,
            _: UserId,
            _: Option<InvitationStatus>,
        ) -> Result<Vec<OrganizationInvitation>, OrganizationError> {
            unimplemented!()
        }
        async fn get_system_prompt(
            &self,
            _: OrganizationId,
            _: UserId,
        ) -> Result<Option<String>, OrganizationError> {
            unimplemented!()
        }
        async fn update_system_prompt(
            &self,
            _: OrganizationId,
            _: UserId,
            _: Option<String>,
        ) -> Result<Option<String>, OrganizationError> {
            unimplemented!()
        }
    }

    fn build_auth_service(user_repo: Arc<MockUserRepo>) -> AuthService {
        let bloom = Bloom::new_for_fp_rate(100, 0.01).expect("bloom filter creation failed");
        AuthService {
            user_repository: user_repo,
            session_repository: Arc::new(StubSessionRepo),
            api_key_repository: Arc::new(StubApiKeyRepo),
            organization_repository: Arc::new(StubOrgRepo),
            workspace_repository: Arc::new(StubWorkspaceRepo),
            organization_service: Arc::new(StubOrgService),
            api_key_cache: moka::future::Cache::builder().build(),
            api_key_bloom_filter: Arc::new(RwLock::new(bloom)),
            bloom_filter_ready: Arc::new(AtomicBool::new(false)),
        }
    }

    #[tokio::test]
    async fn test_returning_user_matched_by_provider_identity() {
        let existing = make_user("alice@example.com", "google");
        let repo = Arc::new(MockUserRepo::with_user(existing.clone()));
        let service = build_auth_service(repo.clone());

        let result = service
            .get_or_create_oauth_user(OAuthUserInfo {
                provider: "google".to_string(),
                provider_user_id: "google-sub-123".to_string(),
                email: "alice@example.com".to_string(),
                username: "alice".to_string(),
                display_name: Some("Test User".to_string()),
                avatar_url: None,
            })
            .await
            .unwrap();

        assert_eq!(result.id, existing.id);
        assert!(*repo.last_login_updated.lock().unwrap());
        assert!(repo.email_updated.lock().unwrap().is_none());
    }

    #[tokio::test]
    async fn test_returning_user_email_changed_updates_email() {
        let existing = make_user("old@example.com", "google");
        let repo = Arc::new(MockUserRepo::with_user(existing.clone()));
        let service = build_auth_service(repo.clone());

        let result = service
            .get_or_create_oauth_user(OAuthUserInfo {
                provider: "google".to_string(),
                provider_user_id: "google-sub-456".to_string(),
                email: "new@example.com".to_string(),
                username: "new".to_string(),
                display_name: Some("Test User".to_string()),
                avatar_url: None,
            })
            .await
            .unwrap();

        assert_eq!(result.id, existing.id);
        assert!(*repo.last_login_updated.lock().unwrap());
        assert_eq!(
            *repo.email_updated.lock().unwrap(),
            Some("new@example.com".to_string())
        );
    }
}
