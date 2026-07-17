use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use services::reporting_tokens::ReportingTokenScope;
use utoipa::ToSchema;
use uuid::Uuid;

#[derive(Debug, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct CreateReportingTokenRequest {
    /// Human-readable name for the reporting token.
    pub name: String,
    /// Optional expiration time. Must be in the future.
    pub expires_at: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct ReportingTokenResponse {
    pub id: Uuid,
    pub organization_id: Uuid,
    pub name: String,
    /// Non-secret token prefix for display and audit correlation.
    pub token_prefix: String,
    pub created_by_user_id: Uuid,
    pub created_at: DateTime<Utc>,
    pub expires_at: Option<DateTime<Utc>>,
    /// Approximate last-authentication timestamp, refreshed at most once every 15 minutes.
    pub last_used_at: Option<DateTime<Utc>>,
    pub scope: ReportingTokenScope,
}

#[derive(Serialize, ToSchema)]
pub struct CreateReportingTokenResponse {
    pub id: Uuid,
    pub organization_id: Uuid,
    pub name: String,
    /// Raw reporting token. Returned only once when the token is created.
    pub token: String,
    /// Non-secret token prefix for display and audit correlation.
    pub token_prefix: String,
    pub created_by_user_id: Uuid,
    pub created_at: DateTime<Utc>,
    pub expires_at: Option<DateTime<Utc>>,
    /// Approximate last-authentication timestamp, refreshed at most once every 15 minutes.
    pub last_used_at: Option<DateTime<Utc>>,
    pub scope: ReportingTokenScope,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct ListReportingTokensResponse {
    pub reporting_tokens: Vec<ReportingTokenResponse>,
    pub total: i64,
}

impl CreateReportingTokenRequest {
    pub fn validate(&self) -> Result<(), String> {
        let name = self.name.trim();
        if name.is_empty() {
            return Err("name is required".to_string());
        }
        if name.chars().count() > 255 {
            return Err("name must be at most 255 characters".to_string());
        }
        if let Some(expires_at) = self.expires_at {
            if expires_at <= Utc::now() {
                return Err("expires_at must be in the future".to_string());
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reporting_token_name_limit_counts_characters_not_utf8_bytes() {
        let valid = CreateReportingTokenRequest {
            name: "é".repeat(255),
            expires_at: None,
        };
        let invalid = CreateReportingTokenRequest {
            name: "é".repeat(256),
            expires_at: None,
        };

        assert!(valid.validate().is_ok());
        assert_eq!(
            invalid.validate(),
            Err("name must be at most 255 characters".to_string())
        );
    }
}
