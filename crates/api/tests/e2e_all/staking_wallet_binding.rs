use super::common::{
    create_org, get_session_id, setup_test_server_with_config_and_database, MOCK_USER_AGENT,
};
use serde_json::Value;
use uuid::Uuid;

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
