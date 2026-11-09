use super::{lease::Lease, progress::Cursor};
use serde_json::json;
use uuid::Uuid;

#[test]
fn cursor_round_trip_preserves_the_lease_and_pending_page() {
    // Given a cursor with non-default page state and an expiring lease.
    let token = Uuid::from_u128(42);
    let cursor = Cursor {
        continuation: Some("page-2".into()),
        last_key: Some("workspace/last".into()),
        pending_keys: vec!["workspace/next".into()],
        pending_next: Some("page-3".into()),
        lease: Some(Lease {
            token,
            expires_at: 123_456,
        }),
        ..Cursor::default()
    };
    // When it is serialized and decoded without any database or clock.
    let encoded = serde_json::to_value(&cursor).unwrap();
    let decoded: Cursor = serde_json::from_value(encoded.clone()).unwrap();
    // Then the JSON contract and recovered lease match the original inputs.
    assert_eq!(
        encoded["lease"],
        json!({"token":token.to_string(),"expires_at":123_456})
    );
    let lease = decoded.lease.as_ref().unwrap();
    assert_eq!(lease.token, token);
    assert_ne!(lease.token, Uuid::from_u128(43));
    assert_eq!(lease.expires_at, 123_456);
    assert_eq!(decoded.pending_keys, ["workspace/next"]);
    assert_eq!(decoded.continuation.as_deref(), Some("page-2"));
    assert_eq!(decoded.last_key.as_deref(), Some("workspace/last"));
    assert_eq!(decoded.pending_next.as_deref(), Some("page-3"));
    assert!(!decoded.complete);
}

#[test]
fn legacy_cursor_without_a_lease_remains_readable() {
    // Given a persisted cursor created before leases were added.
    let encoded = json!({"last_key":"workspace/old","complete":true});
    // When it is decoded and encoded again.
    let decoded: Cursor = serde_json::from_value(encoded).unwrap();
    let encoded = serde_json::to_value(&decoded).unwrap();
    // Then the old progress survives and no lease is invented.
    assert!(decoded.lease.is_none());
    assert!(decoded.complete);
    assert_eq!(decoded.last_key.as_deref(), Some("workspace/old"));
    assert!(encoded.get("lease").is_none());
}

#[test]
fn malformed_lease_token_or_expiry_is_rejected() {
    // Given invalid persisted lease fields.
    for lease in [
        json!({"token":"invalid-uuid","expires_at":123}),
        json!({"token":Uuid::nil(),"expires_at":"tomorrow"}),
        json!({"token":Uuid::nil()}),
    ] {
        // When decoded, then malformed leases cannot become owned cursors.
        assert!(serde_json::from_value::<Cursor>(json!({"lease":lease})).is_err());
    }
}

#[test]
fn every_object_lifecycle_statement_has_the_shared_kind_predicate() {
    for source in [
        include_str!("../object_encryption.rs"),
        include_str!("lease.rs"),
        include_str!("endpoints.rs"),
    ] {
        for line in source.lines().filter(|line| {
            line.contains("FROM database_encryption_jobs")
                || line.contains("UPDATE database_encryption_jobs")
        }) {
            assert!(
                line.contains("{OBJECT_JOB_KIND_FILTER}"),
                "missing shared object kind guard: {line}"
            );
        }
    }
}
