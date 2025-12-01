use crate::models::ModelAlias;
use crate::pool::DbPool;
use crate::repositories::utils::map_db_error;
use crate::retry_db;
use anyhow::{Context, Result};
use services::common::RepositoryError;
use tokio_postgres::Row;
use uuid::Uuid;

#[derive(Debug, Clone)]
pub struct ModelAliasRepository {
    pool: DbPool,
}

impl ModelAliasRepository {
    pub fn new(pool: DbPool) -> Self {
        Self { pool }
    }

    /// Upsert aliases for a model (replaces all existing aliases)
    pub async fn upsert_aliases_for_model(
        &self,
        canonical_model_id: &Uuid,
        alias_names: &[String],
    ) -> Result<Vec<ModelAlias>> {
        let aliases = retry_db!("upsert_model_aliases", {
            let mut client = self
                .pool
                .get()
                .await
                .context("Failed to get database connection")
                .map_err(RepositoryError::PoolError)?;

            let transaction = client.transaction().await.map_err(map_db_error)?;

            // Delete existing aliases for this model
            transaction
                .execute(
                    "DELETE FROM model_aliases WHERE canonical_model_id = $1",
                    &[&canonical_model_id],
                )
                .await
                .map_err(map_db_error)?;

            // Insert new aliases
            let mut aliases = Vec::new();
            for alias_name in alias_names {
                let row = transaction
                    .query_one(
                        r#"
                        INSERT INTO model_aliases (
                            alias_name, canonical_model_id, is_active
                        ) VALUES ($1, $2, true)
                        RETURNING id, alias_name, canonical_model_id,
                                  is_active, created_at, updated_at
                        "#,
                        &[&alias_name, &canonical_model_id],
                    )
                    .await
                    .map_err(map_db_error)?;

                aliases.push(self.row_to_alias(&row));
            }

            transaction.commit().await.map_err(map_db_error)?;

            Ok::<Vec<ModelAlias>, RepositoryError>(aliases)
        })?;

        Ok(aliases)
    }

    /// Helper method to convert database row to ModelAlias
    fn row_to_alias(&self, row: &Row) -> ModelAlias {
        ModelAlias {
            id: row.get("id"),
            alias_name: row.get("alias_name"),
            canonical_model_id: row.get("canonical_model_id"),
            is_active: row.get("is_active"),
            created_at: row.get("created_at"),
            updated_at: row.get("updated_at"),
        }
    }
}
