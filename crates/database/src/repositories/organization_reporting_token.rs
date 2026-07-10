use crate::pool::DbPool;
use crate::repositories::organization_reporting_token_row::OrganizationReportingTokenRow;
use crate::repositories::utils::map_db_error;
use crate::retry_db;
use anyhow::{Context, Result};
use async_trait::async_trait;
use services::common::{
    extract_reporting_token_prefix, generate_reporting_token, hash_reporting_token,
    is_valid_reporting_token_format, RepositoryError,
};
use services::reporting_tokens::ports::{
    CreateOrganizationReportingTokenRequest, CreatedOrganizationReportingToken,
    OrganizationReportingToken, ValidatedOrganizationReportingToken,
};
use uuid::Uuid;

pub struct OrganizationReportingTokenRepository {
    pool: DbPool,
}

impl OrganizationReportingTokenRepository {
    pub fn new(pool: DbPool) -> Self {
        Self { pool }
    }

    async fn create_token(
        &self,
        request: CreateOrganizationReportingTokenRequest,
    ) -> Result<CreatedOrganizationReportingToken, RepositoryError> {
        let name = request.name.trim().to_string();
        if name.is_empty() {
            return Err(RepositoryError::ValidationFailed(
                "reporting token name cannot be empty".to_string(),
            ));
        }

        let id = Uuid::new_v4();
        let raw_token = generate_reporting_token();
        let token_hash = hash_reporting_token(&raw_token);
        let token_prefix = extract_reporting_token_prefix(&raw_token);

        let row = retry_db!("create_organization_reporting_token", {
            let client = self
                .pool
                .get()
                .await
                .context("Failed to get database connection")
                .map_err(RepositoryError::PoolError)?;

            client
                .query_one(
                    r#"
                    INSERT INTO organization_reporting_tokens (
                        id, organization_id, name, token_hash, token_prefix,
                        created_by_user_id, expires_at
                    )
                    VALUES ($1, $2, $3, $4, $5, $6, $7)
                    RETURNING *
                    "#,
                    &[
                        &id,
                        &request.organization_id,
                        &name,
                        &token_hash,
                        &token_prefix,
                        &request.created_by_user_id,
                        &request.expires_at,
                    ],
                )
                .await
                .map_err(map_db_error)
        })?;

        let db_token = OrganizationReportingTokenRow::from_row(row)
            .map_err(RepositoryError::DataConversionError)?;
        Ok(CreatedOrganizationReportingToken {
            token: db_token.into_service_token(),
            raw_token,
        })
    }

    async fn validate_token(
        &self,
        raw_token: &str,
    ) -> Result<Option<ValidatedOrganizationReportingToken>, RepositoryError> {
        if !is_valid_reporting_token_format(raw_token) {
            return Ok(None);
        }

        let token_hash = hash_reporting_token(raw_token);
        let row = retry_db!("validate_organization_reporting_token", {
            let client = self
                .pool
                .get()
                .await
                .context("Failed to get database connection")
                .map_err(RepositoryError::PoolError)?;

            client
                .query_opt(
                    r#"
                    UPDATE organization_reporting_tokens
                    SET last_used_at = NOW()
                    WHERE token_hash = $1
                      AND revoked_at IS NULL
                      AND (expires_at IS NULL OR expires_at > NOW())
                    RETURNING *
                    "#,
                    &[&token_hash],
                )
                .await
                .map_err(map_db_error)
        })?;

        match row {
            Some(row) => {
                let db_token = OrganizationReportingTokenRow::from_row(row)
                    .map_err(RepositoryError::DataConversionError)?;
                Ok(Some(db_token.into_validated_token()))
            }
            None => Ok(None),
        }
    }

    async fn get_token_by_id(
        &self,
        token_id: Uuid,
    ) -> Result<Option<OrganizationReportingToken>, RepositoryError> {
        let row = retry_db!("get_organization_reporting_token_by_id", {
            let client = self
                .pool
                .get()
                .await
                .context("Failed to get database connection")
                .map_err(RepositoryError::PoolError)?;

            client
                .query_opt(
                    "SELECT * FROM organization_reporting_tokens WHERE id = $1",
                    &[&token_id],
                )
                .await
                .map_err(map_db_error)
        })?;

        match row {
            Some(row) => {
                let db_token = OrganizationReportingTokenRow::from_row(row)
                    .map_err(RepositoryError::DataConversionError)?;
                Ok(Some(db_token.into_service_token()))
            }
            None => Ok(None),
        }
    }

    async fn list_active_tokens_by_organization(
        &self,
        organization_id: Uuid,
    ) -> Result<Vec<OrganizationReportingToken>, RepositoryError> {
        let rows = retry_db!("list_active_organization_reporting_tokens", {
            let client = self
                .pool
                .get()
                .await
                .context("Failed to get database connection")
                .map_err(RepositoryError::PoolError)?;

            client
                .query(
                    r#"
                    SELECT *
                    FROM organization_reporting_tokens
                    WHERE organization_id = $1
                      AND revoked_at IS NULL
                      AND (expires_at IS NULL OR expires_at > NOW())
                    ORDER BY created_at DESC, id DESC
                    "#,
                    &[&organization_id],
                )
                .await
                .map_err(map_db_error)
        })?;

        rows.into_iter()
            .map(|row| {
                let db_token = OrganizationReportingTokenRow::from_row(row)?;
                Ok(db_token.into_service_token())
            })
            .collect::<Result<Vec<_>>>()
            .map_err(RepositoryError::DataConversionError)
    }

    async fn revoke_token(
        &self,
        token_id: Uuid,
        revoked_by_user_id: Uuid,
    ) -> Result<bool, RepositoryError> {
        let rows_affected = retry_db!("revoke_organization_reporting_token", {
            let client = self
                .pool
                .get()
                .await
                .context("Failed to get database connection")
                .map_err(RepositoryError::PoolError)?;

            client
                .execute(
                    r#"
                    UPDATE organization_reporting_tokens
                    SET revoked_at = NOW(), revoked_by_user_id = $2
                    WHERE id = $1 AND revoked_at IS NULL
                    "#,
                    &[&token_id, &revoked_by_user_id],
                )
                .await
                .map_err(map_db_error)
        })?;

        Ok(rows_affected > 0)
    }
}

#[async_trait]
impl services::reporting_tokens::ports::OrganizationReportingTokenRepository
    for OrganizationReportingTokenRepository
{
    async fn create(
        &self,
        request: CreateOrganizationReportingTokenRequest,
    ) -> Result<CreatedOrganizationReportingToken, RepositoryError> {
        self.create_token(request).await
    }

    async fn validate(
        &self,
        raw_token: &str,
    ) -> Result<Option<ValidatedOrganizationReportingToken>, RepositoryError> {
        self.validate_token(raw_token).await
    }

    async fn get_by_id(
        &self,
        token_id: Uuid,
    ) -> Result<Option<OrganizationReportingToken>, RepositoryError> {
        self.get_token_by_id(token_id).await
    }

    async fn list_active_by_organization(
        &self,
        organization_id: Uuid,
    ) -> Result<Vec<OrganizationReportingToken>, RepositoryError> {
        self.list_active_tokens_by_organization(organization_id)
            .await
    }

    async fn revoke(
        &self,
        token_id: Uuid,
        revoked_by_user_id: Uuid,
    ) -> Result<bool, RepositoryError> {
        self.revoke_token(token_id, revoked_by_user_id).await
    }
}
