use async_trait::async_trait;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;
use uuid::Uuid;

use crate::common::RepositoryError;

pub const REPORTING_TOKEN_SCOPE_USAGE_READ: ReportingTokenScope = ReportingTokenScope::UsageRead;

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, ToSchema)]
pub enum ReportingTokenScope {
    #[serde(rename = "usage:read")]
    UsageRead,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct OrganizationReportingToken {
    pub id: Uuid,
    pub organization_id: Uuid,
    pub name: String,
    pub token_prefix: String,
    pub created_by_user_id: Uuid,
    pub created_at: DateTime<Utc>,
    pub expires_at: Option<DateTime<Utc>>,
    pub last_used_at: Option<DateTime<Utc>>,
    pub revoked_at: Option<DateTime<Utc>>,
    pub revoked_by_user_id: Option<Uuid>,
    pub scope: ReportingTokenScope,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct CreatedOrganizationReportingToken {
    pub token: OrganizationReportingToken,
    pub raw_token: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ValidatedOrganizationReportingToken {
    pub id: Uuid,
    pub organization_id: Uuid,
    pub token_prefix: String,
    pub last_used_at: Option<DateTime<Utc>>,
    pub scope: ReportingTokenScope,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct CreateOrganizationReportingTokenRequest {
    pub organization_id: Uuid,
    pub name: String,
    pub created_by_user_id: Uuid,
    pub expires_at: Option<DateTime<Utc>>,
}

#[async_trait]
pub trait OrganizationReportingTokenRepository: Send + Sync {
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
