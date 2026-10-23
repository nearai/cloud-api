use super::*;

#[tokio::test]
async fn login_writes_do_not_touch_an_inactive_user() {
    let (_server, database) = setup_test_server_with_database().await;
    let (_s, user_id, _email) = new_user(&database).await;
    let client = database.pool().get().await.unwrap();
    client
        .execute(
            "UPDATE users SET is_active = false, last_login_at = NULL WHERE id = $1",
            &[&user_id],
        )
        .await
        .unwrap();

    let users = database::repositories::UserRepository::new(database.pool().clone());
    users.update_last_login(user_id).await.unwrap();
    services::auth::UserRepository::update_email(
        &users,
        services::auth::UserId(user_id),
        "leak@test.com".to_string(),
    )
    .await
    .unwrap();

    let row = client
        .query_one(
            "SELECT last_login_at, email FROM users WHERE id = $1",
            &[&user_id],
        )
        .await
        .unwrap();
    assert!(row
        .get::<_, Option<chrono::DateTime<chrono::Utc>>>("last_login_at")
        .is_none());
    assert_ne!(row.get::<_, String>("email"), "leak@test.com");

    let sessions = database::repositories::SessionRepository::new(database.pool().clone());
    assert!(sessions
        .create(user_id, None, "test-agent".to_string(), 1)
        .await
        .is_err());
    assert_eq!(
        count(
            &client,
            "SELECT COUNT(*) FROM refresh_tokens WHERE user_id = $1",
            &user_id
        )
        .await,
        0
    );
}

#[tokio::test]
async fn delivery_rows_are_not_written_for_inactive_recipients() {
    let (server, database) = setup_test_server_with_database().await;
    let (_s, user_id, _email) = new_user(&database).await;
    let client = database.pool().get().await.unwrap();
    client
        .execute(
            "UPDATE users SET is_active = false WHERE id = $1",
            &[&user_id],
        )
        .await
        .unwrap();
    let org: uuid::Uuid = client
        .query_one(
            "SELECT organization_id FROM organization_members WHERE user_id = $1",
            &[&user_id],
        )
        .await
        .unwrap()
        .get(0);
    let model_name = setup_qwen_model(&server).await;
    let model_id: uuid::Uuid = client
        .query_one(
            "SELECT id FROM models WHERE model_name = $1",
            &[&model_name],
        )
        .await
        .unwrap()
        .get(0);

    let repo = database::repositories::AdminCompositeRepository::new(database.pool().clone());
    let record = services::admin::ModelDeprecationDeliveryRecord {
        model_id,
        model_name: model_name.clone(),
        model_display_name: model_name.clone(),
        successor_model_name: model_name.clone(),
        deprecation_date: chrono::Utc::now(),
        recipient_user_id: user_id,
        recipient_email: "x@test.com".to_string(),
        organization_id: org,
        organization_name: "erasure-test-org".to_string(),
        status: services::admin::ModelDeprecationEmailStatus::Sent,
        email_message_id: None,
        email_last_error: None,
        initiated_by_user_id: None,
        initiated_by_user_email: None,
    };
    services::admin::AdminRepository::record_model_deprecation_delivery(&repo, record)
        .await
        .unwrap();

    assert_eq!(
        count(
            &client,
            "SELECT COUNT(*) FROM model_deprecation_email_deliveries WHERE recipient_user_id = $1",
            &user_id
        )
        .await,
        0
    );
}

#[tokio::test]
async fn is_user_active_reflects_erasure_state() {
    let (_server, database) = setup_test_server_with_database().await;
    let (_s, user_id, _email) = new_user(&database).await;
    let repo = database::repositories::AdminCompositeRepository::new(database.pool().clone());
    assert!(
        services::admin::AdminRepository::is_user_active(&repo, user_id)
            .await
            .unwrap()
    );
    database
        .pool()
        .get()
        .await
        .unwrap()
        .execute(
            "UPDATE users SET is_active = false WHERE id = $1",
            &[&user_id],
        )
        .await
        .unwrap();
    assert!(
        !services::admin::AdminRepository::is_user_active(&repo, user_id)
            .await
            .unwrap()
    );
    assert!(
        !services::admin::AdminRepository::is_user_active(&repo, uuid::Uuid::new_v4())
            .await
            .unwrap()
    );
}

#[tokio::test]
async fn pricing_change_delivery_rows_are_not_written_for_inactive_recipients() {
    let (_server, database) = setup_test_server_with_database().await;
    let (_s, inactive_user, _email) = new_user(&database).await;
    let (_s2, active_user, _email2) = new_user(&database).await;
    let client = database.pool().get().await.unwrap();
    client
        .execute(
            "UPDATE users SET is_active = false WHERE id = $1",
            &[&inactive_user],
        )
        .await
        .unwrap();
    let org_of = |user: uuid::Uuid| {
        let client = &client;
        async move {
            client
                .query_one(
                    "SELECT organization_id FROM organization_members WHERE user_id = $1",
                    &[&user],
                )
                .await
                .unwrap()
                .get::<_, uuid::Uuid>(0)
        }
    };
    let repo = database::repositories::AdminCompositeRepository::new(database.pool().clone());
    let batch_id = uuid::Uuid::new_v4();
    for user in [inactive_user, active_user] {
        let record = services::admin::PricingChangeDeliveryRecord {
            batch_id,
            recipient_user_id: user,
            recipient_email: "x@test.com".to_string(),
            organization_id: org_of(user).await,
            organization_name: "erasure-test-org".to_string(),
            model_names: vec![format!("erasure-test-model-{batch_id}")],
            status: services::admin::ModelDeprecationEmailStatus::Sent,
            email_message_id: None,
            email_last_error: None,
            initiated_by_user_id: None,
            initiated_by_user_email: None,
        };
        services::admin::AdminRepository::record_pricing_change_delivery(&repo, record)
            .await
            .unwrap();
    }

    let sql =
        "SELECT COUNT(*) FROM model_pricing_change_email_deliveries WHERE recipient_user_id = $1";
    assert_eq!(count(&client, sql, &inactive_user).await, 0);
    assert_eq!(
        count(&client, sql, &active_user).await,
        1,
        "positive control: an active recipient gets a row"
    );
}
