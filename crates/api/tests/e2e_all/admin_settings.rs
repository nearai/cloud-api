use crate::common::*;
use axum::http::StatusCode;
use serde_json::{json, Value};

const PATH: &str = "/v1/admin/settings/placement";

/// The settings are global, so this file is a serialized override in
/// `.config/nextest.toml`, and each test ends by resetting every knob.
async fn patch_as(
    server: &axum_test::TestServer,
    token: &str,
    path: &str,
    body: Value,
) -> axum_test::TestResponse {
    server
        .patch(path)
        .add_header("Authorization", format!("Bearer {token}"))
        .add_header("User-Agent", MOCK_USER_AGENT)
        .json(&body)
        .await
}

async fn patch(server: &axum_test::TestServer, body: Value) -> axum_test::TestResponse {
    patch_as(server, &get_session_id(), PATH, body).await
}

async fn get_as(
    server: &axum_test::TestServer,
    token: &str,
    path: &str,
) -> axum_test::TestResponse {
    server
        .get(path)
        .add_header("Authorization", format!("Bearer {token}"))
        .add_header("User-Agent", MOCK_USER_AGENT)
        .await
}

async fn placement(server: &axum_test::TestServer) -> Value {
    let response = get_as(server, &get_session_id(), PATH).await;
    assert_eq!(response.status_code(), StatusCode::OK);
    response.json::<Value>()
}

fn assert_defaults(setting: &Value) {
    assert_eq!(setting["key"], json!("placement"));
    let v = &setting["value"];
    assert_eq!(v["affinity_abs_slack"], json!(0.25));
    assert_eq!(v["affinity_eps"], json!(0.25));
    assert_eq!(v["kv_max"], json!(0.95));
    assert_eq!(v["lane_load_tokens"], json!(64_000));
    assert_eq!(v["pin_hold_factor"], json!(1.0));
    assert_eq!(v["pin_ttl_ms"], json!(600_000));
    assert_eq!(v["enabled"], json!(true));
}

async fn reset(server: &axum_test::TestServer) {
    let response = patch(
        server,
        json!({
            "affinity_abs_slack": null, "affinity_eps": null, "kv_max": null,
            "lane_load_tokens": null, "pin_hold_factor": null, "pin_ttl_ms": null,
            "enabled": null
        }),
    )
    .await;
    assert_eq!(response.status_code(), StatusCode::OK);
    assert_defaults(&response.json());
}

#[tokio::test]
async fn patch_then_get_merges_validates_and_resets() {
    let server = setup_test_server().await;
    reset(&server).await;
    let setting = placement(&server).await;
    assert_defaults(&setting);
    assert_eq!(setting["updated_by_user_id"], json!(MOCK_USER_ID));

    // A partial patch changes only what it names.
    let response = patch(&server, json!({"kv_max": 0.9, "pin_ttl_ms": 120_000})).await;
    assert_eq!(response.status_code(), StatusCode::OK);
    let v = placement(&server).await["value"].clone();
    assert_eq!(v["kv_max"], json!(0.9));
    assert_eq!(v["pin_ttl_ms"], json!(120_000));
    assert_eq!(v["affinity_abs_slack"], json!(0.25));

    // null resets one field; the other keeps its value.
    assert_eq!(
        patch(&server, json!({"kv_max": null})).await.status_code(),
        StatusCode::OK
    );
    let v = placement(&server).await["value"].clone();
    assert_eq!(v["kv_max"], json!(0.95));
    assert_eq!(v["pin_ttl_ms"], json!(120_000));

    // Invalid values are a 400 and store nothing, even beside a valid field.
    for body in [
        json!({"affinity_abs_slack": 4.5}),
        json!({"affinity_abs_slack": -0.1}),
        json!({"affinity_eps": 2.5}),
        json!({"kv_max": 0.4}),
        json!({"kv_max": 1.1}),
        json!({"lane_load_tokens": 3_999}),
        json!({"lane_load_tokens": -5}),
        json!({"pin_hold_factor": 2.01}),
        json!({"pin_hold_factor": 0.49}),
        json!({"pin_ttl_ms": 59_999}),
        json!({"pin_ttl_ms": 3_600_001}),
        json!({"enabled": "no"}),
        json!({"enabled": 0}),
        json!({"not_a_knob": 1}),
        json!({"affinity_eps": 0.5, "pin_ttl_ms": 1}),
        json!([1]),
    ] {
        let response = patch(&server, body.clone()).await;
        assert_eq!(response.status_code(), StatusCode::BAD_REQUEST, "{body}");
    }
    let v = placement(&server).await["value"].clone();
    assert_eq!(v["affinity_eps"], json!(0.25));
    assert_eq!(v["pin_ttl_ms"], json!(120_000));

    reset(&server).await;
}

#[tokio::test]
async fn list_get_and_unknown_keys() {
    let server = setup_test_server().await;
    reset(&server).await;

    let all = get_as(&server, &get_session_id(), "/v1/admin/settings").await;
    assert_eq!(all.status_code(), StatusCode::OK);
    let all = all.json::<Value>();
    let entry = all["settings"]
        .as_array()
        .unwrap()
        .iter()
        .find(|s| s["key"] == "placement")
        .expect("placement is listed");
    assert_defaults(entry);

    let token = get_session_id();
    assert_eq!(
        get_as(&server, &token, "/v1/admin/settings/nope")
            .await
            .status_code(),
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        patch_as(&server, &token, "/v1/admin/settings/nope", json!({"a": 1}))
            .await
            .status_code(),
        StatusCode::NOT_FOUND
    );
}

