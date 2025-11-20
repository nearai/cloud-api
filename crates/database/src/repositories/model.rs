use crate::models::{Model, ModelHistory, UpdateModelPricingRequest};
use crate::pool::DbPool;
use anyhow::{Context, Result};
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use tokio_postgres::Row;

// Default reason for soft delete operations
const DEFAULT_SOFT_DELETE_REASON: &str = "Model soft deleted";

/// Audit information for model history records
#[derive(Debug, Clone)]
struct AuditInfo {
    user_id: Option<uuid::Uuid>,
    user_email: Option<String>,
    change_reason: Option<String>,
}

#[derive(Debug, Clone)]
pub struct ModelRepository {
    pool: DbPool,
}

impl ModelRepository {
    pub fn new(pool: DbPool) -> Self {
        Self { pool }
    }

    pub async fn get_all_active_models_count(&self) -> Result<i64> {
        let client = self
            .pool
            .get()
            .await
            .context("Failed to get database connection")?;

        let row = client
            .query_one(
                r#"
                SELECT COUNT(*) as count FROM models WHERE is_active = true
                "#,
                &[],
            )
            .await
            .context("Failed to query models")?;
        Ok(row.get::<_, i64>("count"))
    }

    /// Get all active models with pricing information
    pub async fn get_all_active_models(&self, limit: i64, offset: i64) -> Result<Vec<Model>> {
        let client = self
            .pool
            .get()
            .await
            .context("Failed to get database connection")?;

        let rows = client
            .query(
                r#"
                SELECT 
                    m.id, m.model_name, m.model_display_name, m.model_description, m.model_icon,
                    m.input_cost_per_token, m.output_cost_per_token,
                    m.context_length, m.verifiable, m.is_active, m.created_at, m.updated_at,
                    COALESCE(array_agg(a.alias_name) FILTER (WHERE a.alias_name IS NOT NULL), '{}') AS aliases
                FROM models m
                LEFT JOIN model_aliases a ON a.canonical_model_id = m.id AND a.is_active = true
                WHERE m.is_active = true
                GROUP BY m.id
                ORDER BY m.model_name ASC
                LIMIT $1 OFFSET $2
                "#,
                &[&limit, &offset],
            )
            .await
            .context("Failed to query models")?;

        let models = rows
            .into_iter()
            .map(|row| self.row_to_model(&row))
            .collect();
        Ok(models)
    }

    /// Get model by internal model name (for upsert logic - includes inactive models)
    /// Searches model_name only
    pub async fn get_by_internal_name(&self, model_name: &str) -> Result<Option<Model>> {
        let client = self
            .pool
            .get()
            .await
            .context("Failed to get database connection")?;

        let rows = client
            .query(
                r#"
                SELECT 
                    id, model_name, model_display_name, model_description, model_icon,
                    input_cost_per_token, output_cost_per_token,
                    context_length, verifiable, is_active, created_at, updated_at
                FROM models
                WHERE model_name = $1
                "#,
                &[&model_name],
            )
            .await
            .context("Failed to check if model exists")?;

        if let Some(row) = rows.first() {
            Ok(Some(self.row_to_model(row)))
        } else {
            Ok(None)
        }
    }

    /// Get model by UUID (includes inactive models)
    pub async fn get_by_id(&self, model_id: &uuid::Uuid) -> Result<Option<Model>> {
        let client = self
            .pool
            .get()
            .await
            .context("Failed to get database connection")?;

        let rows = client
            .query(
                r#"
                SELECT 
                    id, model_name, model_display_name, model_description, model_icon,
                    input_cost_per_token, output_cost_per_token,
                    context_length, verifiable, is_active, created_at, updated_at
                FROM models
                WHERE id = $1
                "#,
                &[&model_id],
            )
            .await
            .context("Failed to get model by id")?;

        if let Some(row) = rows.first() {
            Ok(Some(self.row_to_model(row)))
        } else {
            Ok(None)
        }
    }

