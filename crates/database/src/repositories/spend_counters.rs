use crate::repositories::utils::map_db_error;
use chrono::{DateTime, Utc};
use services::common::RepositoryError;
use tokio_postgres::Transaction;
use uuid::Uuid;

/// Add one usage charge to the per-key split counters inside its posting transaction.
pub async fn increment_api_key_spend(
    transaction: &Transaction<'_>,
    api_key_id: Uuid,
    inference_spent: i64,
    service_spent: i64,
    updated_at: DateTime<Utc>,
) -> Result<(), RepositoryError> {
    transaction
        .execute(
            r#"
            INSERT INTO api_key_spend (
                api_key_id, inference_spent, service_spent, updated_at
            ) VALUES ($1, $2, $3, $4)
            ON CONFLICT (api_key_id) DO UPDATE SET
                inference_spent = api_key_spend.inference_spent + $2,
                service_spent = api_key_spend.service_spent + $3,
                updated_at = $4
            "#,
            &[&api_key_id, &inference_spent, &service_spent, &updated_at],
        )
        .await
        .map_err(map_db_error)?;
    Ok(())
}
