use chrono::{DateTime, Utc};
use database::DbPool;
use uuid::Uuid;

pub struct ServiceUsageRowIds {
    pub latest_id: Uuid,
    pub same_time_high_id: Uuid,
    pub same_time_low_id: Uuid,
    pub older_id: Uuid,
    pub org_id: Uuid,
    pub workspace_id: Uuid,
    pub api_key_id: Uuid,
    pub web_search_id: Uuid,
    pub other_workspace_id: Uuid,
    pub other_api_key_id: Uuid,
    pub file_search_id: Uuid,
    pub other_org_id: Uuid,
    pub other_org_workspace_id: Uuid,
    pub other_org_api_key_id: Uuid,
}

pub struct UsageRow {
    pub id: Uuid,
    pub org_id: Uuid,
    pub workspace_id: Uuid,
    pub api_key_id: Uuid,
    pub service_id: Uuid,
    pub quantity: i32,
    pub total_cost: i64,
    pub created_at: DateTime<Utc>,
}

pub fn ts(value: &str) -> DateTime<Utc> {
    DateTime::parse_from_rfc3339(value)
        .expect("fixture timestamp is valid RFC3339")
        .with_timezone(&Utc)
}

pub fn service_usage_rows(ids: ServiceUsageRowIds) -> [UsageRow; 7] {
    [
        UsageRow {
            id: ids.latest_id,
            org_id: ids.org_id,
            workspace_id: ids.workspace_id,
            api_key_id: ids.api_key_id,
            service_id: ids.web_search_id,
            quantity: 1,
            total_cost: 100,
            created_at: ts("2026-07-03T00:00:00Z"),
        },
        UsageRow {
            id: ids.same_time_high_id,
            org_id: ids.org_id,
            workspace_id: ids.workspace_id,
            api_key_id: ids.api_key_id,
            service_id: ids.web_search_id,
            quantity: 2,
            total_cost: 200,
            created_at: ts("2026-07-02T00:00:00Z"),
        },
        UsageRow {
            id: ids.same_time_low_id,
            org_id: ids.org_id,
            workspace_id: ids.workspace_id,
            api_key_id: ids.api_key_id,
            service_id: ids.web_search_id,
            quantity: 3,
            total_cost: 300,
            created_at: ts("2026-07-02T00:00:00Z"),
        },
        UsageRow {
            id: ids.older_id,
            org_id: ids.org_id,
            workspace_id: ids.workspace_id,
            api_key_id: ids.api_key_id,
            service_id: ids.web_search_id,
            quantity: 4,
            total_cost: 400,
            created_at: ts("2026-06-30T00:00:00Z"),
        },
        UsageRow {
            id: Uuid::new_v4(),
            org_id: ids.org_id,
            workspace_id: ids.other_workspace_id,
            api_key_id: ids.other_api_key_id,
            service_id: ids.web_search_id,
            quantity: 5,
            total_cost: 500,
            created_at: ts("2026-07-03T00:00:00Z"),
        },
        UsageRow {
            id: Uuid::new_v4(),
            org_id: ids.org_id,
            workspace_id: ids.workspace_id,
            api_key_id: ids.api_key_id,
            service_id: ids.file_search_id,
            quantity: 6,
            total_cost: 600,
            created_at: ts("2026-07-03T00:00:00Z"),
        },
        UsageRow {
            id: Uuid::new_v4(),
            org_id: ids.other_org_id,
            workspace_id: ids.other_org_workspace_id,
            api_key_id: ids.other_org_api_key_id,
            service_id: ids.web_search_id,
            quantity: 7,
            total_cost: 700,
            created_at: ts("2026-07-03T00:00:00Z"),
        },
    ]
}

pub async fn insert_usage(pool: &DbPool, row: &UsageRow) -> anyhow::Result<()> {
    pool.get()
        .await?
        .execute(
            r#"
            INSERT INTO organization_service_usage_log (
                id, organization_id, workspace_id, api_key_id, service_id,
                quantity, total_cost, inference_id, created_at
            )
            VALUES ($1, $2, $3, $4, $5, $6, $7, NULL, $8)
            "#,
            &[
                &row.id,
                &row.org_id,
                &row.workspace_id,
                &row.api_key_id,
                &row.service_id,
                &row.quantity,
                &row.total_cost,
                &row.created_at,
            ],
        )
        .await?;
    Ok(())
}