    /// Get model by model name (public API - only active models)
    /// Searches model_name (canonical name) field only
    pub async fn get_active_model_by_name(&self, model_name: &str) -> Result<Option<Model>> {
        let client = self
            .pool
            .get()
            .await
            .context("Failed to get database connection")?;

        let rows = client
            .query(
                r#"
                SELECT 
                    m.id,
                    m.model_name,
                    m.model_display_name,
                    m.model_description,
                    m.model_icon,
                    m.input_cost_per_token,
                    m.output_cost_per_token,
                    m.context_length,
                    m.verifiable,
                    m.is_active,
                    m.created_at,
                    m.updated_at,
                    COALESCE(
                        array_agg(ma.alias_name)
                        FILTER (WHERE ma.alias_name IS NOT NULL),
                        '{}'
                    ) AS aliases
                FROM models m
                LEFT JOIN model_aliases ma
                    ON ma.canonical_model_id = m.id
                    AND ma.is_active = true
                WHERE m.is_active = true
                AND m.model_name = $1
                GROUP BY m.id
                LIMIT 1;
                "#,
                &[&model_name],
            )
            .await
            .context("Failed to query model by name or alias")?;

        if let Some(row) = rows.first() {
            Ok(Some(self.row_to_model(row)))
        } else {
            Ok(None)
        }
    }

    /// Update model pricing and metadata (or insert if not exists - upsert)
    pub async fn upsert_model_pricing(
        &self,
        model_name: &str,
        update_request: &UpdateModelPricingRequest,
    ) -> Result<Model> {
        let mut client = self
            .pool
            .get()
            .await
            .context("Failed to get database connection")?;

        // Start explicit transaction to ensure atomicity (Issue #3)
        let tx = client
            .transaction()
            .await
            .context("Failed to start transaction")?;

        // For updates, we can do partial updates with COALESCE
        // For inserts, we need all required fields
        // Check if model exists first to determine which code path to take
        let existing = self.get_by_internal_name(model_name).await?;

        // Capture single timestamp for all operations (Issue #2 - prevent temporal gaps)
        let now = chrono::Utc::now();

        let row = if existing.is_some() {
            // Model exists - do UPDATE (partial updates work)
            let updated_row = tx
                .query_one(
                    r#"
                    UPDATE models SET
                        input_cost_per_token = COALESCE($2, input_cost_per_token),
                        output_cost_per_token = COALESCE($3, output_cost_per_token),
                        model_display_name = COALESCE($4, model_display_name),
                        model_description = COALESCE($5, model_description),
                        model_icon = COALESCE($6, model_icon),
                        context_length = COALESCE($7, context_length),
                        verifiable = COALESCE($8, verifiable),
                        is_active = COALESCE($9, is_active),
                        updated_at = $10
                    WHERE model_name = $1
                    RETURNING id, model_name, model_display_name, model_description, model_icon,
                              input_cost_per_token, output_cost_per_token,
                              context_length, verifiable, is_active, created_at, updated_at
                    "#,
                    &[
                        &model_name,
                        &update_request.input_cost_per_token,
                        &update_request.output_cost_per_token,
                        &update_request.model_display_name,
                        &update_request.model_description,
                        &update_request.model_icon,
                        &update_request.context_length,
                        &update_request.verifiable,
                        &update_request.is_active,
                        &now,
                    ],
                )
                .await
                .context("Failed to update model pricing")?;

            // Record history: close previous history record and insert new one
            let model_id: uuid::Uuid = updated_row.get("id");
            let audit = AuditInfo {
                user_id: update_request.changed_by_user_id,
                user_email: update_request.changed_by_user_email.clone(),
                change_reason: update_request.change_reason.clone(),
            };
            self.record_model_history(&tx, model_id, &updated_row, &audit, &now)
                .await
                .context("Failed to record model history")?;

            updated_row
        } else {
            // Model doesn't exist - do INSERT with ON CONFLICT to handle race conditions
            let display_name = update_request
                .model_display_name
                .as_ref()
                .cloned()
                .context("model_display_name is required for new models")?;

            let description = update_request
                .model_description
                .as_ref()
                .cloned()
                .context("model_description is required for new models")?;

            let context_length = update_request
                .context_length
                .context("context_length is required for new models")?;

            // Use INSERT ... ON CONFLICT to handle race conditions where another
            // transaction inserts the same model between our check and insert
            // Use COALESCE to preserve existing values during conflict (Issue #5 - race condition)
            let inserted_row = tx
                .query_one(
                    r#"
                    INSERT INTO models (
                        model_name,
                        input_cost_per_token, output_cost_per_token,
                        model_display_name, model_description, model_icon,
                        context_length, verifiable, is_active
                    ) VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9)
                    ON CONFLICT (model_name) DO UPDATE SET
                        input_cost_per_token = COALESCE(EXCLUDED.input_cost_per_token, models.input_cost_per_token),
                        output_cost_per_token = COALESCE(EXCLUDED.output_cost_per_token, models.output_cost_per_token),
                        model_display_name = COALESCE(EXCLUDED.model_display_name, models.model_display_name),
                        model_description = COALESCE(EXCLUDED.model_description, models.model_description),
                        model_icon = COALESCE(EXCLUDED.model_icon, models.model_icon),
                        context_length = COALESCE(EXCLUDED.context_length, models.context_length),
                        verifiable = COALESCE(EXCLUDED.verifiable, models.verifiable),
                        is_active = COALESCE(EXCLUDED.is_active, models.is_active),
                        updated_at = $10
                    RETURNING id, model_name, model_display_name, model_description, model_icon,
                              input_cost_per_token, output_cost_per_token,
                              context_length, verifiable, is_active, created_at, updated_at
                    "#,
                    &[
                        &model_name,
                        &update_request.input_cost_per_token.unwrap_or(0),
                        &update_request.output_cost_per_token.unwrap_or(0),
                        &display_name,
                        &description,
                        &update_request.model_icon,
                        &context_length,
                        &update_request.verifiable.unwrap_or(true),
                        &update_request.is_active.unwrap_or(true),
                        &now,
                    ],
                )
                .await
                .context("Failed to insert new model")?;

            // Record history for new model
            let model_id: uuid::Uuid = inserted_row.get("id");
            let audit = AuditInfo {
                user_id: update_request.changed_by_user_id,
                user_email: update_request.changed_by_user_email.clone(),
                change_reason: update_request.change_reason.clone(),
            };
            self.record_model_history(&tx, model_id, &inserted_row, &audit, &now)
                .await
                .context("Failed to record model history")?;

            inserted_row
        };

