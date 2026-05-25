use crate::pool::DbPool;
use crate::repositories::utils::map_db_error;
use crate::retry_db;
use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use services::common::RepositoryError;
use std::collections::HashMap;
use tokio_postgres::Row;
use uuid::Uuid;

#[derive(Debug, Clone)]
pub struct FeatureRequestTarget {
    pub id: Uuid,
    pub kind: String,
    pub key: String,
    pub title: String,
    pub status: String,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Debug, Clone)]
pub struct FeatureRequestSummary {
    pub target: FeatureRequestTarget,
    pub unique_user_count: i64,
    pub unique_organization_count: i64,
    pub latest_requested_at: DateTime<Utc>,
    pub recent_votes: Vec<FeatureRequestVoteSummary>,
}

#[derive(Debug, Clone)]
pub struct FeatureRequestVoteSummary {
    pub user_id: Uuid,
    pub user_email: String,
    pub user_display_name: Option<String>,
    pub organization_id: Option<Uuid>,
    pub organization_name: Option<String>,
    pub note: Option<String>,
    pub source: Option<String>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Debug, Clone)]
pub struct SubmitFeatureRequestParams {
    pub kind: String,
    pub key: String,
    pub title: String,
    pub user_id: Uuid,
    pub organization_id: Option<Uuid>,
    pub note: Option<String>,
    pub source: Option<String>,
}

#[derive(Debug, Clone)]
pub struct SubmitFeatureRequestResult {
    pub target: FeatureRequestTarget,
    pub unique_user_count: i64,
}

#[derive(Debug, Clone)]
pub struct FeatureRequestRepository {
    pool: DbPool,
}

impl FeatureRequestRepository {
    pub fn new(pool: DbPool) -> Self {
        Self { pool }
    }

    pub async fn user_belongs_to_organization(
        &self,
        user_id: Uuid,
        organization_id: Uuid,
    ) -> Result<bool> {
        let row = retry_db!("feature_request_user_belongs_to_organization", {
            let client = self
                .pool
                .get()
                .await
                .context("Failed to get database connection")
                .map_err(RepositoryError::PoolError)?;

            client
                .query_opt(
                    r#"
                    SELECT 1
                    FROM organization_members
                    WHERE user_id = $1 AND organization_id = $2
                    LIMIT 1
                    "#,
                    &[&user_id, &organization_id],
                )
                .await
                .map_err(map_db_error)
        })?;

        Ok(row.is_some())
    }

    pub async fn submit(
        &self,
        params: SubmitFeatureRequestParams,
    ) -> Result<SubmitFeatureRequestResult> {
        let (target_row, unique_user_count) = retry_db!("submit_feature_request", {
            let mut client = self
                .pool
                .get()
                .await
                .context("Failed to get database connection")
                .map_err(RepositoryError::PoolError)?;

            let transaction = client
                .transaction()
                .await
                .context("Failed to start transaction")
                .map_err(RepositoryError::DatabaseError)?;

            let target_row = transaction
                .query_one(
                    r#"
                    INSERT INTO feature_request_targets (kind, key, title)
                    VALUES ($1, $2, $3)
                    ON CONFLICT (kind, key) DO UPDATE
                    SET title = EXCLUDED.title,
                        updated_at = CASE
                            WHEN feature_request_targets.title IS DISTINCT FROM EXCLUDED.title
                            THEN NOW()
                            ELSE feature_request_targets.updated_at
                        END
                    RETURNING id, kind, key, title, status, created_at, updated_at
                    "#,
                    &[&params.kind, &params.key, &params.title],
                )
                .await
                .map_err(map_db_error)?;

            let target_id: Uuid = target_row.get("id");

            transaction
                .execute(
                    r#"
                    INSERT INTO feature_request_votes (
                        target_id, user_id, organization_id, note, source
                    )
                    VALUES ($1, $2, $3, $4, $5)
                    ON CONFLICT (target_id, user_id) DO UPDATE
                    SET organization_id = EXCLUDED.organization_id,
                        note = EXCLUDED.note,
                        source = EXCLUDED.source,
                        updated_at = NOW()
                    "#,
                    &[
                        &target_id,
                        &params.user_id,
                        &params.organization_id,
                        &params.note,
                        &params.source,
                    ],
                )
                .await
                .map_err(map_db_error)?;

            let count_row = transaction
                .query_one(
                    r#"
                    SELECT COUNT(DISTINCT user_id)::BIGINT AS unique_user_count
                    FROM feature_request_votes
                    WHERE target_id = $1
                    "#,
                    &[&target_id],
                )
                .await
                .map_err(map_db_error)?;

            transaction
                .commit()
                .await
                .context("Failed to commit feature request transaction")
                .map_err(RepositoryError::DatabaseError)?;

            Ok::<_, RepositoryError>((target_row, count_row.get::<_, i64>("unique_user_count")))
        })?;

        Ok(SubmitFeatureRequestResult {
            target: Self::row_to_target(&target_row),
            unique_user_count,
        })
    }

