use super::super::{
    progress::Progress,
    types::{bad, internal, ApiResult, Mode, ObjectScanRequest, ScanCursor},
    worker::{process_key, storage_call},
    ObjectEncryptionState,
};
use axum::Json;
use serde_json::{json, Value};
use std::time::Duration;
use tokio::time::{timeout_at, Instant};
use uuid::Uuid;

pub(super) async fn run(state: &ObjectEncryptionState, req: ObjectScanRequest) -> ApiResult<Value> {
    let limit = req.limit.unwrap_or(10_000);
    if !(1..=100_000).contains(&limit) {
        return Err(bad("limit must be between 1 and 100000"));
    }
    let seconds = req.deadline_seconds.unwrap_or(60);
    if !(1..=60).contains(&seconds) {
        return Err(bad("deadline_seconds must be between 1 and 60"));
    }
    let deadline = Instant::now() + Duration::from_secs(seconds);
    let mut progress = Progress::default();
    let mut cursor = req.cursor.unwrap_or_default();
    if cursor.pending_keys.len() > 1000 || (cursor.complete && !cursor.pending_keys.is_empty()) {
        return Err(bad("invalid scan cursor"));
    }
    while !cursor.complete && progress.processed < limit && Instant::now() < deadline {
        if cursor.pending_keys.is_empty() {
            let cap = (limit - progress.processed).min(1000);
            let page = timeout_at(
                deadline,
                storage_call(state.storage.list_keys(
                    cursor.continuation.clone(),
                    i32::try_from(cap).map_err(internal)?,
                )),
            )
            .await;
            let (keys, next) = match page {
                Ok(result) => result.map_err(internal)?,
                Err(_) => break,
            };
            if i64::try_from(keys.len()).map_err(internal)? > cap
                || (next.is_some() && next == cursor.continuation)
            {
                return Err(internal("invalid object page"));
            }
            cursor.pending_keys = keys.into();
            cursor.next_continuation = next;
            if cursor.pending_keys.is_empty() {
                advance_page(&mut cursor);
                continue;
            }
        }
        let Some(key) = cursor.pending_keys.front() else {
            return Err(internal("invalid scan cursor"));
        };
        match timeout_at(
            deadline,
            process_key(state, Mode::DryRun, key, &mut progress),
        )
        .await
        {
            Ok(result) => result.map_err(internal)?,
            Err(_) => break,
        }
        cursor.last_key = cursor.pending_keys.pop_front();
        if cursor.pending_keys.is_empty() {
            advance_page(&mut cursor);
        }
    }
    progress.complete = cursor.complete;
    let totals = super::public_progress(serde_json::to_value(progress).map_err(internal)?);
    Ok(Json(json!({
        "run_id":Uuid::new_v4(), "status":"completed", "complete":cursor.complete,
        "totals":totals, "cursor":cursor
    })))
}

fn advance_page(cursor: &mut ScanCursor) {
    cursor.continuation = cursor.next_continuation.take();
    cursor.complete = cursor.continuation.is_none();
}
