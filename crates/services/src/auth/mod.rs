mod linking;
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
use hmac::{Hmac, KeyInit, Mac};
use moka::future::Cache;
use sha2::{Digest, Sha256};
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

fn refresh_token_successor(old_token: &str, encoding_key: &str) -> Result<String, AuthError> {
    let mut mac = Hmac::<Sha256>::new_from_slice(encoding_key.as_bytes())
        .map_err(|e| AuthError::InternalError(format!("Invalid refresh rotation key: {e}")))?;
    mac.update(b"nearai-refresh-token-rotation-v1\0");
    mac.update(old_token.as_bytes());
    Ok(format!("rt_{}", hex::encode(mac.finalize().into_bytes())))
}

#[async_trait]
impl AuthServiceTrait for AuthService {
    async fn list_oauth_identities(
        &self,
        user_id: UserId,
    ) -> Result<Vec<OAuthIdentity>, AuthError> {
        self.get_user_by_id(user_id.clone()).await?;
        self.user_repository
            .list_oauth_identities(user_id)
            .await
            .map_err(|e| AuthError::InternalError(format!("Failed to list sign-in methods: {e}")))
    }
    async fn validate_oauth_link_session(
        &self,
        user_id: UserId,
        session_id: SessionId,
    ) -> Result<(), AuthError> {
        self.validate_link_session(user_id, session_id).await
    }
    async fn prepare_oauth_link(
        &self,
        user_id: UserId,
        session_id: SessionId,
        info: OAuthUserInfo,
    ) -> Result<String, AuthError> {
        self.prepare_link(user_id, session_id, info).await
    }
    async fn complete_oauth_link(
        &self,
        user_id: UserId,
        session_id: SessionId,
        code: &str,
    ) -> Result<OAuthIdentity, AuthError> {
        if code.len() != 64 || !code.bytes().all(|c| c.is_ascii_hexdigit()) {
            return Err(AuthError::OAuthLinkInvalid);
        }
        self.validate_link_session(user_id.clone(), session_id.clone())
            .await?;
        self.user_repository
            .complete_oauth_link_request(
                user_id,
                session_id,
                &hex::encode(Sha256::digest(code.as_bytes())),
            )
            .await
    }
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

        // Create the refresh-token session first so the access token can be
        // bound to it via the `sid` claim: revoking the session on logout then
        // invalidates both credentials at once.
        let (refresh_session, refresh_token) = self
            .session_repository
            .create(
                user_id.clone(),
                ip_address,
                user_agent,
                refresh_expires_in_hours,
            )
            .await
            .map_err(|e| AuthError::InternalError(format!("Failed to create session: {e}")))?;

        let access_token = self.create_session_access_token(
            user_id,
            Some(refresh_session.id.clone()),
            encoding_key,
            expires_in_hours,
        )?;

