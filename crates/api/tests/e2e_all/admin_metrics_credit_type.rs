use crate::admin_provider_attribution_support::setup_platform_provider_usage_fixture;
use crate::common::*;
use chrono::Utc;
use serde_json::{json, Value};
use uuid::Uuid;

#[tokio::test]
async fn admin_metrics_credit_type_uses_saved_allocations() {
    let fixture = setup_platform_provider_usage_fixture().await;
    let client = fixture.database.pool().get().await.unwrap();
    let org = fixture.organization_id;
    let mut limits = std::collections::HashMap::new();
    for kind in ["grant", "payment", "postpay"] {
        // Deliberately expired and with no current capacity: attribution must
        // come from the immutable ledger, not current credit configuration.
        let id: Uuid = client.query_one(
            "INSERT INTO organization_limits_history (organization_id, credit_type, spend_limit, effective_until)
             VALUES ($1, $2, 0, NOW() - INTERVAL '1 day') RETURNING id",
            &[&org, &kind],
        ).await.unwrap().get(0);
        limits.insert(kind, id);
    }
    let mut ids = Vec::new();
    for (date, dollars) in [
        ("2026-09-01T00:00:00Z", 10_i64),
        ("2026-09-02T00:00:00Z", 5),
        ("2026-09-03T00:00:00Z", 7),   // historical, no attribution
        ("2026-09-04T00:00:00Z", 0),   // zero-cost request
        ("2026-09-05T00:00:00Z", 6),   // payment only
        ("2026-09-22T00:00:00Z", 100), // exclusive end
        ("2026-08-31T23:59:59Z", 100), // before start
    ] {
        let id = Uuid::new_v4();
        let created_at = chrono::DateTime::parse_from_rfc3339(date)
            .unwrap()
            .with_timezone(&Utc);
        let cost = dollars * 1_000_000_000;
        client
            .execute(
                "INSERT INTO organization_usage_log
             (id, organization_id, workspace_id, api_key_id, model_id, model_name,
              input_tokens, output_tokens, cache_read_tokens, total_tokens,
              input_cost, output_cost, total_cost, request_type, created_at)
             VALUES ($1,$2,$3,$4,$5,$6,100,20,10,120,$7,0,$7,'chat_completion',$8)",
                &[
                    &id,
                    &org,
                    &fixture.workspace_id,
                    &fixture.api_key_id,
                    &fixture.model_id,
                    &fixture.model_name,
                    &cost,
                    &created_at,
                ],
            )
            .await
            .unwrap();
        ids.push(id);
    }
    for (index, kind, dollars, phase) in [
        (0, "grant", 3_i64, "posting"),
        (0, "payment", 2, "posting"),
        (0, "postpay", 1, "posting"),
        (0, "postpay", 4, "overage_settlement"),
        (1, "postpay", 5, "overage_settlement"),
        (4, "payment", 6, "posting"),
        (5, "postpay", 100, "posting"),
        (6, "postpay", 100, "posting"),
    ] {
        client
            .execute(
                "INSERT INTO usage_credit_allocations
             (organization_id, inference_usage_id, credit_type, amount, organization_limit_id,
              policy_version, priority_position, allocation_phase, created_at)
             VALUES ($1,$2,$3,$4,$5,'test',0,$6,'2026-10-01')",
                &[
                    &org,
                    &ids[index],
                    &kind,
                    &(dollars * 1_000_000_000),
                    &limits[kind],
                    &phase,
                ],
            )
            .await
            .unwrap();
    }
    for (index, funded, unfunded) in [
        (0, 6_i64, 4_i64),
        (1, 0, 5),
        (3, 0, 0),
        (4, 6, 0),
        (5, 100, 0),
        (6, 100, 0),
    ] {
        client
            .execute(
                "UPDATE organization_usage_log SET funded_amount = $2, unfunded_amount = $3,
             allocation_policy_version = 'test' WHERE id = $1",
                &[
                    &ids[index],
                    &(funded * 1_000_000_000),
                    &(unfunded * 1_000_000_000),
                ],
            )
            .await
            .unwrap();
    }
    // Platform-service allocations must not expand the existing inference-only scope.
    let service: Uuid = client
        .query_one(
            "INSERT INTO services (service_name, display_name, unit, cost_per_unit)
         VALUES ($1,'Credit metrics fixture','request',1000000000) RETURNING id",
            &[&format!("credit-metrics-{}", Uuid::new_v4())],
        )
        .await
        .unwrap()
        .get(0);
    let service_usage: Uuid = client
        .query_one(
            "INSERT INTO organization_service_usage_log
         (organization_id,workspace_id,api_key_id,service_id,quantity,total_cost,created_at)
         VALUES ($1,$2,$3,$4,1,1000000000,'2026-09-02') RETURNING id",
            &[&org, &fixture.workspace_id, &fixture.api_key_id, &service],
        )
        .await
        .unwrap()
        .get(0);
    client.execute(
        "INSERT INTO usage_credit_allocations
         (organization_id,service_usage_id,credit_type,amount,organization_limit_id,policy_version,priority_position)
         VALUES ($1,$2,'postpay',1000000000,$3,'test',0)",
        &[&org, &service_usage, &limits["postpay"]],
    ).await.unwrap();
    // Two sources for postpay, but only two matching inference requests.
    let range = "start=2026-09-01T00:00:00Z&end=2026-09-22T00:00:00Z";
    for (filter, cost, requests, days) in [
        ("", 28.0, 5_i64, 5_usize),
        ("&credit_type=postpay", 10.0, 2, 2),
        ("&credit_type=POSTPAY", 10.0, 2, 2),
        ("&credit_type=grant", 3.0, 1, 1),
        ("&credit_type=payment", 8.0, 2, 2),
        ("&credit_type=staking_farm", 0.0, 0, 0),
    ] {
        let response = fixture
            .server
            .get(&format!(
                "/v1/admin/organizations/{org}/metrics?{range}{filter}"
            ))
            .add_header("Authorization", format!("Bearer {}", get_session_id()))
            .await;
        assert_eq!(response.status_code(), 200, "{}", response.text());
        let metrics: Value = response.json();
        assert_eq!(metrics["summary"]["total_cost_usd"], cost, "{filter}");
        assert_eq!(metrics["summary"]["total_requests"], requests, "{filter}");
        assert_eq!(metrics["summary"]["total_input_tokens"], requests * 100);
        assert_eq!(metrics["summary"]["total_output_tokens"], requests * 20);
        assert_eq!(metrics["summary"]["total_cache_read_tokens"], requests * 10);
        assert_eq!(
            metrics["summary"]["unique_api_keys"],
            i64::from(requests > 0)
        );
        for dimension in ["by_workspace", "by_api_key", "by_model"] {
            let rows = metrics[dimension].as_array().unwrap();
            assert_eq!(
                rows.iter()
                    .map(|row| row["cost_usd"].as_f64().unwrap())
                    .sum::<f64>(),
                cost
            );
            assert_eq!(
                rows.iter()
                    .map(|row| row["requests"].as_i64().unwrap())
                    .sum::<i64>(),
                requests
            );
        }
        for granularity in ["hour", "day", "week"] {
            let response = fixture.server.get(&format!("/v1/admin/organizations/{org}/metrics/timeseries?{range}{filter}&granularity={granularity}"))
                .add_header("Authorization", format!("Bearer {}", get_session_id())).await;
            assert_eq!(response.status_code(), 200, "{}", response.text());
            let metrics: Value = response.json();
            let points = metrics["data"].as_array().unwrap();
            if granularity == "day" {
                assert_eq!(points.len(), days);
            }
            assert_eq!(
                points
                    .iter()
                    .map(|p| p["cost_usd"].as_f64().unwrap())
                    .sum::<f64>(),
                cost
            );
            assert_eq!(
                points
                    .iter()
                    .map(|p| p["requests"].as_i64().unwrap())
                    .sum::<i64>(),
                requests
            );
            for (field, per_request) in [
                ("input_tokens", 100),
                ("output_tokens", 20),
                ("cache_read_tokens", 10),
            ] {
                assert_eq!(
                    points
                        .iter()
                        .map(|p| p[field].as_i64().unwrap())
                        .sum::<i64>(),
                    requests * per_request
                );
            }
        }
    }
    for suffix in ["", "/timeseries"] {
        let path = format!("/v1/admin/organizations/{org}/metrics{suffix}");
        for invalid in ["unknown", "", "post-pay", "postpay%20"] {
            let response = fixture
                .server
                .get(&format!("{path}?{range}&credit_type={invalid}"))
                .add_header("Authorization", format!("Bearer {}", get_session_id()))
                .await;
            assert_eq!(response.status_code(), 400, "{}", response.text());
            assert!(response.json::<Value>()["error"]["message"]
                .as_str()
                .unwrap()
                .contains("invalid credit_type"));
        }
        let response = fixture
            .server
            .get(&format!("{path}?{range}&credit_type=postpay"))
            .await;
        assert_eq!(response.status_code(), 401);
    }
}

