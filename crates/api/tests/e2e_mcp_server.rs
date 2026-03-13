mod common;

use common::*;
use serde_json::json;

#[tokio::test]
async fn test_mcp_requires_api_key_auth() {
    let (server, _database) = setup_test_server_with_mock_web_search().await;

    let response = server
        .post("/mcp")
        .json(&json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": {}
        }))
        .await;

    assert_eq!(response.status_code(), 401);
}

#[tokio::test]
async fn test_mcp_tools_list_exposes_web_search() {
    let (server, _database) = setup_test_server_with_mock_web_search().await;
    let org = setup_org_with_credits(&server, 10_000_000_000i64).await;
    let api_key = get_api_key_for_org(&server, org.id.clone()).await;

    let initialize = server
        .post("/mcp")
        .add_header("Authorization", format!("Bearer {api_key}"))
        .json(&json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": {
                "protocolVersion": "2024-11-05"
            }
        }))
        .await;

    assert_eq!(initialize.status_code(), 200, "{}", initialize.text());
    let initialize_body = initialize.json::<serde_json::Value>();
    assert_eq!(initialize_body["result"]["serverInfo"]["name"], "cloud-api");

    let response = server
        .post("/mcp")
        .add_header("Authorization", format!("Bearer {api_key}"))
        .json(&json!({
            "jsonrpc": "2.0",
            "id": 2,
            "method": "tools/list",
            "params": {}
        }))
        .await;

    assert_eq!(response.status_code(), 200, "{}", response.text());
    let body = response.json::<serde_json::Value>();
    let tools = body["result"]["tools"]
        .as_array()
        .expect("tools should be an array");
    assert_eq!(tools.len(), 1);
    assert_eq!(tools[0]["name"], "web_search");
    assert_eq!(tools[0]["inputSchema"]["required"], json!(["query"]));
    assert_eq!(tools[0]["inputSchema"]["properties"]["count"]["default"], 5);
    assert_eq!(
        tools[0]["inputSchema"]["properties"]["count"]["maximum"],
        20
    );
}

#[tokio::test]
async fn test_mcp_tool_call_records_web_search_usage() {
    let (server, _database) = setup_test_server_with_mock_web_search().await;

    let org = setup_org_with_credits(&server, 10_000_000_000i64).await;
    let api_key = get_api_key_for_org(&server, org.id.clone()).await;
    let created = get_or_create_web_search_service(&server).await;

    let response = server
        .post("/mcp")
        .add_header("Authorization", format!("Bearer {api_key}"))
        .json(&json!({
            "jsonrpc": "2.0",
            "id": 3,
            "method": "tools/call",
            "params": {
                "name": "web_search",
                "arguments": {
                    "query": "test query",
                    "count": 5
                }
            }
        }))
        .await;

    assert_eq!(response.status_code(), 200, "{}", response.text());
    let body = response.json::<serde_json::Value>();

    assert_eq!(
        body["result"]["structuredContent"]["query"],
        json!("test query")
    );
    assert_eq!(body["result"]["structuredContent"]["result_count"], 1);
    assert_eq!(
        body["result"]["structuredContent"]["results"][0]["title"],
        json!("Mock Result")
    );

    let history = server
        .get(
            format!(
                "/v1/organizations/{}/service-usage/history?limit=10&serviceName=web_search",
                org.id
            )
            .as_str(),
        )
        .add_header("Authorization", format!("Bearer {}", get_session_id()))
        .add_header("User-Agent", MOCK_USER_AGENT)
        .await;

    assert_eq!(history.status_code(), 200, "{}", history.text());
    let history_body = history.json::<api::routes::usage::ServiceUsageHistoryResponse>();
    assert!(!history_body.data.is_empty());
    assert_eq!(history_body.data[0].quantity, 1);
    assert_eq!(history_body.data[0].total_cost, created.cost_per_unit);
}

#[tokio::test]
async fn test_mcp_tool_call_rejects_invalid_count() {
    let (server, _database) = setup_test_server_with_mock_web_search().await;
    let org = setup_org_with_credits(&server, 10_000_000_000i64).await;
    let api_key = get_api_key_for_org(&server, org.id.clone()).await;

    let response = server
        .post("/mcp")
        .add_header("Authorization", format!("Bearer {api_key}"))
        .json(&json!({
            "jsonrpc": "2.0",
            "id": 4,
            "method": "tools/call",
            "params": {
                "name": "web_search",
                "arguments": {
                    "query": "test query",
                    "count": 21
                }
            }
        }))
        .await;

    assert_eq!(response.status_code(), 200, "{}", response.text());
    let body = response.json::<serde_json::Value>();
    assert_eq!(body["error"]["code"], -32602);
}
