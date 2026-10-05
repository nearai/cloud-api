use super::arm;
use crate::repositories::utils::map_db_error;
use services::common::RepositoryError;
use std::time::Instant;
use tokio_postgres::Transaction;

/// Total matching groups for a paginated report. The page's `total_groups`
/// column (`COUNT(*) OVER ()`, evaluated after GROUP BY/HAVING and before
/// LIMIT) carries it. An empty page needs the count query unless it is a
/// non-empty first page request, where empty means no matches at all.
pub(super) async fn page_total(
    tx: &Transaction<'_>,
    deadline: Instant,
    rows: &[tokio_postgres::Row],
    (limit, offset): (i64, i64),
    count_sql: &str,
    params: &[&(dyn tokio_postgres::types::ToSql + Sync)],
) -> Result<i64, RepositoryError> {
    if let Some(row) = rows.first() {
        return Ok(row.get("total_groups"));
    }
    if offset == 0 && limit > 0 {
        return Ok(0);
    }
    arm(tx, deadline).await?;
    Ok(tx
        .query_one(count_sql, params)
        .await
        .map_err(map_db_error)?
        .get(0))
}
