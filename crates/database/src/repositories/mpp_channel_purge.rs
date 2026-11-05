use std::future::Future;
use std::sync::Mutex;

use services::common::RepositoryError;

use super::conversion;

/// The production bounded scan keeps its cursor across calls, including pages whose
/// recent rows cannot yet be deleted. Cursor locks cover memory access only.
pub(super) async fn scan<F, Fut>(
    saved_cursor: &Mutex<Vec<u8>>,
    page_size: i64,
    max_pages: u32,
    mut page: F,
) -> Result<u64, RepositoryError>
where
    F: FnMut(Vec<u8>) -> Fut,
    Fut: Future<Output = Result<(Vec<Vec<u8>>, u64), RepositoryError>>,
{
    let mut cursor = saved_cursor
        .lock()
        .map_err(|_| conversion("purge cursor"))?
        .clone();
    let mut purged = 0u64;
    for _ in 0..max_pages {
        let (ids, count) = page(cursor.clone()).await?;
        purged = purged.saturating_add(count);
        let full = i64::try_from(ids.len()).is_ok_and(|len| len >= page_size);
        match ids.last() {
            Some(last) if full => cursor = last.clone(),
            _ => {
                saved_cursor
                    .lock()
                    .map_err(|_| conversion("purge cursor"))?
                    .clear();
                break;
            }
        }
        *saved_cursor
            .lock()
            .map_err(|_| conversion("purge cursor"))? = cursor.clone();
    }
    Ok(purged)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn bounded_purge_second_call_reaches_expired_rows_after_recent_prefix() {
        let cursor = Mutex::new(Vec::new());
        let rows = [vec![1], vec![2], vec![3], vec![4], vec![5]];
        let mut examined = Vec::new();
        let mut page = |after: Vec<u8>| {
            let ids: Vec<Vec<u8>> = rows
                .iter()
                .filter(|id| **id > after)
                .take(2)
                .cloned()
                .collect();
            examined.extend(ids.iter().cloned());
            // The prefix is too recent. Only the last row is old enough to purge.
            let count = u64::from(ids.contains(&vec![5]));
            std::future::ready(Ok((ids, count)))
        };
        assert_eq!(scan(&cursor, 2, 2, &mut page).await.unwrap(), 0);
        assert_eq!(*cursor.lock().unwrap(), vec![4]);
        assert_eq!(scan(&cursor, 2, 2, &mut page).await.unwrap(), 1);
        assert!(cursor.lock().unwrap().is_empty());
        assert_eq!(examined, rows);
    }
}