    pub async fn list_admin(
        &self,
        kind: Option<&str>,
        limit: i64,
        offset: i64,
    ) -> Result<(Vec<FeatureRequestSummary>, i64)> {
        let (rows, total) = retry_db!("list_feature_requests_admin", {
            let client = self
                .pool
                .get()
                .await
                .context("Failed to get database connection")
                .map_err(RepositoryError::PoolError)?;

            let total: i64 = client
                .query_one(
                    r#"
                    SELECT COUNT(*)::BIGINT
                    FROM feature_request_targets
                    WHERE ($1::TEXT IS NULL OR kind = $1)
                    "#,
                    &[&kind],
                )
                .await
                .map_err(map_db_error)?
                .get(0);

            let rows = client
                .query(
                    r#"
                    SELECT
                        t.id,
                        t.kind,
                        t.key,
                        t.title,
                        t.status,
                        t.created_at,
                        t.updated_at,
                        COUNT(DISTINCT v.user_id)::BIGINT AS unique_user_count,
                        COUNT(DISTINCT v.organization_id)::BIGINT AS unique_organization_count,
                        COALESCE(MAX(v.updated_at), t.updated_at) AS latest_requested_at
                    FROM feature_request_targets t
                    LEFT JOIN feature_request_votes v ON v.target_id = t.id
                    WHERE ($1::TEXT IS NULL OR t.kind = $1)
                    GROUP BY t.id
                    ORDER BY unique_user_count DESC, latest_requested_at DESC, t.title ASC
                    LIMIT $2 OFFSET $3
                    "#,
                    &[&kind, &limit, &offset],
                )
                .await
                .map_err(map_db_error)?;

            Ok::<_, RepositoryError>((rows, total))
        })?;

        let target_ids = rows
            .iter()
            .map(|row| row.get::<_, Uuid>("id"))
            .collect::<Vec<_>>();
        let mut recent_votes_by_target = self.list_recent_votes_for_targets(&target_ids, 3).await?;

        let mut summaries = Vec::with_capacity(rows.len());
        for row in rows {
            let target = Self::row_to_target(&row);
            let recent_votes = recent_votes_by_target
                .remove(&target.id)
                .unwrap_or_default();
            summaries.push(FeatureRequestSummary {
                target,
                unique_user_count: row.get("unique_user_count"),
                unique_organization_count: row.get("unique_organization_count"),
                latest_requested_at: row.get("latest_requested_at"),
                recent_votes,
            });
        }

        Ok((summaries, total))
    }

    async fn list_recent_votes_for_targets(
        &self,
        target_ids: &[Uuid],
        limit: i64,
    ) -> Result<HashMap<Uuid, Vec<FeatureRequestVoteSummary>>> {
        if target_ids.is_empty() {
            return Ok(HashMap::new());
        }

        let rows = retry_db!("list_recent_feature_request_votes", {
            let client = self
                .pool
                .get()
                .await
                .context("Failed to get database connection")
                .map_err(RepositoryError::PoolError)?;

            client
                .query(
                    r#"
                    WITH ranked_votes AS (
                        SELECT
                            v.target_id,
                            v.user_id,
                            u.email AS user_email,
                            u.display_name AS user_display_name,
                            v.organization_id,
                            o.name AS organization_name,
                            v.note,
                            v.source,
                            v.updated_at,
                            ROW_NUMBER() OVER (
                                PARTITION BY v.target_id
                                ORDER BY v.updated_at DESC
                            ) AS row_number
                        FROM feature_request_votes v
                        JOIN users u ON u.id = v.user_id
                        LEFT JOIN organizations o ON o.id = v.organization_id
                        WHERE v.target_id = ANY($1::UUID[])
                    )
                    SELECT
                        target_id,
                        user_id,
                        user_email,
                        user_display_name,
                        organization_id,
                        organization_name,
                        note,
                        source,
                        updated_at
                    FROM ranked_votes
                    WHERE row_number <= $2
                    ORDER BY target_id, updated_at DESC
                    "#,
                    &[&target_ids, &limit],
                )
                .await
                .map_err(map_db_error)
        })?;

        let mut recent_votes_by_target: HashMap<Uuid, Vec<FeatureRequestVoteSummary>> =
            HashMap::new();
        for row in rows {
            recent_votes_by_target
                .entry(row.get("target_id"))
                .or_default()
                .push(FeatureRequestVoteSummary {
                    user_id: row.get("user_id"),
                    user_email: row.get("user_email"),
                    user_display_name: row.get("user_display_name"),
                    organization_id: row.get("organization_id"),
                    organization_name: row.get("organization_name"),
                    note: row.get("note"),
                    source: row.get("source"),
                    updated_at: row.get("updated_at"),
                });
        }

        Ok(recent_votes_by_target)
    }

    fn row_to_target(row: &Row) -> FeatureRequestTarget {
        FeatureRequestTarget {
            id: row.get("id"),
            kind: row.get("kind"),
            key: row.get("key"),
            title: row.get("title"),
            status: row.get("status"),
            created_at: row.get("created_at"),
            updated_at: row.get("updated_at"),
        }
    }
}
