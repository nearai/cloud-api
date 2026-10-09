use super::*;

#[tokio::test]
async fn concurrent_erase_executes_once() {
    let (server, database) = setup_test_server_with_database().await;
    let (_session, user_id, email) = new_user(&database).await;
    let (a, b) = tokio::join!(
        erase(&server, user_id, &email),
        erase(&server, user_id, &email)
    );
    assert!(a.status_code().is_success(), "{}", a.text());
    assert!(b.status_code().is_success(), "{}", b.text());
    let fresh = [a, b]
        .iter()
        .filter(|r| r.json::<serde_json::Value>()["already_erased"] == false)
        .count();
    assert_eq!(fresh, 1, "exactly one call performs the erasure");
    let client = database.pool().get().await.unwrap();
    assert_eq!(
        count(
            &client,
            "SELECT COUNT(*) FROM user_erasure_log WHERE user_id = $1",
            &user_id
        )
        .await,
        1
    );
}

/// Backend pid of a pooled connection, used to scope lock-wait polling to this test.
async fn backend_pid(client: &deadpool_postgres::Object) -> i32 {
    client
        .query_one("SELECT pg_backend_pid()", &[])
        .await
        .unwrap()
        .get(0)
}

/// Polls `pg_stat_activity` (10s deadline) until a backend whose query matches `query_like`
/// is waiting on a lock held by `holder_pid`. Scoping by the blocking pid keeps this
/// independent of other tests running concurrently against the same database.
async fn wait_until_blocked_by(
    client: &deadpool_postgres::Object,
    holder_pid: i32,
    query_like: &str,
) {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    loop {
        let waiting: i64 = client
            .query_one(
                "SELECT COUNT(*) FROM pg_stat_activity \
                 WHERE wait_event_type = 'Lock' AND query LIKE $1 \
                   AND pid <> pg_backend_pid() \
                   AND $2 = ANY(pg_blocking_pids(pid))",
                &[&query_like, &holder_pid],
            )
            .await
            .unwrap()
            .get(0);
        if waiting > 0 {
            return;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "no backend matching {query_like:?} became blocked by pid {holder_pid}"
        );
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
}

#[tokio::test]
async fn login_waits_for_erasure_and_writes_no_token() {
    let (_server, database) = setup_test_server_with_database().await;
    let (_s, user_id, _email) = new_user(&database).await;
    let holder = database.pool().get().await.unwrap();
    let holder_pid = backend_pid(&holder).await;
    let observer = database.pool().get().await.unwrap();

    // Stand in for erasure: hold the user row lock.
    holder.batch_execute("BEGIN").await.unwrap();
    holder
        .execute("SELECT 1 FROM users WHERE id = $1 FOR UPDATE", &[&user_id])
        .await
        .unwrap();

    let sessions = database::repositories::SessionRepository::new(database.pool().clone());
    let login = tokio::spawn(async move {
        sessions
            .create(user_id, None, "test-agent".to_string(), 1)
            .await
    });
    wait_until_blocked_by(&observer, holder_pid, "%INSERT INTO refresh_tokens%").await;

    holder
        .execute(
            "UPDATE users SET is_active = false WHERE id = $1",
            &[&user_id],
        )
        .await
        .unwrap();
    holder.batch_execute("COMMIT").await.unwrap();

    let result = login.await.unwrap();
    assert!(
        result.is_err(),
        "login must fail once erasure deactivated the user"
    );
    assert_eq!(
        count(
            &observer,
            "SELECT COUNT(*) FROM refresh_tokens WHERE user_id = $1",
            &user_id
        )
        .await,
        0
    );
}

#[tokio::test]
async fn login_before_erasure_token_is_deleted_by_erasure() {
    let (server, database) = setup_test_server_with_database().await;
    let (_s, user_id, email) = new_user(&database).await;
    let holder = database.pool().get().await.unwrap();
    let holder_pid = backend_pid(&holder).await;
    let observer = database.pool().get().await.unwrap();

    // Stand in for a login that has inserted its token but not yet committed: the same
    // INSERT ... SELECT ... FOR SHARE that SessionRepository::create runs.
    holder.batch_execute("BEGIN").await.unwrap();
    let inserted = holder
        .execute(
            "INSERT INTO refresh_tokens (id, user_id, token_hash, created_at, expires_at, user_agent) \
             SELECT $1, $2, $3, NOW(), NOW() + INTERVAL '1 hour', 'test-agent' \
             WHERE EXISTS (SELECT 1 FROM users WHERE id = $2 AND is_active = true FOR SHARE)",
            &[
                &uuid::Uuid::new_v4(),
                &user_id,
                &format!("{:064x}", uuid::Uuid::new_v4().as_u128()),
            ],
        )
        .await
        .unwrap();
    assert_eq!(inserted, 1);

    let erasure = erase(&server, user_id, &email);
    let release = async {
        wait_until_blocked_by(
            &observer,
            holder_pid,
            "%FROM users WHERE id = $1 FOR UPDATE%",
        )
        .await;
        holder.batch_execute("COMMIT").await.unwrap();
    };
    let (response, ()) = tokio::join!(erasure, release);
    assert_eq!(response.status_code(), 200, "{}", response.text());
    assert_eq!(
        count(
            &observer,
            "SELECT COUNT(*) FROM refresh_tokens WHERE user_id = $1",
            &user_id
        )
        .await,
        0,
        "a token committed just before erasure must be deleted by it"
    );
}

#[tokio::test]
async fn delivery_upsert_waits_for_erasure_and_writes_nothing() {
    let (server, database) = setup_test_server_with_database().await;
    let (_s, user_id, email) = new_user(&database).await;
    let observer = database.pool().get().await.unwrap();
    let org: uuid::Uuid = observer
        .query_one(
            "SELECT organization_id FROM organization_members WHERE user_id = $1",
            &[&user_id],
        )
        .await
        .unwrap()
        .get(0);
    let model_name = setup_qwen_model(&server).await;
    let model_id: uuid::Uuid = observer
        .query_one(
            "SELECT id FROM models WHERE model_name = $1",
            &[&model_name],
        )
        .await
        .unwrap()
        .get(0);
    let deprecation_date = chrono::Utc::now();
    let batch_id = uuid::Uuid::new_v4();
    let model_names = vec![format!("erasure-test-model-{batch_id}")];

    let deprecation_record = {
        let (model_name, email) = (model_name.clone(), email.clone());
        move || services::admin::ModelDeprecationDeliveryRecord {
            model_id,
            model_name: model_name.clone(),
            model_display_name: model_name.clone(),
            successor_model_name: model_name.clone(),
            deprecation_date,
            recipient_user_id: user_id,
            recipient_email: email.clone(),
            organization_id: org,
            organization_name: "erasure-test-org".to_string(),
            status: services::admin::ModelDeprecationEmailStatus::Sent,
            email_message_id: None,
            email_last_error: None,
            initiated_by_user_id: None,
            initiated_by_user_email: None,
        }
    };
    let pricing_record = {
        let (model_names, email) = (model_names.clone(), email.clone());
        move || services::admin::PricingChangeDeliveryRecord {
            batch_id,
            recipient_user_id: user_id,
            recipient_email: email.clone(),
            organization_id: org,
            organization_name: "erasure-test-org".to_string(),
            model_names: model_names.clone(),
            status: services::admin::ModelDeprecationEmailStatus::Sent,
            email_message_id: None,
            email_last_error: None,
            initiated_by_user_id: None,
            initiated_by_user_email: None,
        }
    };

    // Existing rows for the active user, written with the real email.
    let repo = database::repositories::AdminCompositeRepository::new(database.pool().clone());
    services::admin::AdminRepository::record_model_deprecation_delivery(
        &repo,
        deprecation_record(),
    )
    .await
    .unwrap();
    services::admin::AdminRepository::record_pricing_change_delivery(&repo, pricing_record())
        .await
        .unwrap();

    let holder = database.pool().get().await.unwrap();
    let holder_pid = backend_pid(&holder).await;
    let tomb = format!("erased+{user_id}@erased.invalid");

    // Stand in for erasure: hold the user row lock.
    holder.batch_execute("BEGIN").await.unwrap();
    holder
        .execute("SELECT 1 FROM users WHERE id = $1 FOR UPDATE", &[&user_id])
        .await
        .unwrap();

    let deprecation_task = {
        let repo = database::repositories::AdminCompositeRepository::new(database.pool().clone());
        let record = deprecation_record();
        tokio::spawn(async move {
            services::admin::AdminRepository::record_model_deprecation_delivery(&repo, record).await
        })
    };
    let pricing_task = {
        let repo = database::repositories::AdminCompositeRepository::new(database.pool().clone());
        let record = pricing_record();
        tokio::spawn(async move {
            services::admin::AdminRepository::record_pricing_change_delivery(&repo, record).await
        })
    };
    wait_until_blocked_by(
        &observer,
        holder_pid,
        "%INSERT INTO model_deprecation_email_deliveries%",
    )
    .await;
    wait_until_blocked_by(
        &observer,
        holder_pid,
        "%INSERT INTO model_pricing_change_email_deliveries%",
    )
    .await;

    // Mimic what erasure commits.
    holder
        .execute(
            "UPDATE users SET is_active = false, email = $2 WHERE id = $1",
            &[&user_id, &tomb],
        )
        .await
        .unwrap();
    holder
        .execute(
            "UPDATE model_deprecation_email_deliveries SET recipient_email = $2 \
             WHERE recipient_user_id = $1",
            &[&user_id, &tomb],
        )
        .await
        .unwrap();
    holder
        .execute(
            "UPDATE model_pricing_change_email_deliveries SET recipient_email = $2 \
             WHERE recipient_user_id = $1",
            &[&user_id, &tomb],
        )
        .await
        .unwrap();
    holder.batch_execute("COMMIT").await.unwrap();

    deprecation_task.await.unwrap().unwrap();
    pricing_task.await.unwrap().unwrap();

    for table in [
        "model_deprecation_email_deliveries",
        "model_pricing_change_email_deliveries",
    ] {
        let rows = observer
            .query(
                format!("SELECT recipient_email FROM {table} WHERE recipient_user_id = $1")
                    .as_str(),
                &[&user_id],
            )
            .await
            .unwrap();
        assert_eq!(rows.len(), 1, "{table}: the existing row must remain");
        assert_eq!(
            rows[0].get::<_, String>(0),
            tomb,
            "{table}: a delivery upsert that raced erasure must not restore the real email"
        );
    }
}