        // Commit transaction
        tx.commit().await.context("Failed to commit transaction")?;

        Ok(self.row_to_model(&row))
    }

    /// Create a new model with pricing
    pub async fn create_model(&self, model: &Model) -> Result<Model> {
        let client = self
            .pool
            .get()
            .await
            .context("Failed to get database connection")?;

        let row = client
            .query_one(
                r#"
                INSERT INTO models (
                    model_name, model_display_name, model_description, model_icon,
                    input_cost_per_token, output_cost_per_token,
                    context_length, verifiable, is_active
                ) VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9)
                RETURNING id, model_name, model_display_name, model_description, model_icon,
                          input_cost_per_token, output_cost_per_token,
                          context_length, verifiable, is_active, created_at, updated_at
                "#,
                &[
                    &model.model_name,
                    &model.model_display_name,
                    &model.model_description,
                    &model.model_icon,
                    &model.input_cost_per_token,
                    &model.output_cost_per_token,
                    &model.context_length,
                    &model.verifiable,
                    &model.is_active,
                ],
            )
            .await
            .context("Failed to create model")?;

        Ok(self.row_to_model(&row))
    }

    /// Get history for a specific model
    pub async fn get_model_history(&self, model_id: &uuid::Uuid) -> Result<Vec<ModelHistory>> {
        let client = self
            .pool
            .get()
            .await
            .context("Failed to get database connection")?;

        let rows = client
            .query(
                r#"
                SELECT
                    id, model_id, input_cost_per_token, output_cost_per_token,
                    context_length, model_name, model_display_name, model_description,
                    model_icon, verifiable, is_active,
                    effective_from, effective_until, changed_by_user_id, changed_by_user_email,
                    change_reason, created_at
                FROM model_history
                WHERE model_id = $1
                ORDER BY effective_from DESC
                "#,
                &[&model_id],
            )
            .await
            .context("Failed to query model history")?;

        let history = rows
            .into_iter()
            .map(|row| self.row_to_model_history(&row))
            .collect();
        Ok(history)
    }

    /// Get model state that was effective at a specific timestamp
    pub async fn get_model_state_at_time(
        &self,
        model_id: &uuid::Uuid,
        timestamp: DateTime<Utc>,
    ) -> Result<Option<ModelHistory>> {
        let client = self
            .pool
            .get()
            .await
            .context("Failed to get database connection")?;

        let rows = client
            .query(
                r#"
                SELECT
                    id, model_id, input_cost_per_token, output_cost_per_token,
                    context_length, model_name, model_display_name, model_description,
                    model_icon, verifiable, is_active,
                    effective_from, effective_until, changed_by_user_id, changed_by_user_email,
                    change_reason, created_at
                FROM model_history
                WHERE model_id = $1
                AND effective_from <= $2
                AND (effective_until IS NULL OR effective_until > $2)
                ORDER BY effective_from DESC
                LIMIT 1
                "#,
                &[&model_id, &timestamp],
            )
            .await
            .context("Failed to query model state at time")?;

        if let Some(row) = rows.first() {
            Ok(Some(self.row_to_model_history(row)))
        } else {
            Ok(None)
        }
    }

    /// Get count of history entries for a model by model name
    pub async fn count_model_history_by_name(&self, model_name: &str) -> Result<i64> {
        let client = self
            .pool
            .get()
            .await
            .context("Failed to get database connection")?;

        let row = client
            .query_one(
                r#"
                SELECT COUNT(*) as count
                FROM model_history h
                JOIN models m ON h.model_id = m.id
                WHERE m.model_name = $1
                "#,
                &[&model_name],
            )
            .await
            .context("Failed to count model history")?;
        Ok(row.get::<_, i64>("count"))
    }

    /// Get complete history for a model by model name with pagination (includes pricing and other attributes)
    pub async fn get_model_history_by_name(
        &self,
        model_name: &str,
        limit: i64,
        offset: i64,
    ) -> Result<Vec<ModelHistory>> {
        let client = self
            .pool
            .get()
            .await
            .context("Failed to get database connection")?;

        let rows = client
            .query(
                r#"
                SELECT
                    h.id, h.model_id, h.input_cost_per_token, h.output_cost_per_token,
                    h.context_length, h.model_name, h.model_display_name, h.model_description,
                    h.model_icon, h.verifiable, h.is_active,
                    h.effective_from, h.effective_until, h.changed_by_user_id, h.changed_by_user_email,
                    h.change_reason, h.created_at
                FROM model_history h
                JOIN models m ON h.model_id = m.id
                WHERE m.model_name = $1
                ORDER BY h.effective_from DESC
                LIMIT $2 OFFSET $3
                "#,
                &[&model_name, &limit, &offset],
            )
            .await
            .context("Failed to query model history by name")?;

        let history = rows
            .into_iter()
            .map(|row| self.row_to_model_history(&row))
            .collect();
        Ok(history)
    }

    /// Soft delete a model by setting is_active to false
    pub async fn soft_delete_model(
        &self,
        model_name: &str,
        change_reason: Option<String>,
        changed_by_user_id: Option<uuid::Uuid>,
        changed_by_user_email: Option<String>,
    ) -> Result<bool> {
        let mut client = self
            .pool
            .get()
            .await
            .context("Failed to get database connection")?;

        // Start transaction to ensure atomicity (Issue #3)
        let tx = client
            .transaction()
            .await
            .context("Failed to start transaction")?;

        // Capture single timestamp for all operations (Issue #2)
        let now = chrono::Utc::now();

        let result = tx
            .query_opt(
                r#"
                UPDATE models
                SET is_active = false, updated_at = $2
                WHERE model_name = $1 AND is_active = true
                RETURNING id, model_name, model_display_name, model_description, model_icon,
                          input_cost_per_token, output_cost_per_token,
                          context_length, verifiable, is_active, created_at, updated_at
                "#,
                &[&model_name, &now],
            )
            .await
            .context("Failed to soft delete model")?;

        if let Some(row) = result {
            // Record history: capture the soft delete in history
            let model_id: uuid::Uuid = row.get("id");
            let reason = change_reason.or_else(|| Some(DEFAULT_SOFT_DELETE_REASON.to_string()));
            let audit = AuditInfo {
                user_id: changed_by_user_id,
                user_email: changed_by_user_email,
                change_reason: reason,
            };
            self.record_model_history(&tx, model_id, &row, &audit, &now)
                .await
                .context("Failed to record model history for deletion")?;

            // Commit transaction
            tx.commit().await.context("Failed to commit transaction")?;

            Ok(true)
        } else {
            Ok(false)
        }
    }

    /// Helper method to record a model history entry
    /// Closes the previous history record and creates a new one
    /// Works within a transaction context with a provided timestamp (Issue #2 - prevent temporal gaps)
    async fn record_model_history(
        &self,
        tx: &tokio_postgres::Transaction<'_>,
        model_id: uuid::Uuid,
        model_row: &Row,
        audit: &AuditInfo,
        timestamp: &chrono::DateTime<chrono::Utc>,
    ) -> Result<()> {
        // Close previous history record (set effective_until to provided timestamp)
        tx.execute(
            r#"
                UPDATE model_history
                SET effective_until = $2
                WHERE model_id = $1 AND effective_until IS NULL
                "#,
            &[&model_id, timestamp],
        )
        .await
        .context("Failed to close previous model history record")?;

        // Insert new history record with current model state
        tx.execute(
            r#"
                INSERT INTO model_history (
                    model_id,
                    input_cost_per_token,
                    output_cost_per_token,
                    context_length,
                    model_name,
                    model_display_name,
                    model_description,
                    model_icon,
                    verifiable,
                    is_active,
                    effective_from,
                    effective_until,
                    changed_by_user_id,
                    changed_by_user_email,
                    change_reason,
                    created_at
                ) VALUES (
                    $1, $2, $3, $4, $5, $6, $7, $8, $9, $10,
                    $11, NULL, $12, $13, $14, $11
                )
                "#,
            &[
                &model_id,
                &model_row.get::<_, i64>("input_cost_per_token"),
                &model_row.get::<_, i64>("output_cost_per_token"),
                &model_row.get::<_, i32>("context_length"),
                &model_row.get::<_, String>("model_name"),
                &model_row.get::<_, String>("model_display_name"),
                &model_row.get::<_, String>("model_description"),
                &model_row.get::<_, Option<String>>("model_icon"),
                &model_row.get::<_, bool>("verifiable"),
                &model_row.get::<_, bool>("is_active"),
                timestamp,
                &audit.user_id,
                &audit.user_email,
                &audit.change_reason,
            ],
        )
        .await
        .context("Failed to insert model history record")?;

        Ok(())
    }

    /// Get list of configured model names (canonical names)
    /// Returns only active models that have been configured with pricing
    pub async fn get_configured_model_names(&self) -> Result<Vec<String>> {
        let client = self
            .pool
            .get()
            .await
            .context("Failed to get database connection")?;

        let rows = client
            .query(
                r#"
                SELECT model_name
                FROM models
                WHERE is_active = true
                ORDER BY model_name ASC
                "#,
                &[],
            )
            .await
            .context("Failed to query configured model names")?;

        let names = rows
            .into_iter()
            .map(|row| row.get::<_, String>("model_name"))
            .collect();

        Ok(names)
    }

    /// Resolve a model identifier (alias or canonical name) and return the full model details
    /// Returns None if the model is not found or not active
    pub async fn resolve_and_get_model(&self, identifier: &str) -> Result<Option<Model>> {
        let client = self
            .pool
            .get()
            .await
            .context("Failed to get database connection")?;

        let row = client
            .query_opt(
                r#"
                SELECT 
                    m.id,
                    m.model_name,
                    m.model_display_name,
                    m.model_description,
                    m.model_icon,
                    m.input_cost_per_token,
                    m.output_cost_per_token,
                    m.context_length,
                    m.verifiable,
                    m.is_active,
                    m.created_at,
                    m.updated_at,
                    COALESCE(
                        array_agg(ma_all.alias_name)
                        FILTER (WHERE ma_all.alias_name IS NOT NULL),
                        '{}'
                    ) AS aliases
                FROM models m
                LEFT JOIN model_aliases ma_all
                    ON ma_all.canonical_model_id = m.id
                    AND ma_all.is_active = true
                WHERE m.is_active = true
                AND (
                    m.model_name = $1
                    OR EXISTS (
                        SELECT 1
                        FROM model_aliases ma_match
                        WHERE ma_match.canonical_model_id = m.id
                        AND ma_match.alias_name = $1
                        AND ma_match.is_active = true
                    )
                )
                GROUP BY m.id
                LIMIT 1;
                "#,
                &[&identifier],
            )
            .await
            .context("Failed to resolve and fetch model (by name or alias)")?;

        Ok(row.map(|r| self.row_to_model(&r)))
    }

    /// Helper method to convert database row to Model
    fn row_to_model(&self, row: &Row) -> Model {
        Model {
            id: row.get("id"),
            model_name: row.get("model_name"),
            model_display_name: row.get("model_display_name"),
            model_description: row.get("model_description"),
            model_icon: row.get("model_icon"),
            input_cost_per_token: row.get("input_cost_per_token"),
            output_cost_per_token: row.get("output_cost_per_token"),
            context_length: row.get("context_length"),
            verifiable: row.get("verifiable"),
            is_active: row.get("is_active"),
            created_at: row.get("created_at"),
            updated_at: row.get("updated_at"),
            aliases: row.try_get("aliases").unwrap_or_default(),
        }
    }

    /// Helper method to convert database row to ModelHistory
    fn row_to_model_history(&self, row: &Row) -> ModelHistory {
        ModelHistory {
            id: row.get("id"),
            model_id: row.get("model_id"),
            input_cost_per_token: row.get("input_cost_per_token"),
            output_cost_per_token: row.get("output_cost_per_token"),
            context_length: row.get("context_length"),
            model_name: row.get("model_name"),
            model_display_name: row.get("model_display_name"),
            model_description: row.get("model_description"),
            model_icon: row.get("model_icon"),
            verifiable: row.get("verifiable"),
            is_active: row.get("is_active"),
            effective_from: row.get("effective_from"),
            effective_until: row.get("effective_until"),
            changed_by_user_id: row.get("changed_by_user_id"),
            changed_by_user_email: row.get("changed_by_user_email"),
            change_reason: row.get("change_reason"),
            created_at: row.get("created_at"),
        }
    }
}

