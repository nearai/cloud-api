use chrono::{DateTime, Utc};
use uuid::Uuid;

use crate::DbPool;

/// The allowlisted fields of a single admin request.
pub struct AdminRequest {
    pub request_id: Uuid,
    pub occurred_at: DateTime<Utc>,
    pub route_template: String,
    pub method: &'static str,
    pub status: Option<i16>,
    pub auth_outcome: &'static str,
}

pub async fn insert(pool: &DbPool, event: &AdminRequest) -> anyhow::Result<()> {
    let client = pool.get().await?;
    client
        .execute(
            "INSERT INTO public.admin_request_audit
             (request_id, occurred_at, service, route_template, method, status, auth_outcome)
             VALUES ($1, $2, 'cloud-api', $3, $4, $5, $6)
             ON CONFLICT DO NOTHING",
            &[
                &event.request_id,
                &event.occurred_at,
                &event.route_template,
                &event.method,
                &event.status,
                &event.auth_outcome,
            ],
        )
        .await?;
    Ok(())
}
