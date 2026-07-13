use anyhow::Result;
use chrono::{DateTime, Utc};
use services::reporting_tokens::ports::{
    OrganizationReportingToken, ValidatedOrganizationReportingToken,
    REPORTING_TOKEN_SCOPE_USAGE_READ,
};
use uuid::Uuid;

pub struct OrganizationReportingTokenRow {
    id: Uuid,
    organization_id: Uuid,
    name: String,
    token_prefix: String,
    created_by_user_id: Uuid,
    created_at: DateTime<Utc>,
    expires_at: Option<DateTime<Utc>>,
    last_used_at: Option<DateTime<Utc>>,
    revoked_at: Option<DateTime<Utc>>,
    revoked_by_user_id: Option<Uuid>,
}

impl OrganizationReportingTokenRow {
    pub fn from_row(row: tokio_postgres::Row) -> Result<Self> {
        Ok(Self {
            id: row.get("id"),
            organization_id: row.get("organization_id"),
            name: row.get("name"),
            token_prefix: row.get("token_prefix"),
            created_by_user_id: row.get("created_by_user_id"),
            created_at: row.get("created_at"),
            expires_at: row.get("expires_at"),
            last_used_at: row.get("last_used_at"),
            revoked_at: row.get("revoked_at"),
            revoked_by_user_id: row.get("revoked_by_user_id"),
        })
    }

    pub fn into_service_token(self) -> OrganizationReportingToken {
        OrganizationReportingToken {
            id: self.id,
            organization_id: self.organization_id,
            name: self.name,
            token_prefix: self.token_prefix,
            created_by_user_id: self.created_by_user_id,
            created_at: self.created_at,
            expires_at: self.expires_at,
            last_used_at: self.last_used_at,
            revoked_at: self.revoked_at,
            revoked_by_user_id: self.revoked_by_user_id,
            scope: REPORTING_TOKEN_SCOPE_USAGE_READ,
        }
    }

    pub fn into_validated_token(self) -> ValidatedOrganizationReportingToken {
        ValidatedOrganizationReportingToken {
            id: self.id,
            organization_id: self.organization_id,
            token_prefix: self.token_prefix,
            last_used_at: self.last_used_at,
            scope: REPORTING_TOKEN_SCOPE_USAGE_READ,
        }
    }
}
