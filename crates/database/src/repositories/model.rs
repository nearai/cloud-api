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
                    id, model_name, public_name, model_display_name, model_description, model_icon,
                    input_cost_per_token, output_cost_per_token,
                    context_length, verifiable, is_active, created_at, updated_at
                FROM models 
                WHERE is_active = true
                ORDER BY public_name ASC
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
                    id, model_name, public_name, model_display_name, model_description, model_icon,
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

    /// Get model by model name or public name (public API - only active models)
    /// Searches both model_name (internal) and public_name (public-facing) fields
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
                    id, model_name, public_name, model_display_name, model_description, model_icon,
                    input_cost_per_token, output_cost_per_token,
                    context_length, verifiable, is_active, created_at, updated_at
                FROM models 
                WHERE (model_name = $1 OR public_name = $1) AND is_active = true
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

        // Check if model exists
        let existing = self.get_by_internal_name(model_name).await?;

        let row = if existing.is_some() {
            // Model exists - do UPDATE (partial updates work)
            client
                .query_one(
                    r#"
                    UPDATE models SET
                        public_name = COALESCE($2, public_name),
                        input_cost_per_token = COALESCE($3, input_cost_per_token),
                        output_cost_per_token = COALESCE($4, output_cost_per_token),
                        model_display_name = COALESCE($5, model_display_name),
                        model_description = COALESCE($6, model_description),
                        model_icon = COALESCE($7, model_icon),
                        context_length = COALESCE($8, context_length),
                        verifiable = COALESCE($9, verifiable),
                        is_active = COALESCE($10, is_active),
                        updated_at = NOW()
                    WHERE model_name = $1
                    RETURNING id, model_name, public_name, model_display_name, model_description, model_icon,
                              input_cost_per_token, output_cost_per_token,
                              context_length, verifiable, is_active, created_at, updated_at
                    "#,
                    &[
                        &model_name,
                        &update_request.public_name,
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
                .context("Failed to update model pricing")?
        } else {
            // Model doesn't exist - do INSERT (need required fields)
            // Use existing values or default to model_name for public_name if not provided
            let public_name = update_request
                .public_name
                .as_ref()
                .or(Some(&model_name.to_string()))
                .cloned()
                .context("public_name is required for new models")?;

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

            client
                .query_one(
                    r#"
                    INSERT INTO models (
                        model_name, public_name,
                        input_cost_per_token, output_cost_per_token,
                        model_display_name, model_description, model_icon,
                        context_length, verifiable, is_active
                    ) VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10)
                    RETURNING id, model_name, public_name, model_display_name, model_description, model_icon,
                              input_cost_per_token, output_cost_per_token,
                              context_length, verifiable, is_active, created_at, updated_at
                    "#,
                    &[
                        &model_name,
                        &public_name,
                        &update_request.input_cost_per_token.unwrap_or(0),
                        &update_request.output_cost_per_token.unwrap_or(0),
                        &display_name,
                        &description,
                        &update_request.model_icon,
                        &context_length,
                        &update_request.verifiable.unwrap_or(true),
                        &update_request.is_active.unwrap_or(true),
                    ],
                )
                .await
                .context("Failed to insert new model")?
        };

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
                    model_name, public_name, model_display_name, model_description, model_icon,
                    input_cost_per_token, output_cost_per_token,
                    context_length, verifiable, is_active
                ) VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10)
                RETURNING id, model_name, public_name, model_display_name, model_description, model_icon,
                          input_cost_per_token, output_cost_per_token,
                          context_length, verifiable, is_active, created_at, updated_at
                "#,
                &[
                    &model.model_name,
                    &model.public_name,
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

    /// Get count of pricing history entries for a model by model name
    pub async fn count_pricing_history_by_name(&self, model_name: &str) -> Result<i64> {
        let client = self
            .pool
            .get()
            .await
            .context("Failed to get database connection")?;

        let row = client
            .query_one(
                r#"
                SELECT COUNT(*) as count
                FROM model_pricing_history h
                JOIN models m ON h.model_id = m.id
                WHERE m.model_name = $1
                "#,
                &[&model_name],
            )
            .await
            .context("Failed to count pricing history")?;
        Ok(row.get::<_, i64>("count"))
    }

    /// Get pricing history for a model by model name with pagination
    pub async fn get_pricing_history_by_name(
        &self,
        model_name: &str,
        limit: i64,
        offset: i64,
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
                LIMIT $2 OFFSET $3
                "#,
                &[&model_name, &limit, &offset],
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

    /// Get model by public name
    pub async fn get_by_public_name(&self, public_name: &str) -> Result<Option<Model>> {
        let client = self
            .pool
            .get()
            .await
            .context("Failed to get database connection")?;

        let rows = client
            .query(
                r#"
                SELECT
                    id, model_name, public_name, model_display_name, model_description, model_icon,
                    input_cost_per_token, output_cost_per_token,
                    context_length, verifiable, is_active, created_at, updated_at
                FROM models 
                WHERE public_name = $1
                "#,
                &[&public_name],
            )
            .await
            .context("Failed to query model by public name")?;

        if let Some(row) = rows.first() {
            Ok(Some(self.row_to_model(row)))
        } else {
            Ok(None)
        }
    }

    /// Get list of configured model names (public names)
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
                SELECT public_name
                FROM models
                WHERE is_active = true
                ORDER BY public_name ASC
                "#,
                &[],
            )
            .await
            .context("Failed to query configured model names")?;

        let names = rows
            .into_iter()
            .map(|row| row.get::<_, String>("public_name"))
            .collect();

        Ok(names)
    }

    /// Check if a public_name is already used by an active model
    /// Returns true if the public_name is already taken by an active model
    /// If exclude_model_name is provided, excludes that model from the check
    pub async fn is_public_name_taken_by_active_model(
        &self,
        public_name: &str,
        exclude_model_name: Option<&str>,
    ) -> Result<bool> {
        let client = self
            .pool
            .get()
            .await
            .context("Failed to get database connection")?;

        let rows = if let Some(exclude_name) = exclude_model_name {
            client
                .query(
                    r#"
                    SELECT 1
                    FROM models 
                    WHERE public_name = $1 AND is_active = true AND model_name != $2
                    LIMIT 1
                    "#,
                    &[&public_name, &exclude_name],
                )
                .await
                .context("Failed to check if public_name is taken by active model")?
        } else {
            client
                .query(
                    r#"
                    SELECT 1
                    FROM models 
                    WHERE public_name = $1 AND is_active = true
                    LIMIT 1
                    "#,
                    &[&public_name],
                )
                .await
                .context("Failed to check if public_name is taken by active model")?
        };

        Ok(!rows.is_empty())
    }

    /// Resolve a model identifier (public name, alias, or canonical name) to the canonical model name
    /// Resolution order:
    /// 1. Check if it's a public_name -> return model_name
    /// 2. Check if it's an alias -> return model_name
    /// 3. Check if it's already a canonical model_name -> return it
    /// 4. Otherwise, return the input as-is (will fail later if invalid)
    pub async fn resolve_to_canonical_name(&self, identifier: &str) -> Result<String> {
        let client = self
            .pool
            .get()
            .await
            .context("Failed to get database connection")?;

        // First, try to find by public_name
        let rows = client
            .query(
                r#"
                SELECT model_name
                FROM models
                WHERE public_name = $1 AND is_active = true
                LIMIT 1
                "#,
                &[&identifier],
            )
            .await
            .context("Failed to resolve public name to canonical name")?;

        if let Some(row) = rows.first() {
            return Ok(row.get::<_, String>("model_name"));
        }

        // Second, try to resolve as an alias
        let rows = client
            .query(
                r#"
                SELECT m.model_name
                FROM model_aliases ma
                JOIN models m ON ma.canonical_model_id = m.id
                WHERE ma.alias_name = $1 AND ma.is_active = true AND m.is_active = true
                LIMIT 1
                "#,
                &[&identifier],
            )
            .await
            .context("Failed to resolve alias to canonical name")?;

        if let Some(row) = rows.first() {
            return Ok(row.get::<_, String>("model_name"));
        }

        // Third, check if it's already a canonical model_name
        let rows = client
            .query(
                r#"
                SELECT model_name
                FROM models
                WHERE model_name = $1 AND is_active = true
                LIMIT 1
                "#,
                &[&identifier],
            )
            .await
            .context("Failed to check if identifier is canonical name")?;

        if let Some(row) = rows.first() {
            return Ok(row.get::<_, String>("model_name"));
        }

        // Not found in any form, return as-is (will be caught as invalid later)
        Ok(identifier.to_string())
    }

    /// Helper method to convert database row to Model
    fn row_to_model(&self, row: &Row) -> Model {
        Model {
            id: row.get("id"),
            model_name: row.get("model_name"),
            public_name: row.get("public_name"),
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
                public_name: m.public_name,
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
        let model_opt = self.get_active_model_by_name(model_name).await?;
        Ok(model_opt.map(|m| services::models::ModelWithPricing {
            id: m.id,
            model_name: m.model_name,
            public_name: m.public_name,
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

    async fn get_configured_model_names(&self) -> Result<Vec<String>> {
        self.get_configured_model_names().await
    }
}