#[tokio::test]
async fn admin_metrics_credit_type_allows_read_only_tokens() {
    let (server, _db) = setup_test_server_with_config_and_database(|config| {
        config.auth.admin_read_only_tokens_enabled = true;
    })
    .await;
    let org = create_org(&server).await;
    let response = server.post("/v1/admin/access-tokens")
        .add_header("Authorization", format!("Bearer {}", get_session_id()))
        .add_header("User-Agent", MOCK_USER_AGENT)
        .json(&json!({"name":"credit-metrics", "reason":"test", "expires_in_hours":24, "permission":"read_only"})).await;
    assert_eq!(response.status_code(), 200, "{}", response.text());
    let token = response
        .json::<api::models::AdminAccessTokenResponse>()
        .access_token;
    for suffix in ["", "/timeseries"] {
        let response = server
            .get(&format!(
                "/v1/admin/organizations/{}/metrics{suffix}?credit_type=postpay",
                org.id
            ))
            .add_header("Authorization", format!("Bearer {token}"))
            .add_header("User-Agent", MOCK_USER_AGENT)
            .await;
        assert_eq!(response.status_code(), 200, "{}", response.text());
    }
}

#[test]
fn admin_metrics_credit_type_is_documented() {
    use utoipa::OpenApi;
    let spec = serde_json::to_value(api::openapi::ApiDoc::openapi()).unwrap();
    let mut descriptions = Vec::new();
    for path in [
        "/v1/admin/organizations/{org_id}/metrics",
        "/v1/admin/organizations/{org_id}/metrics/timeseries",
    ] {
        let parameter = spec["paths"][path]["get"]["parameters"]
            .as_array()
            .unwrap()
            .iter()
            .find(|p| p["name"] == "credit_type")
            .expect("credit_type parameter");
        assert_eq!(parameter["in"], "query");
        assert_eq!(parameter["required"], false);
        descriptions.push(parameter["description"].clone());
        assert!(parameter["description"]
            .as_str()
            .unwrap()
            .contains("settlements"));
    }
    assert_eq!(descriptions[0], descriptions[1]);
}
