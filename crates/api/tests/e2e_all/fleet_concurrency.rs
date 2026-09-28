use crate::common;
use inference_providers::mock::ResponseTemplate;
use std::time::Duration;
use uuid::Uuid;

const INSTANCES: usize = 4;
const LIMIT: i32 = 2;
const ATTEMPTS: usize = 8;
const HOLD: Duration = Duration::from_secs(2);
const MODEL: &str = "Qwen/Qwen3-30B-A3B-Instruct-2507";

async fn set_rate_limit(database: &database::Database, organization_id: &str, limit: i32) {
    let organization_id = Uuid::parse_str(organization_id).expect("organization id is a uuid");
    database
        .pool()
        .get()
        .await
        .expect("database connection")
        .execute(
            "UPDATE organizations SET rate_limit = $1 WHERE id = $2",
            &[&limit, &organization_id],
        )
        .await
        .expect("rate limit update");
}

async fn live_lease_count(database: &database::Database, organization_id: &str) -> i64 {
    let organization_id = Uuid::parse_str(organization_id).expect("organization id is a uuid");
    database
        .pool()
        .get()
        .await
        .expect("database connection")
        .query_one(
            "SELECT COUNT(*) FROM concurrency_leases
             WHERE organization_id = $1 AND expires_at > NOW()",
            &[&organization_id],
        )
        .await
        .expect("lease count")
        .get(0)
}

/// Four replicas sharing one database must not admit more than the
/// organization's limit between them. Without fleet-wide accounting each
/// replica counts only itself, so this admits INSTANCES * LIMIT.
#[tokio::test]
async fn fleet_limit_is_shared_across_replicas() {
    let (servers, mocks, database) = common::setup_test_fleet(INSTANCES, |config| {
        config.fleet_concurrency.mode = config::FleetConcurrencyMode::Enforce;
    })
    .await;

    for mock in &mocks {
        mock.set_default_response(ResponseTemplate::new("held").with_hold(HOLD))
            .await;
    }

    let org = common::setup_org_with_credits(&servers[0], 100_000_000_000).await;
    let api_key = common::get_api_key_for_org(&servers[0], org.id.clone()).await;
    set_rate_limit(&database, &org.id, LIMIT).await;

    let attempts = (0..ATTEMPTS).map(|attempt| {
        let server = &servers[attempt % INSTANCES];
        let api_key = api_key.clone();
        async move {
            server
                .post("/v1/chat/completions")
                .add_header("Authorization", format!("Bearer {api_key}"))
                .json(&serde_json::json!({
                    "model": MODEL,
                    "messages": [{ "role": "user", "content": "hello" }]
                }))
                .await
                .status_code()
                .as_u16()
        }
    });

    let statuses = futures::future::join_all(attempts).await;
    let admitted = statuses.iter().filter(|status| **status == 200).count();
    let rejected = statuses.iter().filter(|status| **status == 429).count();

    assert_eq!(
        admitted, LIMIT as usize,
        "expected the fleet to admit exactly {LIMIT} of {ATTEMPTS}, got {statuses:?}"
    );
    assert_eq!(
        rejected,
        ATTEMPTS - LIMIT as usize,
        "every attempt over the limit should be rejected, got {statuses:?}"
    );
}

/// A finished request must give its slot back, or the organization stays
/// locked out until the leases expire.
#[tokio::test]
async fn completed_requests_release_their_lease() {
    let (servers, mocks, database) = common::setup_test_fleet(1, |config| {
        config.fleet_concurrency.mode = config::FleetConcurrencyMode::Enforce;
    })
    .await;

    mocks[0]
        .set_default_response(ResponseTemplate::new("done"))
        .await;

    let org = common::setup_org_with_credits(&servers[0], 100_000_000_000).await;
    let api_key = common::get_api_key_for_org(&servers[0], org.id.clone()).await;
    set_rate_limit(&database, &org.id, LIMIT).await;

    for _ in 0..(ATTEMPTS as i32 + LIMIT) {
        let status = servers[0]
            .post("/v1/chat/completions")
            .add_header("Authorization", format!("Bearer {api_key}"))
            .json(&serde_json::json!({
                "model": MODEL,
                "messages": [{ "role": "user", "content": "hello" }]
            }))
            .await
            .status_code();
        assert_eq!(
            status, 200,
            "a released slot should let the next request straight through"
        );
    }

    tokio::time::sleep(Duration::from_millis(500)).await;
    assert_eq!(
        live_lease_count(&database, &org.id).await,
        0,
        "leases outlived the requests that took them"
    );
}
