use crate::DbPool;
use anyhow::{bail, Context, Result};

const REQUIRED_USAGE_REPORTING_INDEXES: [(&str, &str); 6] = [
    (
        "idx_org_usage_reporting_org_created_id",
        "organization_usage_log",
    ),
    (
        "idx_org_usage_reporting_org_workspace_created_id",
        "organization_usage_log",
    ),
    (
        "idx_org_usage_reporting_org_api_key_created_id",
        "organization_usage_log",
    ),
    (
        "idx_org_service_usage_reporting_org_created_id",
        "organization_service_usage_log",
    ),
    (
        "idx_org_service_usage_reporting_org_workspace_created_id",
        "organization_service_usage_log",
    ),
    (
        "idx_org_service_usage_reporting_org_api_key_created_id",
        "organization_service_usage_log",
    ),
];

pub async fn ensure_usage_reporting_indexes(pool: &DbPool) -> Result<()> {
    let client = pool
        .get()
        .await
        .context("Failed to get database connection for reporting index check")?;
    let required_names: Vec<&str> = REQUIRED_USAGE_REPORTING_INDEXES
        .iter()
        .map(|(index_name, _)| *index_name)
        .collect();
    let required_tables: Vec<&str> = REQUIRED_USAGE_REPORTING_INDEXES
        .iter()
        .map(|(_, table_name)| *table_name)
        .collect();
    let rows = client
        .query(
            r#"
            WITH required AS (
                SELECT *
                FROM UNNEST($1::TEXT[], $2::TEXT[]) AS item(index_name, table_name)
            )
            SELECT required.index_name
            FROM required
            LEFT JOIN pg_namespace AS namespace
              ON namespace.nspname = current_schema()
            LEFT JOIN pg_class AS table_class
              ON table_class.relnamespace = namespace.oid
             AND table_class.relname = required.table_name
             AND table_class.relkind IN ('r', 'p')
            LEFT JOIN pg_class AS index_class
              ON index_class.relnamespace = namespace.oid
             AND index_class.relname = required.index_name
             AND index_class.relkind = 'i'
            LEFT JOIN pg_index
              ON pg_index.indexrelid = index_class.oid
             AND pg_index.indrelid = table_class.oid
            WHERE index_class.oid IS NULL
               OR pg_index.indexrelid IS NULL
               OR NOT pg_index.indisvalid
               OR NOT pg_index.indisready
            ORDER BY required.index_name
            "#,
            &[&required_names, &required_tables],
        )
        .await
        .context("Failed to verify usage reporting indexes")?;
    let missing: Vec<String> = rows.into_iter().map(|row| row.get(0)).collect();
    if !missing.is_empty() {
        bail!(
            "usage reporting cannot be enabled; missing, invalid, or unready indexes: {}",
            missing.join(", ")
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    #[test]
    fn reporting_index_prerequisites_cover_both_usage_tables() {
        assert_eq!(REQUIRED_USAGE_REPORTING_INDEXES.len(), 6);
        assert_eq!(
            REQUIRED_USAGE_REPORTING_INDEXES
                .iter()
                .map(|(index_name, _)| *index_name)
                .collect::<HashSet<_>>()
                .len(),
            6
        );
        assert_eq!(
            REQUIRED_USAGE_REPORTING_INDEXES
                .iter()
                .filter(|(_, table)| *table == "organization_usage_log")
                .count(),
            3
        );
        assert_eq!(
            REQUIRED_USAGE_REPORTING_INDEXES
                .iter()
                .filter(|(_, table)| *table == "organization_service_usage_log")
                .count(),
            3
        );
    }
}