// Implement ModelsRepository trait from services
#[async_trait]
impl services::models::ModelsRepository for ModelRepository {
    async fn get_all_active_models_count(&self) -> Result<i64> {
        self.get_all_active_models_count().await
    }

    async fn get_all_active_models(
        &self,
        limit: i64,
        offset: i64,
    ) -> Result<Vec<services::models::ModelWithPricing>> {
        let models = self.get_all_active_models(limit, offset).await?;
        Ok(models
            .into_iter()
            .map(|m| services::models::ModelWithPricing {
                id: m.id,
                model_name: m.model_name,
                model_display_name: m.model_display_name,
                model_description: m.model_description,
                model_icon: m.model_icon,
                input_cost_per_token: m.input_cost_per_token,
                output_cost_per_token: m.output_cost_per_token,
                context_length: m.context_length,
                verifiable: m.verifiable,
                aliases: m.aliases,
            })
            .collect())
    }

    async fn get_model_by_name(
        &self,
        model_name: &str,
    ) -> Result<Option<services::models::ModelWithPricing>> {
        let model_opt = self.get_active_model_by_name(model_name).await?;
        Ok(model_opt.map(|m| services::models::ModelWithPricing {
            id: m.id,
            model_name: m.model_name,
            model_display_name: m.model_display_name,
            model_description: m.model_description,
            model_icon: m.model_icon,
            input_cost_per_token: m.input_cost_per_token,
            output_cost_per_token: m.output_cost_per_token,
            context_length: m.context_length,
            verifiable: m.verifiable,
            aliases: m.aliases,
        }))
    }

    async fn resolve_and_get_model(
        &self,
        identifier: &str,
    ) -> Result<Option<services::models::ModelWithPricing>> {
        let model_opt = self.resolve_and_get_model(identifier).await?;
        Ok(model_opt.map(|m| services::models::ModelWithPricing {
            id: m.id,
            model_name: m.model_name,
            model_display_name: m.model_display_name,
            model_description: m.model_description,
            model_icon: m.model_icon,
            input_cost_per_token: m.input_cost_per_token,
            output_cost_per_token: m.output_cost_per_token,
            context_length: m.context_length,
            verifiable: m.verifiable,
            aliases: m.aliases,
        }))
    }

    async fn get_configured_model_names(&self) -> Result<Vec<String>> {
        self.get_configured_model_names().await
    }
}
