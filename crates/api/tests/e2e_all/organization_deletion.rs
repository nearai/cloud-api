use crate::common::*;

#[tokio::test]
async fn test_delete_organization_bound_to_staking_wallet_returns_409() {
    let (server, database) = setup_test_server_with_database().await;
    let org = create_org(&server).await;
    let org_id = uuid::Uuid::parse_str(&org.id).expect("organization id should be a UUID");
    let user_id = uuid::Uuid::parse_str(MOCK_USER_ID).expect("mock user id should be a UUID");
    let near_account_id = format!("staking-{}.near", uuid::Uuid::new_v4());

    {
        let client = database
            .pool()
            .get()
            .await
            .expect("failed to get database connection");
        client
            .execute(
                r#"
                INSERT INTO organization_staking_farm_sources (
                    organization_id,
                    near_account_id,
                    network_id,
                    contract_id,
                    farm_product_id,
                    farm_price_id,
                    credit_nano_usd_per_reward_unit,
                    status,
                    sync_status,
                    created_by_user_id
                )
                VALUES ($1, $2, 'testnet', 'stake.testnet', 'cloud-credits', 'price-test', 1000000000, 'active', 'never_synced', $3)
                "#,
                &[&org_id, &near_account_id, &user_id],
            )
            .await
            .expect("failed to insert staking farm source");
    }

    let response = server
        .delete(format!("/v1/organizations/{}", org.id).as_str())
        .add_header("Authorization", format!("Bearer {}", get_session_id()))
        .add_header("User-Agent", MOCK_USER_AGENT)
        .await;

    assert_eq!(response.status_code(), 409);
    let error = response.json::<api::models::ErrorResponse>();
    assert_eq!(error.error.r#type, "staking_wallet_bound");
    assert!(error.error.message.contains("NEAR staking wallet"));

    let client = database
        .pool()
        .get()
        .await
        .expect("failed to get database connection");
    let row = client
        .query_one(
            r#"
            SELECT
                o.is_active,
                (SELECT COUNT(*) FROM organization_staking_farm_sources WHERE organization_id = o.id) AS source_count
            FROM organizations o
            WHERE o.id = $1
            "#,
            &[&org_id],
        )
        .await
        .expect("failed to fetch organization deletion state");
    assert!(row.get::<_, bool>("is_active"));
    assert_eq!(row.get::<_, i64>("source_count"), 1);
}
