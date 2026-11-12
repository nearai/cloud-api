use super::super::{
    progress::Progress,
    types::{
        bad, internal, ApiResult, Mode, ObjectScanRequest, ObjectScanResponse, ScanCursor,
        SignedScanCursor,
    },
    worker::{process_key, storage_call},
    ObjectEncryptionState,
};
use axum::Json;
use serde::Serialize;
use std::time::Duration;
use tokio::time::{timeout_at, Instant};
use uuid::Uuid;

const SCAN_CURSOR_VERSION: u8 = 1;
const SCAN_CURSOR_PURPOSE: &[u8] = b"NEAR-AI-OBJECT-ENCRYPTION-SCAN-CURSOR\0v1\0";

#[derive(Serialize)]
struct ScanCursorSigningPayload<'a> {
    purpose: &'static str,
    version: u8,
    key_id: &'a str,
    state: &'a ScanCursor,
}

impl SignedScanCursor {
    fn sign(
        state: ScanCursor,
        ring: &services::files::encryption::ObjectKeyRing,
    ) -> anyhow::Result<Self> {
        let key_id = ring.write_id().to_owned();
        let message = Self::signing_message(&key_id, &state)?;
        Ok(Self {
            version: SCAN_CURSOR_VERSION,
            key_id,
            state,
            tag: hex::encode(ring.sign(&message)),
        })
    }

    fn verify(self, ring: &services::files::encryption::ObjectKeyRing) -> Option<ScanCursor> {
        if self.version != SCAN_CURSOR_VERSION {
            return None;
        }
        let tag = hex::decode(&self.tag).ok()?;
        let message = Self::signing_message(&self.key_id, &self.state).ok()?;
        ring.verify(&self.key_id, &message, &tag)
            .then_some(self.state)
    }

    fn signing_message(key_id: &str, state: &ScanCursor) -> anyhow::Result<Vec<u8>> {
        let payload = serde_json::to_vec(&ScanCursorSigningPayload {
            purpose: "object-encryption.scan.cursor",
            version: SCAN_CURSOR_VERSION,
            key_id,
            state,
        })?;
        let mut message = Vec::with_capacity(SCAN_CURSOR_PURPOSE.len() + payload.len());
        message.extend_from_slice(SCAN_CURSOR_PURPOSE);
        message.extend_from_slice(&payload);
        Ok(message)
    }
}

pub(super) async fn run(
    state: &ObjectEncryptionState,
    req: ObjectScanRequest,
) -> ApiResult<ObjectScanResponse> {
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
    let mut cursor = match req.cursor {
        Some(cursor) => cursor
            .verify(&state.ring)
            .ok_or_else(|| bad("invalid scan cursor"))?,
        None => ScanCursor::default(),
    };
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
    let complete = cursor.complete;
    let cursor = SignedScanCursor::sign(cursor, &state.ring).map_err(internal)?;
    Ok(Json(ObjectScanResponse {
        run_id: Uuid::new_v4(),
        status: "completed".into(),
        complete,
        totals,
        cursor,
    }))
}

fn advance_page(cursor: &mut ScanCursor) {
    cursor.continuation = cursor.next_continuation.take();
    cursor.complete = cursor.continuation.is_none();
}

#[cfg(test)]
mod tests {
    use super::*;
    use services::files::encryption::ObjectKeyRing;
    use std::collections::VecDeque;

    #[test]
    fn signed_cursor_binds_state_and_allows_configured_key_rotation() {
        // Given a cursor signed by a key that remains in the rotated read ring.
        let state = ScanCursor {
            continuation: Some("continue".into()),
            last_key: Some("last".into()),
            complete: false,
            pending_keys: VecDeque::from(["pending".into()]),
            next_continuation: Some("next".into()),
        };
        let old_ring = ObjectKeyRing::new("old".into(), [1; 32], vec![]).unwrap();
        let signed = SignedScanCursor::sign(state, &old_ring).unwrap();
        let rotated_ring =
            ObjectKeyRing::new("current".into(), [2; 32], vec![("old".into(), [1; 32])]).unwrap();

        // When authenticating the old cursor with the retained reader.
        let authenticated = signed.clone().verify(&rotated_ring);

        // Then the authenticated state is preserved, while removed signers and any mutation fail.
        assert_eq!(authenticated, Some(signed.state.clone()));
        let no_old_reader = ObjectKeyRing::new("current".into(), [2; 32], vec![]).unwrap();
        assert!(signed.clone().verify(&no_old_reader).is_none());

        let mut altered = signed.clone();
        altered.state.continuation = Some("altered".into());
        assert!(altered.verify(&rotated_ring).is_none());
        let mut altered = signed.clone();
        altered.state.last_key = Some("altered".into());
        assert!(altered.verify(&rotated_ring).is_none());
        let mut altered = signed.clone();
        altered.state.complete = true;
        assert!(altered.verify(&rotated_ring).is_none());
        let mut altered = signed.clone();
        altered.state.pending_keys.push_back("attacker".into());
        assert!(altered.verify(&rotated_ring).is_none());
        let mut altered = signed.clone();
        altered.state.next_continuation = Some("altered".into());
        assert!(altered.verify(&rotated_ring).is_none());
        let mut altered = signed.clone();
        altered.key_id = "unknown".into();
        assert!(altered.verify(&rotated_ring).is_none());
        let mut altered = signed.clone();
        altered.version = 2;
        assert!(altered.verify(&rotated_ring).is_none());
        let mut altered = signed;
        altered.tag = "not-hex".into();
        assert!(altered.verify(&rotated_ring).is_none());
    }
}
