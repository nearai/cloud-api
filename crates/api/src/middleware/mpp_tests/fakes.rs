//! Fakes for the authentication state the middleware tests run against: one service key and
//! one customer key that validate, nothing else.

use super::*;

pub(super) struct Keys(Option<Arc<AtomicUsize>>);

pub(super) fn api_key(key: &str) -> ApiKey {
    ApiKey {
        id: ApiKeyId(Uuid::from_u128(5).to_string()),
        key: None,
        key_prefix: key.chars().take(12).collect(),
        name: "k".to_string(),
        workspace_id: WorkspaceId(Uuid::from_u128(8)),
        created_by_user_id: services::auth::UserId(Uuid::nil()),
        created_at: Utc::now(),
        expires_at: None,
        last_used_at: None,
        is_active: true,
        deleted_at: None,
        spend_limit: None,
        usage: None,
    }
}

#[async_trait]
impl ApiKeyRepository for Keys {
    async fn validate(&self, key: String) -> Result<Option<ApiKey>, RepositoryError> {
        if let Some(calls) = &self.0 {
            calls.fetch_add(1, Ordering::SeqCst);
        }
        Ok((key == SERVICE_KEY || key == BEARER_KEY).then(|| api_key(&key)))
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
        Ok(())
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
        _: Option<Option<DateTime<Utc>>>,
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
        _: DateTime<Utc>,
    ) -> Result<Vec<String>, RepositoryError> {
        Ok(vec![])
    }
}

pub(super) struct Workspaces;

#[async_trait]
impl WorkspaceRepository for Workspaces {
    async fn get_by_id(&self, _: WorkspaceId) -> Result<Option<Workspace>, RepositoryError> {
        unimplemented!()
    }
    async fn get_by_name(&self, _: Uuid, _: &str) -> Result<Option<Workspace>, RepositoryError> {
        unimplemented!()
    }
    async fn get_workspace_with_organization(
        &self,
        id: WorkspaceId,
    ) -> Result<Option<(Workspace, Organization)>, RepositoryError> {
        let owner = services::auth::UserId(Uuid::nil());
        Ok(Some((
            Workspace {
                id,
                name: "w".to_string(),
                description: None,
                organization_id: OrganizationId(Uuid::from_u128(9)),
                created_by_user_id: owner.clone(),
                created_at: Utc::now(),
                updated_at: Utc::now(),
                is_active: true,
                settings: None,
            },
            Organization {
                request_priority: 0,
                id: OrganizationId(Uuid::from_u128(9)),
                name: "o".to_string(),
                description: None,
                owner_id: owner,
                settings: json!({}),
                is_active: true,
                created_at: Utc::now(),
                updated_at: Utc::now(),
            },
        )))
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
        _: services::auth::UserId,
    ) -> Result<Workspace, RepositoryError> {
        unimplemented!()
    }
    async fn update(
        &self,
        _: WorkspaceId,
        _: Option<String>,
        _: Option<String>,
        _: Option<Value>,
    ) -> Result<Option<Workspace>, RepositoryError> {
        unimplemented!()
    }
    async fn delete(&self, _: WorkspaceId) -> Result<bool, RepositoryError> {
        unimplemented!()
    }
    async fn count_by_organization(&self, _: OrganizationId) -> Result<i64, RepositoryError> {
        unimplemented!()
    }
    async fn list_by_user(
        &self,
        _: services::auth::UserId,
        _: i64,
    ) -> Result<Vec<Workspace>, RepositoryError> {
        unimplemented!()
    }
}

pub(super) struct NoReportingTokens;

#[async_trait]
impl OrganizationReportingTokenService for NoReportingTokens {
    async fn create(
        &self,
        _: CreateOrganizationReportingTokenRequest,
    ) -> Result<CreatedOrganizationReportingToken, RepositoryError> {
        unimplemented!()
    }
    async fn validate(
        &self,
        _: &str,
    ) -> Result<Option<ValidatedOrganizationReportingToken>, RepositoryError> {
        Ok(None)
    }
    async fn get_by_id(
        &self,
        _: Uuid,
    ) -> Result<Option<OrganizationReportingToken>, RepositoryError> {
        unimplemented!()
    }
    async fn list_active_by_organization(
        &self,
        _: Uuid,
    ) -> Result<Vec<OrganizationReportingToken>, RepositoryError> {
        unimplemented!()
    }
    async fn revoke(&self, _: Uuid, _: Uuid) -> Result<bool, RepositoryError> {
        unimplemented!()
    }
}

pub(super) fn auth_state() -> AuthState {
    auth_state_with_calls(None)
}

pub(super) fn auth_state_with_calls(calls: Option<Arc<AtomicUsize>>) -> AuthState {
    AuthState::new(
        Arc::new(services::auth::OAuthManager::new(None, None).unwrap()),
        Arc::new(services::auth::MockAuthService {
            apikey_repository: Arc::new(Keys(calls)),
        }),
        Arc::new(Workspaces),
        Arc::new(NoReportingTokens),
        Arc::new(database::repositories::AdminAccessTokenRepository::new(
            database::DbPool::uninitialized(),
        )),
        Vec::new(),
        String::new(),
    )
}
