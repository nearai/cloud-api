use super::*;

#[test]
fn every_job_lifecycle_statement_uses_the_shared_kind_filter() {
    assert_eq!(DATABASE_JOB_KIND_FILTER, "COALESCE(scope->>'kind','') = ''");
    let sources = [
        include_str!("../database_encryption.rs"),
        include_str!("batch_delay.rs"),
    ];
    let mut statements = 0;
    for source in sources {
        for line in source.lines().filter(|line| {
            line.contains("FROM database_encryption_jobs")
                || line.contains("UPDATE database_encryption_jobs")
        }) {
            statements += 1;
            assert!(
                line.contains("WHERE {DATABASE_JOB_KIND_FILTER} AND"),
                "job statement lacks the shared ownership predicate: {line}"
            );
        }
    }
    assert_eq!(statements, 11, "cover every lifecycle statement");
}

#[tokio::test]
async fn any_present_scope_kind_is_rejected_before_database_access() {
    let pool = DbPool::uninitialized();
    pool.set_encryption_ring(KeyRing::new("db-v2".into(), [9; 32], vec![]).unwrap());
    let state = DatabaseEncryptionState::new(pool).unwrap();
    let admin = AdminUser(database::User {
        id: Uuid::nil(),
        email: "admin@example.com".into(),
        username: "admin".into(),
        display_name: None,
        avatar_url: None,
        created_at: Utc::now(),
        updated_at: Utc::now(),
        last_login_at: None,
        is_active: true,
        auth_provider: "google".into(),
        provider_user_id: "admin".into(),
        tokens_revoked_at: None,
        contact_email: None,
        contact_email_verified_at: None,
        email_opt_in_at: None,
    });
    for kind in [
        json!("objects"),
        json!(""),
        Value::Null,
        json!(0),
        json!({}),
    ] {
        let request = serde_json::from_value(json!({
            "mode":"dry_run", "actions":["encrypt"],
            "scope":{"kind":kind,"fields":[{"table":"files","column":"filename"}]}
        }))
        .unwrap();
        let (status, body) = create_job(
            Extension(state.clone()),
            Extension(admin.clone()),
            Json(request),
        )
        .await
        .expect_err("kind must be rejected before acquiring a connection");
        assert_eq!(status, StatusCode::BAD_REQUEST, "kind={kind}");
        assert_eq!(
            body["error"]["message"],
            "scope.kind is not supported for database jobs"
        );
    }
}

#[test]
fn ordinary_database_scope_serializes_without_a_kind() {
    let scope: Scope = serde_json::from_value(json!({
        "fields":[{"table":"files","column":"filename"}]
    }))
    .unwrap();
    assert_eq!(
        serde_json::to_value(scope).unwrap(),
        json!({
            "tables":[],"fields":[{"table":"files","column":"filename"}]
        })
    );
}
