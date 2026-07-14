use super::{
    CreateOrganizationReportingTokenRequest, CreatedOrganizationReportingToken,
    OrganizationReportingToken, OrganizationReportingTokenRepository,
    ValidatedOrganizationReportingToken,
};
use crate::common::{is_valid_reporting_token_format, RepositoryError};
use async_trait::async_trait;
use chrono::Utc;
use std::sync::Arc;
use uuid::Uuid;

#[async_trait]
pub trait OrganizationReportingTokenService: Send + Sync {
    async fn create(
        &self,
        request: CreateOrganizationReportingTokenRequest,
    ) -> Result<CreatedOrganizationReportingToken, RepositoryError>;

    async fn validate(
        &self,
        raw_token: &str,
    ) -> Result<Option<ValidatedOrganizationReportingToken>, RepositoryError>;

    async fn get_by_id(
        &self,
        token_id: Uuid,
    ) -> Result<Option<OrganizationReportingToken>, RepositoryError>;

    async fn list_active_by_organization(
        &self,
        organization_id: Uuid,
    ) -> Result<Vec<OrganizationReportingToken>, RepositoryError>;

    async fn revoke(
        &self,
        token_id: Uuid,
        revoked_by_user_id: Uuid,
    ) -> Result<bool, RepositoryError>;
}

pub struct OrganizationReportingTokenServiceImpl {
    repository: Arc<dyn OrganizationReportingTokenRepository>,
}

impl OrganizationReportingTokenServiceImpl {
    pub fn new(repository: Arc<dyn OrganizationReportingTokenRepository>) -> Self {
        Self { repository }
    }
}

#[async_trait]
impl OrganizationReportingTokenService for OrganizationReportingTokenServiceImpl {
    async fn create(
        &self,
        mut request: CreateOrganizationReportingTokenRequest,
    ) -> Result<CreatedOrganizationReportingToken, RepositoryError> {
        request.name = request.name.trim().to_string();
        if request.name.is_empty() {
            return Err(RepositoryError::ValidationFailed(
                "reporting token name cannot be empty".to_string(),
            ));
        }
        if request.name.chars().count() > 255 {
            return Err(RepositoryError::ValidationFailed(
                "reporting token name must be at most 255 characters".to_string(),
            ));
        }
        if request
            .expires_at
            .is_some_and(|expires_at| expires_at <= Utc::now())
        {
            return Err(RepositoryError::ValidationFailed(
                "reporting token expiration must be in the future".to_string(),
            ));
        }
        self.repository.create(request).await
    }

    async fn validate(
        &self,
        raw_token: &str,
    ) -> Result<Option<ValidatedOrganizationReportingToken>, RepositoryError> {
        if !is_valid_reporting_token_format(raw_token) {
            return Ok(None);
        }
        self.repository.validate(raw_token).await
    }

    async fn get_by_id(
        &self,
        token_id: Uuid,
    ) -> Result<Option<OrganizationReportingToken>, RepositoryError> {
        self.repository.get_by_id(token_id).await
    }

    async fn list_active_by_organization(
        &self,
        organization_id: Uuid,
    ) -> Result<Vec<OrganizationReportingToken>, RepositoryError> {
        self.repository
            .list_active_by_organization(organization_id)
            .await
    }

    async fn revoke(
        &self,
        token_id: Uuid,
        revoked_by_user_id: Uuid,
    ) -> Result<bool, RepositoryError> {
        self.repository.revoke(token_id, revoked_by_user_id).await
    }
}
