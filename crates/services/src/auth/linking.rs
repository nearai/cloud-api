use super::*;
use rand::RngExt;

impl AuthService {
    pub(super) async fn validate_link_session(
        &self,
        user_id: UserId,
        session_id: SessionId,
    ) -> Result<(), AuthError> {
        let user = self.get_user_by_id(user_id.clone()).await?;
        if !user.is_active || !matches!(user.auth_provider.as_str(), "google" | "github") {
            return Err(AuthError::OAuthLinkNotAllowed);
        }
        let session = self
            .session_repository
            .get_by_id(session_id)
            .await
            .map_err(|e| AuthError::InternalError(format!("Failed to validate link session: {e}")))?
            .ok_or(AuthError::OAuthLinkInvalid)?;
        if session.user_id != user_id
            || session.expires_at <= Utc::now()
            || user
                .tokens_revoked_at
                .is_some_and(|revoked| session.created_at <= revoked)
        {
            return Err(AuthError::OAuthLinkInvalid);
        }
        Ok(())
    }

    pub(super) async fn prepare_link(
        &self,
        user_id: UserId,
        session_id: SessionId,
        info: OAuthUserInfo,
    ) -> Result<String, AuthError> {
        self.validate_link_session(user_id.clone(), session_id.clone())
            .await?;
        if !matches!(info.provider.as_str(), "google" | "github") {
            return Err(AuthError::OAuthLinkNotAllowed);
        }
        if !info.email_verified || info.email.is_empty() {
            return Err(AuthError::OAuthEmailUnverified);
        }
        let code = hex::encode(rand::rng().random::<[u8; 32]>());
        let code_hash = hex::encode(Sha256::digest(code.as_bytes()));
        self.user_repository
            .store_oauth_link_request(user_id, session_id, &code_hash, &info)
            .await
            .map_err(|e| {
                AuthError::InternalError(format!("Failed to prepare account link: {e}"))
            })?;
        Ok(code)
    }
}
