use super::common::{
    create_org, get_session_id, setup_test_server_with_config_and_database, MOCK_USER_AGENT,
};
use serde_json::Value;
use uuid::Uuid;

#[tokio::test]
async fn current_user_reports_staking_organization_for_bound_wallet() {
    let (server, database) = setup_test_server_with_config_and_database(|config| {
        config.staking_farm.enabled = true;
        config.staking_farm.selected_org_binding_enabled = true;
        config.staking_farm.network_id = "testnet".into();
        config.staking_farm.contract_id = "stake.testnet".into();
        config.staking_farm.farm_product_id = "cloud".into();
        config.staking_farm.credit_nano_usd_per_reward_unit = 1_000_000_000;
    })
    .await;
    let org = create_org(&server).await;
    let org_id: Uuid = org.id.parse().unwrap();
    let account = format!("w{}.testnet", Uuid::new_v4().simple());
    let wallet_user = Uuid::new_v4();
    let client = database.pool().get().await.unwrap();
    client
        .execute(
            "INSERT INTO users(id,email,username,auth_provider,provider_user_id) \
             VALUES($1,$2,'wallet-user','near',$3)",
            &[
                &wallet_user,
                &format!("{wallet_user}@example.test"),
                &account,
            ],
        )
        .await
        .unwrap();
    client
        .execute(
            "INSERT INTO organization_members(organization_id,user_id,role) VALUES($1,$2,'admin')",
            &[&org_id, &wallet_user],
        )
        .await
        .unwrap();
    client
        .execute(
            "INSERT INTO organization_staking_farm_sources (id, organization_id, near_account_id, \
             network_id, contract_id, farm_product_id, credit_nano_usd_per_reward_unit, status, \
             sync_status, created_by_user_id, created_at, updated_at) \
             VALUES (uuid_generate_v4(), $1, $2, 'testnet', 'stake.testnet', 'cloud', \
             1000000000, 'active', 'never_synced', $3, now(), now())",
            &[&org_id, &account, &wallet_user],
        )
        .await
        .unwrap();

    let me = server
        .get("/v1/users/me")
        .add_header("Authorization", format!("Bearer rt_{wallet_user}"))
        .add_header("User-Agent", MOCK_USER_AGENT)
        .await;
    assert_eq!(me.status_code(), 200, "{}", me.text());
    let me = me.json::<Value>();
    assert_eq!(me["staking_organization_id"], org.id);
    assert!(me["organizations"]
        .as_array()
        .unwrap()
        .iter()
        .any(|membership| membership["id"] == org.id && membership["role"] == "admin"));
}

#[tokio::test]
async fn staking_sync_returns_not_found_for_missing_or_inactive_organizations() {
    let (server, database) = setup_test_server_with_config_and_database(|config| {
        config.staking_farm.enabled = true;
        config.staking_farm.selected_org_binding_enabled = true;
    })
    .await;
    let inactive_org = create_org(&server).await;
    let inactive_id: Uuid = inactive_org.id.parse().unwrap();
    database
        .pool()
        .get()
        .await
        .unwrap()
        .execute(
            "UPDATE organizations SET is_active = false WHERE id = $1",
            &[&inactive_id],
        )
        .await
        .unwrap();
    for organization in [Uuid::new_v4(), inactive_id] {
        let response = server
            .post(&format!(
                "/v1/organizations/{organization}/staking/farm/sync"
            ))
            .add_header("Authorization", format!("Bearer {}", get_session_id()))
            .add_header("User-Agent", MOCK_USER_AGENT)
            .await;
        assert_eq!(response.status_code(), 404, "{}", response.text());
        assert_eq!(response.json::<Value>()["error"]["type"], "not_found");
    }
}
