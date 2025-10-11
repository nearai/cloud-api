use crate::models::{Model, ModelPricingHistory, UpdateModelPricingRequest};
use crate::pool::DbPool;
use anyhow::{Context, Result};
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use tokio_postgres::Row;

#[derive(Debug, Clone)]
pub struct ModelRepository {
    pool: DbPool,
}

impl ModelRepository {
    pub fn new(pool: DbPool) -> Self {
        Self { pool }
    }

    /// Get all active models with pricing information
    pub async fn get_all_active_models(&self) -> Result<Vec<Model>> {
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
                WHERE is_active = true
                ORDER BY model_name ASC
                "#,
                &[],
            )
            .await
            .context("Failed to query models")?;

        let models = rows
            .into_iter()
            .map(|row| self.row_to_model(&row))
            .collect();
        Ok(models)
    }

    /// Get model by model name
    pub async fn get_by_name(&self, model_name: &str) -> Result<Option<Model>> {
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
            .context("Failed to query model by name")?;

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
        let client = self
            .pool
            .get()
            .await
            .context("Failed to get database connection")?;

        // Use INSERT ... ON CONFLICT for upsert behavior
        let row = client
            .query_one(
                r#"
                INSERT INTO models (
                    model_name, 
                    input_cost_per_token, output_cost_per_token,
                    model_display_name, model_description, model_icon,
                    context_length, verifiable, is_active
                ) VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9)
                ON CONFLICT (model_name) DO UPDATE SET
                    input_cost_per_token = COALESCE($2, models.input_cost_per_token),
                    output_cost_per_token = COALESCE($3, models.output_cost_per_token),
                    model_display_name = COALESCE($4, models.model_display_name),
                    model_description = COALESCE($5, models.model_description),
                    model_icon = COALESCE($6, models.model_icon),
                    context_length = COALESCE($7, models.context_length),
                    verifiable = COALESCE($8, models.verifiable),
                    is_active = COALESCE($9, models.is_active),
                    updated_at = NOW()
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
                ],
            )
            .await
            .context("Failed to upsert model pricing")?;

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

    /// Get pricing history for a specific model
    pub async fn get_pricing_history(
        &self,
        model_id: &uuid::Uuid,
    ) -> Result<Vec<ModelPricingHistory>> {
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
                    context_length, model_display_name, model_description,
                    effective_from, effective_until, changed_by, change_reason, created_at
                FROM model_pricing_history
                WHERE model_id = $1
                ORDER BY effective_from DESC
                "#,
                &[&model_id],
            )
            .await
            .context("Failed to query pricing history")?;

        let history = rows
            .into_iter()
            .map(|row| self.row_to_pricing_history(&row))
            .collect();
        Ok(history)
    }

    /// Get pricing that was effective at a specific timestamp
    pub async fn get_pricing_at_time(
        &self,
        model_id: &uuid::Uuid,
        timestamp: DateTime<Utc>,
    ) -> Result<Option<ModelPricingHistory>> {
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
                    context_length, model_display_name, model_description,
                    effective_from, effective_until, changed_by, change_reason, created_at
                FROM model_pricing_history
                WHERE model_id = $1
                AND effective_from <= $2
                AND (effective_until IS NULL OR effective_until > $2)
                ORDER BY effective_from DESC
                LIMIT 1
                "#,
                &[&model_id, &timestamp],
            )
            .await
            .context("Failed to query pricing at time")?;

        if let Some(row) = rows.first() {
            Ok(Some(self.row_to_pricing_history(row)))
        } else {
            Ok(None)
        }
    }

    /// Get pricing history for a model by model name
    pub async fn get_pricing_history_by_name(
        &self,
        model_name: &str,
    ) -> Result<Vec<ModelPricingHistory>> {
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
                    h.context_length, h.model_display_name, h.model_description,
                    h.effective_from, h.effective_until, h.changed_by, h.change_reason, h.created_at
                FROM model_pricing_history h
                JOIN models m ON h.model_id = m.id
                WHERE m.model_name = $1
                ORDER BY h.effective_from DESC
                "#,
                &[&model_name],
            )
            .await
            .context("Failed to query pricing history by name")?;

        let history = rows
            .into_iter()
            .map(|row| self.row_to_pricing_history(&row))
            .collect();
        Ok(history)
    }

    /// Soft delete a model by setting is_active to false
    pub async fn soft_delete_model(&self, model_name: &str) -> Result<bool> {
        let client = self
            .pool
            .get()
            .await
            .context("Failed to get database connection")?;

        let result = client
            .execute(
                r#"
                UPDATE models 
                SET is_active = false, updated_at = NOW()
                WHERE model_name = $1
                "#,
                &[&model_name],
            )
            .await
            .context("Failed to soft delete model")?;

        Ok(result > 0)
    }

    /// Resolve a model name (which could be an alias) to the canonical model name
    /// If the name is already canonical, returns it as-is
    /// If the name is an alias, resolves to the canonical name
    pub async fn resolve_to_canonical_name(&self, model_name: &str) -> Result<String> {
        let client = self
            .pool
            .get()
            .await
            .context("Failed to get database connection")?;

        // Try to resolve as an alias first
        let rows = client
            .query(
                r#"
                SELECT m.model_name
                FROM model_aliases ma
                JOIN models m ON ma.canonical_model_id = m.id
                WHERE ma.alias_name = $1 AND ma.is_active = true AND m.is_active = true
                LIMIT 1
                "#,
                &[&model_name],
            )
            .await
            .context("Failed to resolve alias to canonical name")?;

        if let Some(row) = rows.first() {
            // It's an alias, return the canonical name
            Ok(row.get::<_, String>("model_name"))
        } else {
            // Not an alias, assume it's already canonical (or doesn't exist, which will be caught later)
            Ok(model_name.to_string())
        }
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
        }
    }

    /// Helper method to convert database row to ModelPricingHistory
    fn row_to_pricing_history(&self, row: &Row) -> ModelPricingHistory {
        ModelPricingHistory {
            id: row.get("id"),
            model_id: row.get("model_id"),
            input_cost_per_token: row.get("input_cost_per_token"),
            output_cost_per_token: row.get("output_cost_per_token"),
            context_length: row.get("context_length"),
            model_display_name: row.get("model_display_name"),
            model_description: row.get("model_description"),
            effective_from: row.get("effective_from"),
            effective_until: row.get("effective_until"),
            changed_by: row.get("changed_by"),
            change_reason: row.get("change_reason"),
            created_at: row.get("created_at"),
        }
    }
}

// Implement ModelsRepository trait from services
#[async_trait]
impl services::models::ModelsRepository for ModelRepository {
    async fn get_all_active_models(&self) -> Result<Vec<services::models::ModelWithPricing>> {
        let models = self.get_all_active_models().await?;
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
            })
            .collect())
    }

    async fn get_model_by_name(
        &self,
        model_name: &str,
    ) -> Result<Option<services::models::ModelWithPricing>> {
        let model_opt = self.get_by_name(model_name).await?;
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
        }))
    }

    async fn resolve_to_canonical_name(&self, model_name: &str) -> Result<String> {
        self.resolve_to_canonical_name(model_name).await
    }
}