#[tokio::test]
async fn read_only_token_can_get_but_not_patch() {
    let server = setup_test_server_with_config(|config| {
        config.auth.admin_read_only_tokens_enabled = true;
    })
    .await;
    reset(&server).await;

    let issued = server
        .post("/v1/admin/access-tokens")
        .add_header("Authorization", format!("Bearer {}", get_session_id()))
        .add_header("User-Agent", MOCK_USER_AGENT)
        .json(&json!({
            "name": "settings-read-only", "reason": "test",
            "expires_in_hours": 1, "permission": "read_only"
        }))
        .await;
    assert_eq!(issued.status_code(), StatusCode::OK);
    let token = issued.json::<Value>()["access_token"]
        .as_str()
        .unwrap()
        .to_string();

    for path in [PATH, "/v1/admin/settings"] {
        assert_eq!(
            get_as(&server, &token, path).await.status_code(),
            StatusCode::OK
        );
    }
    let write = patch_as(&server, &token, PATH, json!({"kv_max": 0.8})).await;
    assert_eq!(write.status_code(), StatusCode::FORBIDDEN);
    assert_defaults(&placement(&server).await);

    reset(&server).await;
}

#[tokio::test]
async fn attested_3p_kill_switch_round_trips_and_validates() {
    let server = setup_test_server().await;
    let token = get_session_id();
    let path = "/v1/admin/settings/attested_3p";

    let response = patch_as(
        &server,
        &token,
        path,
        json!({"disabled_sources": ["chutes"]}),
    )
    .await;
    assert_eq!(response.status_code(), StatusCode::OK);
    let got = get_as(&server, &token, path).await;
    assert_eq!(got.status_code(), StatusCode::OK);
    assert_eq!(
        got.json::<Value>()["value"],
        json!({"disabled_sources": ["chutes"]})
    );

    for body in [
        json!({"disabled_sources": ["vllm"]}),
        json!({"disabled_sources": ["nope"]}),
        json!({"other": 1}),
    ] {
        let response = patch_as(&server, &token, path, body.clone()).await;
        assert_eq!(response.status_code(), StatusCode::BAD_REQUEST, "{body}");
    }
    let got = get_as(&server, &token, path).await;
    assert_eq!(
        got.json::<Value>()["value"],
        json!({"disabled_sources": ["chutes"]})
    );

    let response = patch_as(&server, &token, path, json!({"disabled_sources": null})).await;
    assert_eq!(response.status_code(), StatusCode::OK);
    let got = get_as(&server, &token, path).await;
    assert_eq!(
        got.json::<Value>()["value"],
        json!({"disabled_sources": []})
    );
}

/// With NEAR failing and Chutes as its fallback, switching the `chutes` source
/// off stops traffic to it: the fallback is never invoked.
#[tokio::test]
async fn attested_3p_kill_switch_stops_serving_from_source() {
    let (server, pool, _mock, _database) = setup_test_server_with_pool().await;
    let (model_name, near, chutes) = crate::signature_verification::setup_chutes_signature_model(
        &server,
        &pool,
        Some(false),
        None,
    )
    .await;
    let org = setup_org_with_credits(&server, 10_000_000_000i64).await;
    let api_key = get_api_key_for_org(&server, org.id).await;
    let token = get_session_id();
    let path = "/v1/admin/settings/attested_3p";
    let chat = |max_tokens: i64| {
        server
            .post("/v1/chat/completions")
            .add_header("Authorization", format!("Bearer {api_key}"))
            .json(&json!({
                "model": model_name,
                "messages": [{"role": "user", "content": "Hello"}],
                "stream": false,
                "max_tokens": max_tokens
            }))
    };

    // Baseline: NEAR fails, so Chutes serves the request.
    let response = chat(16).await;
    assert_eq!(
        response.status_code(),
        StatusCode::OK,
        "{}",
        response.text()
    );
    assert_eq!(response.header("x-serving-provider"), "chutes");
    assert_eq!(
        chutes.last_chat_params().await.unwrap().max_tokens,
        Some(16)
    );

    // Kill switch on: Chutes is never invoked again.
    let patched = patch_as(
        &server,
        &token,
        path,
        json!({"disabled_sources": ["chutes"]}),
    )
    .await;
    assert_eq!(patched.status_code(), StatusCode::OK);
    let response = chat(17).await;
    let status = response.status_code();
    let body = response.text();
    let chutes_last = chutes.last_chat_params().await.unwrap().max_tokens;

    // Reset before asserting so a failure cannot leak the setting.
    let reset = patch_as(&server, &token, path, json!({"disabled_sources": null})).await;
    assert_eq!(reset.status_code(), StatusCode::OK);

    assert_eq!(
        chutes_last,
        Some(16),
        "a disabled source must not be invoked"
    );
    assert_eq!(near.last_chat_params().await.unwrap().max_tokens, Some(17));
    assert_eq!(status.as_u16(), 429, "{body}"); // NEAR 503 surfaces as service_overloaded
}