        Ok((access_token, refresh_session, refresh_token))
    }

    fn create_session_access_token(
        &self,
        user_id: UserId,
        session_id: Option<SessionId>,
        encoding_key: String,
        expires_in_hours: i64,
    ) -> Result<String, AuthError> {
        let expiration = chrono::Utc::now() + chrono::Duration::hours(expires_in_hours);

        let claims = AccessTokenClaims {
            sub: user_id,
            exp: expiration.timestamp(),
            iat: chrono::Utc::now().timestamp(),
            sid: session_id,
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
            .get_by_id(claims.sub.clone())
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

        // Bind the access token to its originating refresh-token session: a
        // revoked (logged-out) or expired session invalidates the token
        // immediately. This is a single primary-key lookup per request; it is
        // deliberately uncached because any cache TTL would re-open the exact
        // revocation window this check closes.
        match claims.sid {
            Some(session_id) => {
                let session = self
                    .session_repository
                    .get_by_id(session_id)
                    .await
                    .map_err(|e| AuthError::InternalError(format!("Failed to get session: {e}")))?
                    .ok_or(AuthError::SessionNotFound)?;

                if session.user_id != claims.sub || session.expires_at < Utc::now() {
                    debug!("Access token session is expired or bound to another user");
                    return Err(AuthError::SessionNotFound);
                }
            }
            None => {
                // Legacy access token issued before session binding. During
                // the compatibility window (flag off) these keep validating
                // and age out naturally; once the flag is on, any token that
                // cannot be tied to a live session is rejected.
                if self.require_session_bound_access_tokens {
                    debug!("Rejecting legacy access token without sid claim");
                    return Err(AuthError::SessionNotFound);
                }
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
        old_token: &str,
        encoding_key: String,
        access_token_expires_in_hours: i64,
        refresh_token_expires_in_hours: i64,
    ) -> Result<(String, Session, String), AuthError> {
        let successor_token = refresh_token_successor(old_token, &encoding_key)?;
        // Rotate the refresh token first; only mint an access token for a
        // session that is confirmed live. Rotation keeps the session id, so
        // the new access token stays bound to the same session.
        let (rotated_session, new_refresh_token) = self
            .session_repository
            .rotate(
                session_id,
                old_token,
                &successor_token,
                refresh_token_expires_in_hours,
            )
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

        // Create a new access token bound to the rotated session
        let access_token = self.create_session_access_token(
            user_id,
            Some(rotated_session.id.clone()),
            encoding_key,
            access_token_expires_in_hours,
        )?;

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
            return self.sync_existing_oauth_user(user, &oauth_info).await;
        }

        if let Some(user) = self.find_or_link_oauth_user_by_email(&oauth_info).await? {
            return Ok(user);
        }

        // Create new user
        let creation = self
            .user_repository
            .create_from_oauth(
                oauth_info.email.clone(),
                oauth_info.username.clone(),
                oauth_info.display_name.clone(),
                oauth_info.avatar_url.clone(),
                oauth_info.provider.clone(),
                oauth_info.provider_user_id.clone(),
            )
            .await;
        let new_user = match creation {
            Ok(user) => user,
            Err(error) => {
                // Explicit binding may provision the provider identity concurrently
                // with first login. Reuse it without creating another destination.
                if let Some(user) = self
                    .user_repository
                    .get_by_provider(&oauth_info.provider, &oauth_info.provider_user_id)
                    .await
                    .map_err(|e| {
                        AuthError::InternalError(format!("Failed to resolve user identity: {e}"))
                    })?
                {
                    return self.sync_existing_oauth_user(user, &oauth_info).await;
                }
                // The email may have been claimed after the preflight lookup.
                // Only classify a uniqueness failure as a possible email conflict;
                // infrastructure failures must remain internal errors.
                if matches!(
                    error.downcast_ref::<crate::common::RepositoryError>(),
                    Some(crate::common::RepositoryError::AlreadyExists)
                ) {
                    if let Some(user) = self.find_or_link_oauth_user_by_email(&oauth_info).await? {
                        return Ok(user);
                    }
                }
                return Err(AuthError::InternalError(format!(
                    "Failed to create user: {error}"
                )));
            }
        };

        // NEAR identities are provisioned with their personal organization and
        // workspace atomically by the repository, shared with wallet binding.
        if oauth_info.provider == "near" {
            return Ok(new_user);
        }

        self.user_repository
            .observe_oauth_identity(new_user.id.clone(), &oauth_info)
            .await
            .map_err(|e| AuthError::InternalError(format!("Failed to update identity: {e}")))?;

        // Create default organization and workspace for new user
        debug!(
            user_id = %new_user.id.0,
            "Creating default organization and workspace for new user"
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
                    organization_id = %organization.id.0,
                    user_id = %new_user.id.0,
                    "Created default organization for user"
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
                            workspace_id = %workspace.id.0,
                            user_id = %new_user.id.0,
                            "Created default workspace for user"
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
    async fn find_or_link_oauth_user_by_email(
        &self,
        oauth_info: &OAuthUserInfo,
    ) -> Result<Option<User>, AuthError> {
        if self.oauth_auto_linking_enabled
            && oauth_info.email_verified
            && matches!(oauth_info.provider.as_str(), "google" | "github")
        {
            if let Some(user) = self
                .user_repository
                .try_link_oauth_identity_by_email(oauth_info)
                .await
                .map_err(|e| {
                    AuthError::InternalError(format!("Failed to link OAuth identity: {e}"))
                })?
            {
                return self
                    .sync_existing_oauth_user(user, oauth_info)
                    .await
                    .map(Some);
            }
        }
        self.check_oauth_email_conflict(oauth_info).await?;
        Ok(None)
    }

    async fn check_oauth_email_conflict(
        &self,
        oauth_info: &OAuthUserInfo,
    ) -> Result<(), AuthError> {
        let existing = self
            .user_repository
            .get_by_email(&oauth_info.email)
            .await
            .map_err(|e| AuthError::InternalError(format!("Failed to check user email: {e}")))?;
        if let Some(user) = existing {
            // A matching identity may have been created concurrently. Let the
            // insert/re-read path reuse it. Other matches remain conflicts.
            if user.auth_provider != oauth_info.provider
                || user.provider_user_id != oauth_info.provider_user_id
            {
                return Err(AuthError::OAuthProviderConflict {
                    existing_provider: user.auth_provider,
                });
            }
        }
        Ok(())
    }

    async fn sync_existing_oauth_user(
        &self,
        user: User,
        oauth_info: &OAuthUserInfo,
    ) -> Result<User, AuthError> {
        // Linked identities authenticate the same account, but must not
        // overwrite the original provider's account email or profile.
        if user.auth_provider != oauth_info.provider
            || user.provider_user_id != oauth_info.provider_user_id
        {
            self.user_repository
                .observe_oauth_identity(user.id.clone(), oauth_info)
                .await
                .map_err(|e| AuthError::InternalError(format!("Failed to update identity: {e}")))?;
            self.user_repository
                .update_last_login(user.id.clone())
                .await
                .map_err(|e| {
                    AuthError::InternalError(format!("Failed to update last login: {e}"))
                })?;
            return Ok(user);
        }
        if user.email != oauth_info.email {
            self.check_oauth_email_conflict(oauth_info).await?;
            if let Err(error) = self
                .user_repository
                .update_email(user.id.clone(), oauth_info.email.clone())
                .await
            {
                // Another identity may have claimed the email after the lookup.
                // Infrastructure failures must not be classified as conflicts.
                if matches!(
                    error.downcast_ref::<crate::common::RepositoryError>(),
                    Some(crate::common::RepositoryError::AlreadyExists)
                ) {
                    self.check_oauth_email_conflict(oauth_info).await?;
                }
                return Err(AuthError::InternalError(format!(
                    "Failed to update user email: {error}"
                )));
            }
        }

        if user.display_name != oauth_info.display_name || user.avatar_url != oauth_info.avatar_url
        {
            self.user_repository
                .update(
                    user.id.clone(),
                    oauth_info.display_name.clone(),
                    oauth_info.avatar_url.clone(),
                )
                .await
                .map_err(|e| AuthError::InternalError(format!("Failed to update user: {e}")))?;
        }

        self.user_repository
            .update_last_login(user.id.clone())
            .await
            .map_err(|e| AuthError::InternalError(format!("Failed to update last login: {e}")))?;

        self.user_repository
            .observe_oauth_identity(user.id.clone(), oauth_info)
            .await
            .map_err(|e| AuthError::InternalError(format!("Failed to update identity: {e}")))?;
        Ok(user)
    }

    pub fn new(
        user_repository: Arc<dyn UserRepository>,
        session_repository: Arc<dyn SessionRepository>,
        api_key_repository: Arc<dyn ApiKeyRepository>,
        organization_repository: Arc<dyn OrganizationRepository>,
        workspace_repository: Arc<dyn WorkspaceRepository>,
        organization_service: Arc<dyn crate::organization::OrganizationServiceTrait>,
        auth_config: &config::AuthConfig,
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
            require_session_bound_access_tokens: auth_config.require_session_bound_access_tokens,
            oauth_auto_linking_enabled: auth_config.oauth_auto_linking_enabled,
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
    use crate::organization::{Organization, OrganizationId};
    use crate::workspace::{
        ApiKey, ApiKeyId, ApiKeyOrderBy, ApiKeyOrderDirection, ApiKeyRepository,
        CreateApiKeyRequest, Workspace, WorkspaceId, WorkspaceOrderBy, WorkspaceOrderDirection,
        WorkspaceRepository,
    };
    use bloomfilter::Bloom;
    use chrono::Utc;
    use std::sync::atomic::AtomicBool;
    use std::sync::Mutex;
    use tokio::sync::RwLock;
    use uuid::Uuid;

    #[test]
    fn refresh_successor_is_stable_and_keyed() {
        let first = refresh_token_successor("rt_original", "signing-key").unwrap();
        assert_eq!(
            first,
            refresh_token_successor("rt_original", "signing-key").unwrap()
        );
        assert!(first.starts_with("rt_"));
        assert_ne!(
            first,
            refresh_token_successor("rt_other", "signing-key").unwrap()
        );
        assert_ne!(
            first,
            refresh_token_successor("rt_original", "other-key").unwrap()
        );
    }

    fn make_user(email: &str, provider: &str) -> User {
        User {
            id: UserId(Uuid::new_v4()),
            email: email.to_string(),
            username: email.split('@').next().unwrap().to_string(),
            display_name: Some("Test User".to_string()),
            avatar_url: None,
            auth_provider: provider.to_string(),
            provider_user_id: email.to_string(),
            role: UserRole::User,
            is_active: true,
            last_login: None,
            created_at: Utc::now(),
            updated_at: Utc::now(),
            tokens_revoked_at: None,
            contact_email: None,
            contact_email_verified_at: None,
            email_opt_in_at: None,
        }
    }

    struct MockUserRepo {
        linked_identities: Mutex<Vec<OAuthUserInfo>>,
        user: Mutex<Option<User>>,
        email_updated: Mutex<Option<String>>,
        last_login_updated: Mutex<bool>,
        profile_updated: Mutex<bool>,
        provider_lookups: Mutex<usize>,
        creation_calls: Mutex<usize>,
        creation_race: bool,
        fallback_lookup_fails: bool,
        email_lookup_fails: bool,
        fallback_email_lookup_fails: bool,
        creation_unavailable: bool,
        email_owner: Mutex<Option<User>>,
        email_update_calls: Mutex<usize>,
        email_update_race: bool,
        email_update_unavailable: bool,
    }

    impl MockUserRepo {
        fn with_user(user: User) -> Self {
            Self {
                linked_identities: Mutex::new(Vec::new()),
                user: Mutex::new(Some(user)),
                email_updated: Mutex::new(None),
                last_login_updated: Mutex::new(false),
                profile_updated: Mutex::new(false),
                provider_lookups: Mutex::new(0),
                creation_calls: Mutex::new(0),
                creation_race: false,
                fallback_lookup_fails: false,
                email_lookup_fails: false,
                fallback_email_lookup_fails: false,
                creation_unavailable: false,
                email_owner: Mutex::new(None),
                email_update_calls: Mutex::new(0),
                email_update_race: false,
                email_update_unavailable: false,
            }
        }
    }

    impl MockUserRepo {
        fn creation_failure(user: Option<User>) -> Self {
            let mut repo = Self::with_user(
                user.clone()
                    .unwrap_or_else(|| make_user("unused@example.test", "near")),
            );
            repo.user = Mutex::new(user);
            repo.creation_race = true;
            repo
        }
    }

    #[async_trait]
    impl UserRepository for MockUserRepo {
        async fn try_link_oauth_identity_by_email(
            &self,
            info: &OAuthUserInfo,
        ) -> anyhow::Result<Option<User>> {
            let Some(user) = self.get_by_email(&info.email).await? else {
                return Ok(None);
            };
            if info.email_verified
                && matches!(info.provider.as_str(), "github" | "google")
                && matches!(user.auth_provider.as_str(), "github" | "google")
                && user.auth_provider != info.provider
            {
                self.linked_identities.lock().unwrap().push(info.clone());
                return Ok(Some(user));
            }
            Ok(None)
        }
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
            *self.creation_calls.lock().unwrap() += 1;
            if self.creation_unavailable {
                return Err(
                    RepositoryError::ConnectionFailed("test database unavailable".into()).into(),
                );
            }
            Err(anyhow::Error::new(RepositoryError::AlreadyExists)
                .context("test creation conflict"))
        }
        async fn get_by_id(&self, id: UserId) -> anyhow::Result<Option<User>> {
            let user = self.user.lock().unwrap();
            Ok(user.as_ref().filter(|u| u.id == id).cloned())
        }
        async fn get_by_email(&self, email: &str) -> anyhow::Result<Option<User>> {
            let created = *self.creation_calls.lock().unwrap() > 0;
            let updated = *self.email_update_calls.lock().unwrap() > 0;
            if self.email_lookup_fails || ((created || updated) && self.fallback_email_lookup_fails)
            {
                return Err(anyhow::anyhow!("test email lookup unavailable"));
            }
            if self.creation_race && !created {
                return Ok(None);
            }
            if self.email_update_race && *self.email_update_calls.lock().unwrap() == 0 {
                return Ok(None);
            }
            if let Some(owner) = self.email_owner.lock().unwrap().as_ref() {
                if owner.email == email && owner.is_active {
                    return Ok(Some(owner.clone()));
                }
            }
            Ok(self
                .user
                .lock()
                .unwrap()
                .as_ref()
                .filter(|u| u.email == email && u.is_active)
                .cloned())
        }
        async fn get_by_provider(
            &self,
            auth_provider: &str,
            provider_user_id: &str,
        ) -> anyhow::Result<Option<User>> {
            let mut calls = self.provider_lookups.lock().unwrap();
            *calls += 1;
            if self.creation_race && *calls == 1 {
                return Ok(None);
            }
            if self.fallback_lookup_fails {
                return Err(anyhow::anyhow!("test lookup unavailable"));
            }
            let user = self.user.lock().unwrap();
            Ok(user
                .as_ref()
                .filter(|u| {
                    u.is_active
                        && ((u.auth_provider == auth_provider
                            && u.provider_user_id == provider_user_id)
                            || self.linked_identities.lock().unwrap().iter().any(|info| {
                                info.provider == auth_provider
                                    && info.provider_user_id == provider_user_id
                            }))
                })
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
            *self.email_update_calls.lock().unwrap() += 1;
            if self.email_update_unavailable {
                return Err(RepositoryError::ConnectionFailed(
                    "test email update unavailable".into(),
                )
                .into());
            }
            if self.email_update_race
                || self
                    .email_owner
                    .lock()
                    .unwrap()
                    .as_ref()
                    .is_some_and(|owner| owner.email == email)
            {
                return Err(anyhow::Error::new(RepositoryError::AlreadyExists)
                    .context("test email update conflict"));
            }
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
        async fn rotate(
            &self,
            _: SessionId,
            _: &str,
            _: &str,
            _: i64,
        ) -> anyhow::Result<(Session, String)> {
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

    /// In-memory session repository for exercising session binding/revocation.
    struct InMemorySessionRepo {
        sessions: Mutex<std::collections::HashMap<Uuid, Session>>,
    }

    impl InMemorySessionRepo {
        fn new() -> Self {
            Self {
                sessions: Mutex::new(std::collections::HashMap::new()),
            }
        }

        fn insert_session(&self, user_id: UserId, expires_at: chrono::DateTime<Utc>) -> Session {
            let session = Session {
                id: SessionId(Uuid::new_v4()),
                user_id,
                token_hash: "test_token_hash".to_string(),
                created_at: Utc::now(),
                expires_at,
                ip_address: None,
                user_agent: "Test Agent".to_string(),
            };
            self.sessions
                .lock()
                .unwrap()
                .insert(session.id.0, session.clone());
            session
        }

        fn contains(&self, session_id: &SessionId) -> bool {
            self.sessions.lock().unwrap().contains_key(&session_id.0)
        }
    }

    #[async_trait]
    impl SessionRepository for InMemorySessionRepo {
        async fn create(
            &self,
            user_id: UserId,
            _: Option<String>,
            _: String,
            expires_in_hours: i64,
        ) -> anyhow::Result<(Session, String)> {
            let session = self.insert_session(
                user_id,
                Utc::now() + chrono::Duration::hours(expires_in_hours),
            );
            let token = format!("rt_{}", session.id.0.simple());
            Ok((session, token))
        }
        async fn validate(&self, _: SessionToken, _: &str) -> anyhow::Result<Option<Session>> {
            unimplemented!()
        }
        async fn get_by_id(&self, session_id: SessionId) -> anyhow::Result<Option<Session>> {
            Ok(self.sessions.lock().unwrap().get(&session_id.0).cloned())
        }
        async fn list_by_user(&self, _: UserId) -> anyhow::Result<Vec<Session>> {
            unimplemented!()
        }
        async fn extend(&self, _: SessionId, _: i64) -> anyhow::Result<bool> {
            unimplemented!()
        }
        async fn rotate(
            &self,
            _: SessionId,
            _: &str,
            _: &str,
            _: i64,
        ) -> anyhow::Result<(Session, String)> {
            unimplemented!()
        }
        async fn revoke(&self, session_id: SessionId) -> anyhow::Result<bool> {
            Ok(self
                .sessions
                .lock()
                .unwrap()
                .remove(&session_id.0)
                .is_some())
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
            _: Option<ApiKeyOrderBy>,
            _: Option<ApiKeyOrderDirection>,
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
        async fn list_by_user(&self, _: UserId, _: i64) -> Result<Vec<Workspace>, RepositoryError> {
            unimplemented!()
        }
    }

    fn build_auth_service(user_repo: Arc<MockUserRepo>) -> AuthService {
        build_auth_service_with_sessions(user_repo, Arc::new(StubSessionRepo), false)
    }

    fn build_auth_service_with_sessions(
        user_repo: Arc<MockUserRepo>,
        session_repo: Arc<dyn SessionRepository>,
        require_session_bound_access_tokens: bool,
    ) -> AuthService {
        let bloom = Bloom::new_for_fp_rate(100, 0.01).expect("bloom filter creation failed");
        AuthService {
            user_repository: user_repo,
            session_repository: session_repo,
            api_key_repository: Arc::new(StubApiKeyRepo),
            organization_repository: Arc::new(
                crate::organization::MockOrganizationRepository::new(),
            ),
            workspace_repository: Arc::new(StubWorkspaceRepo),
            organization_service: Arc::new(crate::organization::MockOrganizationServiceTrait::new()),
            api_key_cache: moka::future::Cache::builder().build(),
            api_key_bloom_filter: Arc::new(RwLock::new(bloom)),
            bloom_filter_ready: Arc::new(AtomicBool::new(false)),
            require_session_bound_access_tokens,
            oauth_auto_linking_enabled: true,
        }
    }

    #[tokio::test]
    async fn test_returning_user_matched_by_provider_identity() {
        let mut existing = make_user("alice@example.com", "google");
        existing.provider_user_id = "google-sub-123".into();
        let repo = Arc::new(MockUserRepo::with_user(existing.clone()));
        let service = build_auth_service(repo.clone());

        let result = service
            .get_or_create_oauth_user(OAuthUserInfo {
                email_verified: true,
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
        let mut existing = make_user("old@example.com", "google");
        existing.provider_user_id = "google-sub-456".into();
        let repo = Arc::new(MockUserRepo::with_user(existing.clone()));
        let service = build_auth_service(repo.clone());

        let result = service
            .get_or_create_oauth_user(OAuthUserInfo {
                email_verified: true,
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

    fn race_oauth_info() -> OAuthUserInfo {
        OAuthUserInfo {
            email_verified: true,
            provider: "near".into(),
            provider_user_id: "alice.near".into(),
            email: "alice.near@near".into(),
            username: "alice.near".into(),
            display_name: Some("Updated wallet profile".into()),
            avatar_url: Some("https://example.test/avatar".into()),
        }
    }

    #[tokio::test]
    async fn creation_race_reuses_identity_and_syncs_last_login_and_profile() {
        let mut existing = make_user("old@near", "near");
        existing.provider_user_id = "alice.near".into();
        let repo = Arc::new(MockUserRepo::creation_failure(Some(existing.clone())));
        let service = build_auth_service(repo.clone());
        let result = service
            .get_or_create_oauth_user(race_oauth_info())
            .await
            .unwrap();
        assert_eq!(result.id, existing.id);
        assert!(*repo.last_login_updated.lock().unwrap());
        assert!(*repo.profile_updated.lock().unwrap());
        assert_eq!(
            *repo.email_updated.lock().unwrap(),
            Some("alice.near@near".into())
        );
    }

    #[tokio::test]
    async fn creation_failure_without_matching_identity_preserves_original_error() {
        let repo = Arc::new(MockUserRepo::creation_failure(None));
        let error = build_auth_service(repo.clone())
            .get_or_create_oauth_user(race_oauth_info())
            .await
            .unwrap_err();
        assert!(error.to_string().contains("test creation conflict"));
        assert!(!*repo.last_login_updated.lock().unwrap());
    }

    #[tokio::test]
    async fn creation_race_lookup_failure_preserves_lookup_cause() {
        let mut repo = MockUserRepo::creation_failure(None);
        repo.fallback_lookup_fails = true;
        let error = build_auth_service(Arc::new(repo))
            .get_or_create_oauth_user(race_oauth_info())
            .await
            .unwrap_err();
        assert!(error.to_string().contains("test lookup unavailable"));
    }

    fn oauth_info(provider: &str) -> OAuthUserInfo {
        OAuthUserInfo {
            email_verified: true,
            provider: provider.into(),
            provider_user_id: format!("{provider}-alice"),
            email: "alice@example.com".into(),
            username: "alice".into(),
            display_name: Some("New profile".into()),
            avatar_url: Some("https://example.test/new-avatar".into()),
        }
    }

    #[tokio::test]
    async fn oauth_disabled_auto_linking_rejects_verified_emails_including_creation_races() {
        for (original, incoming) in [("github", "google"), ("google", "github")] {
            for creation_race in [false, true] {
                let existing = make_user("alice@example.com", original);
                let repo = Arc::new(if creation_race {
                    MockUserRepo::creation_failure(Some(existing))
                } else {
                    MockUserRepo::with_user(existing)
                });
                let mut service = build_auth_service(repo.clone());
                service.oauth_auto_linking_enabled = false;
                let error = service
                    .get_or_create_oauth_user(oauth_info(incoming))
                    .await
                    .unwrap_err();
                assert!(matches!(
                    error,
                    AuthError::OAuthProviderConflict { existing_provider } if existing_provider == original
                ));
                assert!(repo.linked_identities.lock().unwrap().is_empty());
                assert_eq!(
                    *repo.creation_calls.lock().unwrap(),
                    usize::from(creation_race)
                );
                assert!(!*repo.last_login_updated.lock().unwrap());
            }
        }
    }

    #[tokio::test]
    async fn oauth_disabled_auto_linking_preserves_linked_provider_login() {
        for (original, incoming) in [("github", "google"), ("google", "github")] {
            let existing = make_user("alice@example.com", original);
            let repo = Arc::new(MockUserRepo::with_user(existing.clone()));
            let info = oauth_info(incoming);
            repo.linked_identities.lock().unwrap().push(info.clone());
            let mut service = build_auth_service(repo.clone());
            service.oauth_auto_linking_enabled = false;
            let user = service.get_or_create_oauth_user(info).await.unwrap();
            assert_eq!(user.id, existing.id);
            assert_eq!(repo.linked_identities.lock().unwrap().len(), 1);
            assert_eq!(*repo.creation_calls.lock().unwrap(), 0);
            assert!(*repo.last_login_updated.lock().unwrap());
        }
    }

    #[tokio::test]
    async fn oauth_verified_email_links_both_providers_without_reprovisioning_account() {
        for (original, incoming) in [("github", "google"), ("google", "github")] {
            let existing = make_user("alice@example.com", original);
            let repo = Arc::new(MockUserRepo::with_user(existing.clone()));
            let user = build_auth_service(repo.clone())
                .get_or_create_oauth_user(oauth_info(incoming))
                .await
                .unwrap();

            assert_eq!(user.id, existing.id);
            assert_eq!(repo.linked_identities.lock().unwrap().len(), 1);
            assert_eq!(*repo.creation_calls.lock().unwrap(), 0);
            assert!(*repo.last_login_updated.lock().unwrap());
            assert!(!*repo.profile_updated.lock().unwrap());
            assert!(repo.email_updated.lock().unwrap().is_none());
            let stored = repo.user.lock().unwrap().clone().unwrap();
            assert_eq!(stored.id, existing.id);
            assert_eq!(stored.auth_provider, original);
            assert_eq!(stored.provider_user_id, existing.provider_user_id);
            // Organization/workspace/session stubs panic if provisioning is attempted.
        }
    }

    #[tokio::test]
    async fn oauth_unverified_email_never_links_an_existing_account() {
        for (original, incoming) in [("github", "google"), ("google", "github")] {
            let existing = make_user("alice@example.com", original);
            let repo = Arc::new(MockUserRepo::with_user(existing));
            let mut info = oauth_info(incoming);
            info.email_verified = false;
            let error = build_auth_service(repo.clone())
                .get_or_create_oauth_user(info)
                .await
                .unwrap_err();
            assert!(matches!(error, AuthError::OAuthProviderConflict { .. }));
            assert!(repo.linked_identities.lock().unwrap().is_empty());
            assert_eq!(*repo.creation_calls.lock().unwrap(), 0);
            assert!(!*repo.last_login_updated.lock().unwrap());
        }
    }

    #[tokio::test]
    async fn oauth_verified_email_claimed_concurrently_links_in_both_directions() {
        for (original, incoming) in [("github", "google"), ("google", "github")] {
            let repo = Arc::new(MockUserRepo::creation_failure(Some(make_user(
                "alice@example.com",
                original,
            ))));
            let user = build_auth_service(repo.clone())
                .get_or_create_oauth_user(oauth_info(incoming))
                .await
                .unwrap();

            assert_eq!(user.auth_provider, original);
            assert_eq!(repo.linked_identities.lock().unwrap().len(), 1);
            assert_eq!(*repo.creation_calls.lock().unwrap(), 1);
            assert_eq!(*repo.provider_lookups.lock().unwrap(), 2);
            assert!(*repo.last_login_updated.lock().unwrap());
            assert!(!*repo.profile_updated.lock().unwrap());
        }
    }

    #[tokio::test]
    async fn oauth_same_identity_created_concurrently_is_reused_for_both_providers() {
        for provider in ["github", "google"] {
            let info = oauth_info(provider);
            let mut existing = make_user(&info.email, provider);
            existing.provider_user_id = info.provider_user_id.clone();
            let repo = Arc::new(MockUserRepo::creation_failure(Some(existing.clone())));
            let user = build_auth_service(repo.clone())
                .get_or_create_oauth_user(info)
                .await
                .unwrap();

            assert_eq!(user.id, existing.id);
            assert_eq!(*repo.creation_calls.lock().unwrap(), 1);
            assert!(*repo.last_login_updated.lock().unwrap());
        }
    }

    #[tokio::test]
    async fn oauth_email_does_not_authenticate_another_identity_on_the_same_provider() {
        let repo = Arc::new(MockUserRepo::with_user(make_user(
            "alice@example.com",
            "github",
        )));
        let error = build_auth_service(repo.clone())
            .get_or_create_oauth_user(oauth_info("github"))
            .await
            .unwrap_err();
        assert!(matches!(error, AuthError::OAuthProviderConflict { .. }));
        assert_eq!(*repo.creation_calls.lock().unwrap(), 0);
        assert!(!*repo.last_login_updated.lock().unwrap());
    }

    #[tokio::test]
    async fn oauth_existing_identity_is_resolved_before_email_lookup() {
        let info = oauth_info("github");
        let mut existing = make_user(&info.email, "github");
        existing.provider_user_id = info.provider_user_id.clone();
        let mut repo = MockUserRepo::with_user(existing.clone());
        repo.email_lookup_fails = true;
        let user = build_auth_service(Arc::new(repo))
            .get_or_create_oauth_user(info)
            .await
            .unwrap();
        assert_eq!(user.id, existing.id);
    }

    #[tokio::test]
    async fn oauth_database_failure_is_not_misclassified_as_email_conflict() {
        let mut repo =
            MockUserRepo::creation_failure(Some(make_user("alice@example.com", "google")));
        repo.creation_unavailable = true;
        let error = build_auth_service(Arc::new(repo))
            .get_or_create_oauth_user(oauth_info("github"))
            .await
            .unwrap_err();
        assert!(matches!(error, AuthError::InternalError(ref message)
            if message.contains("test database unavailable")));
    }

    #[tokio::test]
    async fn oauth_email_lookup_failure_remains_internal_before_and_after_creation() {
        for after_creation in [false, true] {
            let mut repo = MockUserRepo::creation_failure(None);
            repo.email_lookup_fails = !after_creation;
            repo.fallback_email_lookup_fails = after_creation;
            let repo = Arc::new(repo);
            let error = build_auth_service(repo.clone())
                .get_or_create_oauth_user(oauth_info("github"))
                .await
                .unwrap_err();
            assert!(matches!(error, AuthError::InternalError(ref message)
                if message.contains("test email lookup unavailable")));
            assert_eq!(
                *repo.creation_calls.lock().unwrap(),
                usize::from(after_creation)
            );
        }
    }

    const TEST_ENCODING_KEY: &str = "test_encoding_key";

    #[tokio::test]
    async fn returning_oauth_user_email_conflict_leaves_login_and_profile_unchanged() {
        for (provider, owner_provider) in [
            ("google", "github"),
            ("github", "google"),
            ("google", "google"),
            ("github", "github"),
        ] {
            let info = oauth_info(provider);
            let mut existing = make_user("original@example.com", provider);
            existing.provider_user_id = info.provider_user_id.clone();
            let repo = MockUserRepo::with_user(existing.clone());
            *repo.email_owner.lock().unwrap() = Some(make_user(&info.email, owner_provider));
            let repo = Arc::new(repo);
            let error = build_auth_service(repo.clone())
                .get_or_create_oauth_user(info)
                .await
                .unwrap_err();
            assert!(
                matches!(error, AuthError::OAuthProviderConflict { existing_provider }
                if existing_provider == owner_provider)
            );
            assert_eq!(*repo.email_update_calls.lock().unwrap(), 0);
            assert!(!*repo.last_login_updated.lock().unwrap());
            assert!(!*repo.profile_updated.lock().unwrap());
            assert!(repo.email_updated.lock().unwrap().is_none());
            assert_eq!(
                repo.user.lock().unwrap().as_ref().unwrap().email,
                existing.email
            );
        }
    }

    #[tokio::test]
    async fn returning_oauth_user_email_claimed_during_update_returns_conflict() {
        for (provider, owner_provider) in [("google", "github"), ("github", "google")] {
            let info = oauth_info(provider);
            let mut existing = make_user("original@example.com", provider);
            existing.provider_user_id = info.provider_user_id.clone();
            let mut repo = MockUserRepo::with_user(existing);
            *repo.email_owner.lock().unwrap() = Some(make_user(&info.email, owner_provider));
            repo.email_update_race = true;
            let repo = Arc::new(repo);
            let error = build_auth_service(repo.clone())
                .get_or_create_oauth_user(info)
                .await
                .unwrap_err();
            assert!(
                matches!(error, AuthError::OAuthProviderConflict { existing_provider }
                if existing_provider == owner_provider)
            );
            assert_eq!(*repo.email_update_calls.lock().unwrap(), 1);
            assert!(!*repo.last_login_updated.lock().unwrap());
            assert!(!*repo.profile_updated.lock().unwrap());
            assert!(repo.email_updated.lock().unwrap().is_none());
        }
    }

    #[tokio::test]
    async fn returning_oauth_user_email_update_failure_stays_internal_without_login_mutation() {
        let info = oauth_info("google");
        let mut existing = make_user("original@example.com", "google");
        existing.provider_user_id = info.provider_user_id.clone();
        let mut repo = MockUserRepo::with_user(existing);
        repo.email_update_unavailable = true;
        repo.email_update_race = true;
        *repo.email_owner.lock().unwrap() = Some(make_user(&info.email, "github"));
        let repo = Arc::new(repo);
        let error = build_auth_service(repo.clone())
            .get_or_create_oauth_user(info)
            .await
            .unwrap_err();
        assert!(matches!(error, AuthError::InternalError(ref message)
            if message.contains("test email update unavailable")));
        assert!(!*repo.last_login_updated.lock().unwrap());
        assert!(!*repo.profile_updated.lock().unwrap());
    }

    #[tokio::test]
    async fn returning_oauth_user_email_lookup_failure_leaves_login_and_profile_unchanged() {
        for race in [false, true] {
            let info = oauth_info("google");
            let mut existing = make_user("original@example.com", "google");
            existing.provider_user_id = info.provider_user_id.clone();
            let mut repo = MockUserRepo::with_user(existing);
            repo.email_update_race = race;
            repo.email_lookup_fails = !race;
            repo.fallback_email_lookup_fails = race;
            let repo = Arc::new(repo);
            let error = build_auth_service(repo.clone())
                .get_or_create_oauth_user(info)
                .await
                .unwrap_err();
            assert!(matches!(error, AuthError::InternalError(ref message)
                if message.contains("test email lookup unavailable")));
            assert_eq!(*repo.email_update_calls.lock().unwrap(), usize::from(race));
            assert!(!*repo.last_login_updated.lock().unwrap());
            assert!(!*repo.profile_updated.lock().unwrap());
            assert!(repo.email_updated.lock().unwrap().is_none());
        }
    }

    #[tokio::test]
    async fn inactive_oauth_email_owner_is_never_authenticated_or_reactivated() {
        let mut inactive = make_user("alice@example.com", "github");
        inactive.is_active = false;
        let repo = Arc::new(MockUserRepo::with_user(inactive.clone()));
        let error = build_auth_service(repo.clone())
            .get_or_create_oauth_user(oauth_info("google"))
            .await
            .unwrap_err();
        assert!(matches!(error, AuthError::InternalError(_)));
        assert_eq!(*repo.creation_calls.lock().unwrap(), 1);
        assert!(!*repo.last_login_updated.lock().unwrap());
        assert!(!repo.user.lock().unwrap().as_ref().unwrap().is_active);
    }

    #[tokio::test]
    async fn test_create_session_binds_access_token_to_session() {
        let user = make_user("alice@example.com", "google");
        let user_repo = Arc::new(MockUserRepo::with_user(user.clone()));
        let session_repo = Arc::new(InMemorySessionRepo::new());
        let service = build_auth_service_with_sessions(user_repo, session_repo.clone(), false);

        let (access_token, session, refresh_token) = service
            .create_session(
                user.id.clone(),
                None,
                "Test Agent".to_string(),
                TEST_ENCODING_KEY.to_string(),
                1,
                24,
            )
            .await
            .unwrap();
        assert!(refresh_token.starts_with("rt_"));

        // The access token carries the sid claim of the session it was minted from.
        let claims = service
            .validate_session_access_token(access_token.clone(), TEST_ENCODING_KEY.to_string())
            .unwrap()
            .unwrap();
        assert_eq!(claims.sid.as_ref().map(|s| s.0), Some(session.id.0));

        // And it validates while the session is live.
        let validated = service
            .validate_session_access(access_token, TEST_ENCODING_KEY.to_string())
            .await
            .unwrap();
        assert_eq!(validated.id, user.id);
    }

    #[tokio::test]
    async fn test_logout_revokes_session_and_invalidates_access_token() {
        let user = make_user("alice@example.com", "google");
        let user_repo = Arc::new(MockUserRepo::with_user(user.clone()));
        let session_repo = Arc::new(InMemorySessionRepo::new());
        let service = build_auth_service_with_sessions(user_repo, session_repo.clone(), false);

        let (access_token, session, _refresh_token) = service
            .create_session(
                user.id.clone(),
                None,
                "Test Agent".to_string(),
                TEST_ENCODING_KEY.to_string(),
                1,
                24,
            )
            .await
            .unwrap();

        assert!(service.logout(session.id.clone()).await.unwrap());
        assert!(!session_repo.contains(&session.id));

        // The still-unexpired access token is rejected once its session is gone.
        let result = service
            .validate_session_access(access_token, TEST_ENCODING_KEY.to_string())
            .await;
        assert!(matches!(result, Err(AuthError::SessionNotFound)));

        // Repeated logout is safe: reports nothing revoked, restores nothing.
        assert!(!service.logout(session.id.clone()).await.unwrap());
        assert!(!session_repo.contains(&session.id));
    }

    #[tokio::test]
    async fn test_logout_revokes_only_target_session() {
        let user = make_user("alice@example.com", "google");
        let user_repo = Arc::new(MockUserRepo::with_user(user.clone()));
        let session_repo = Arc::new(InMemorySessionRepo::new());
        let service = build_auth_service_with_sessions(user_repo, session_repo.clone(), false);

        let (token_a, session_a, _) = service
            .create_session(
                user.id.clone(),
                None,
                "Test Agent".to_string(),
                TEST_ENCODING_KEY.to_string(),
                1,
                24,
            )
            .await
            .unwrap();
        let (token_b, session_b, _) = service
            .create_session(
                user.id.clone(),
                None,
                "Test Agent".to_string(),
                TEST_ENCODING_KEY.to_string(),
                1,
                24,
            )
            .await
            .unwrap();

        assert!(service.logout(session_a.id.clone()).await.unwrap());

        // Session A's access token dies immediately; session B is untouched.
        assert!(matches!(
            service
                .validate_session_access(token_a, TEST_ENCODING_KEY.to_string())
                .await,
            Err(AuthError::SessionNotFound)
        ));
        assert!(session_repo.contains(&session_b.id));
        assert!(service
            .validate_session_access(token_b, TEST_ENCODING_KEY.to_string())
            .await
            .is_ok());
    }

    #[tokio::test]
    async fn test_access_token_rejected_when_session_expired() {
        let user = make_user("alice@example.com", "google");
        let user_repo = Arc::new(MockUserRepo::with_user(user.clone()));
        let session_repo = Arc::new(InMemorySessionRepo::new());
        let service = build_auth_service_with_sessions(user_repo, session_repo.clone(), false);

        // Session row exists but is already expired.
        let expired_session =
            session_repo.insert_session(user.id.clone(), Utc::now() - chrono::Duration::hours(1));
        let access_token = service
            .create_session_access_token(
                user.id.clone(),
                Some(expired_session.id),
                TEST_ENCODING_KEY.to_string(),
                1,
            )
            .unwrap();

        let result = service
            .validate_session_access(access_token, TEST_ENCODING_KEY.to_string())
            .await;
        assert!(matches!(result, Err(AuthError::SessionNotFound)));
    }

    #[tokio::test]
    async fn test_legacy_access_token_without_sid_follows_compat_flag() {
        let user = make_user("alice@example.com", "google");

        // Flag off (default): legacy tokens keep validating during the cutover window.
        let service = build_auth_service_with_sessions(
            Arc::new(MockUserRepo::with_user(user.clone())),
            Arc::new(InMemorySessionRepo::new()),
            false,
        );
        let legacy_token = service
            .create_session_access_token(user.id.clone(), None, TEST_ENCODING_KEY.to_string(), 1)
            .unwrap();
        assert!(service
            .validate_session_access(legacy_token.clone(), TEST_ENCODING_KEY.to_string())
            .await
            .is_ok());

        // Flag on: tokens that cannot be tied to a live session are rejected.
        let strict_service = build_auth_service_with_sessions(
            Arc::new(MockUserRepo::with_user(user.clone())),
            Arc::new(InMemorySessionRepo::new()),
            true,
        );
        assert!(matches!(
            strict_service
                .validate_session_access(legacy_token, TEST_ENCODING_KEY.to_string())
                .await,
            Err(AuthError::SessionNotFound)
        ));
    }

    #[tokio::test]
    async fn test_access_token_rejected_when_sid_belongs_to_other_user() {
        let user = make_user("alice@example.com", "google");
        let user_repo = Arc::new(MockUserRepo::with_user(user.clone()));
        let session_repo = Arc::new(InMemorySessionRepo::new());
        let service = build_auth_service_with_sessions(user_repo, session_repo.clone(), false);

        // Session belongs to a different user than the token subject.
        let other_session = session_repo.insert_session(
            UserId(Uuid::new_v4()),
            Utc::now() + chrono::Duration::hours(24),
        );
        let access_token = service
            .create_session_access_token(
                user.id.clone(),
                Some(other_session.id),
                TEST_ENCODING_KEY.to_string(),
                1,
            )
            .unwrap();

        let result = service
            .validate_session_access(access_token, TEST_ENCODING_KEY.to_string())
            .await;
        assert!(matches!(result, Err(AuthError::SessionNotFound)));
    }
}
