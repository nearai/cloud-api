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

    /// Upsert aliases for a model (replaces all existing aliases without visibility gaps).
    ///
    /// Uses a merge approach instead of DELETE-then-INSERT to avoid downtime during
    /// cross-model alias reassignment:
    /// 1. UPSERT new aliases (atomically reassigns from another model if needed)
    /// 2. DELETE stale aliases that no longer belong to this model
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

            if alias_names.is_empty() {
                // No aliases — just remove all for this model.
                transaction
                    .execute(
                        "DELETE FROM model_aliases WHERE canonical_model_id = $1",
                        &[&canonical_model_id],
                    )
                    .await
                    .map_err(map_db_error)?;
            } else {
                // 1. Batch upsert — atomically reassigns aliases that may point to
                //    another model, acquiring row locks in a consistent order to
                //    avoid deadlocks between concurrent calls.
                let alias_name_refs: Vec<&str> = alias_names.iter().map(|s| s.as_str()).collect();
                transaction
                    .execute(
                        r#"
                        INSERT INTO model_aliases (alias_name, canonical_model_id, is_active)
                        SELECT unnest($1::text[]), $2, true
                        ON CONFLICT (alias_name) DO UPDATE SET
                            canonical_model_id = EXCLUDED.canonical_model_id,
                            is_active = true,
                            updated_at = NOW()
                        "#,
                        &[&alias_name_refs, &canonical_model_id],
                    )
                    .await
                    .map_err(map_db_error)?;

                // 2. Delete stale aliases no longer in the new list.
                transaction
                    .execute(
                        "DELETE FROM model_aliases WHERE canonical_model_id = $1 AND alias_name != ALL($2)",
                        &[&canonical_model_id, &alias_name_refs],
                    )
                    .await
                    .map_err(map_db_error)?;
            }

            // 3. Return the final set of aliases for this model.
            let rows = transaction
                .query(
                    r#"
                    SELECT id, alias_name, canonical_model_id, is_active, created_at, updated_at
                    FROM model_aliases
                    WHERE canonical_model_id = $1
                    "#,
                    &[&canonical_model_id],
                )
                .await
                .map_err(map_db_error)?;

            transaction.commit().await.map_err(map_db_error)?;

            Ok::<Vec<ModelAlias>, RepositoryError>(
                rows.iter().map(|r| self.row_to_alias(r)).collect(),
            )
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
