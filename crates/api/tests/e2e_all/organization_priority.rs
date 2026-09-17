use crate::common::*;
use axum::http::{Method, StatusCode};
use serde_json::{json, Value};
use uuid::Uuid;

#[path = "organization_priority_local_proxy.rs"]
mod local_proxy;

async fn admin_call(
    server: &axum_test::TestServer,
    method: Method,
    org_id: &str,
    token: &str,
    body: Value,
) -> axum_test::TestResponse {
    server
        .method(
            method,
            &format!("/v1/admin/organizations/{org_id}/priority"),
        )
        .add_header("Authorization", format!("Bearer {token}"))
        .add_header("User-Agent", MOCK_USER_AGENT)
        .json(&body)
        .await
}

async fn set_priority(server: &axum_test::TestServer, org_id: &str, priority: i32) {
    let response = admin_call(
        server,
        Method::PATCH,
        org_id,
        &get_session_id(),
        json!({"priority": priority}),
    )
    .await;
    response.assert_status_ok();
    assert_eq!(
        response.json::<Value>(),
        json!({"organization_id": org_id, "priority": priority})
    );
}

#[tokio::test]
async fn priority_crud_validation_and_database_constraint() {
    let (server, db) = setup_test_server_with_database().await;
    let org = create_org(&server).await;
    let id = Uuid::parse_str(&org.id).unwrap();
    let session = get_session_id();
    let initial = admin_call(&server, Method::GET, &org.id, &session, json!({})).await;
    initial.assert_status_ok();
    assert_eq!(
        initial.json::<Value>(),
        json!({"organization_id": org.id, "priority": 0})
    );

    for priority in [-1000, -2, 1000, 0] {
        set_priority(&server, &org.id, priority).await;
        let read = admin_call(&server, Method::GET, &org.id, &session, json!({})).await;
        read.assert_status_ok();
        assert_eq!(read.json::<Value>()["priority"], priority);
        let client = db.pool().get().await.unwrap();
        let stored: i32 = client
            .query_one(
                "SELECT request_priority FROM organizations WHERE id = $1",
                &[&id],
            )
            .await
            .unwrap()
            .get(0);
        assert_eq!(stored, priority);
    }
    for value in [json!(-1001), json!(1001)] {
        let response = admin_call(
            &server,
            Method::PATCH,
            &org.id,
            &session,
            json!({"priority": value}),
        )
        .await;
        assert_eq!(response.status_code(), StatusCode::BAD_REQUEST);
    }
    for body in [
        json!({}),
        json!({"priority": null}),
        json!({"priority": "-2"}),
        json!({"priority": 1.5}),
    ] {
        let response = admin_call(&server, Method::PATCH, &org.id, &session, body).await;
        assert!(response.status_code().is_client_error());
    }
    let client = db.pool().get().await.unwrap();
    for priority in [-1001_i32, 1001] {
        let error = client
            .execute(
                "UPDATE organizations SET request_priority = $2 WHERE id = $1",
                &[&id, &priority],
            )
            .await
            .unwrap_err();
        assert_eq!(
            error.code(),
            Some(&tokio_postgres::error::SqlState::CHECK_VIOLATION)
        );
    }
    client
        .execute(
            "UPDATE organizations SET is_active = false WHERE id = $1",
            &[&id],
        )
        .await
        .unwrap();
    for org_id in [org.id, Uuid::new_v4().to_string()] {
        for method in [Method::GET, Method::PATCH] {
            let response =
                admin_call(&server, method, &org_id, &session, json!({"priority": -2})).await;
            assert_eq!(response.status_code(), StatusCode::NOT_FOUND);
        }
    }
}

#[tokio::test]
async fn priority_requires_platform_admin_and_respects_token_permissions() {
    let (server, db) = setup_test_server_with_config_and_database(|config| {
        config.auth.admin_read_only_tokens_enabled = true;
    })
    .await;
    let (owner_session, _) = setup_unique_test_session(&db).await;
    let owner_id = Uuid::parse_str(owner_session.trim_start_matches("rt_")).unwrap();
    let (org_admin_session, _) = setup_unique_test_session(&db).await;
    let org_admin_id = Uuid::parse_str(org_admin_session.trim_start_matches("rt_")).unwrap();
    // Real organization roles, but neither account is a platform administrator.
    let client = db.pool().get().await.unwrap();
    for user_id in [owner_id, org_admin_id] {
        client
            .execute(
                "UPDATE users SET email = $2 WHERE id = $1",
                &[&user_id, &format!("{user_id}@example.org")],
            )
            .await
            .unwrap();
    }
    drop(client);
    let org = create_org_with_session(&server, &owner_session).await;
    let client = db.pool().get().await.unwrap();
    client
        .execute(
            "INSERT INTO organization_members (organization_id, user_id, role, joined_at) \
             VALUES ($1, $2, 'admin', NOW())",
            &[&Uuid::parse_str(&org.id).unwrap(), &org_admin_id],
        )
        .await
        .unwrap();
    drop(client);
    let api_key = get_api_key_for_org_with_session(&server, org.id.clone(), &owner_session).await;
    // MockAuthService treats every session as a platform admin. Exercise the
    // negative authorization cases with real sessions and the real auth service.
    let (real_server, _) = setup_test_server_with_config_and_database(|config| {
        config.auth.mock = false;
    })
    .await;
    let sessions = database::repositories::SessionRepository::new(db.pool().clone());
    let mut non_admin_tokens = Vec::new();
    for user_id in [owner_id, org_admin_id] {
        let (_, refresh_token) = sessions
            .create(user_id, None, MOCK_USER_AGENT.to_string(), 1)
            .await
            .unwrap();
        non_admin_tokens
            .push(get_access_token_from_refresh_token(&real_server, refresh_token).await);
    }
    non_admin_tokens.push(api_key);
    for token in &non_admin_tokens {
        for method in [Method::GET, Method::PATCH] {
            let response = admin_call(
                &real_server,
                method,
                &org.id,
                token,
                json!({"priority": 1000}),
            )
            .await;
            assert!(matches!(
                response.status_code(),
                StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN
            ));
        }
    }
    server
        .get(&format!("/v1/admin/organizations/{}/priority", org.id))
        .await
        .assert_status_unauthorized();
    for permission in ["read_only", "read_write"] {
        let issued = server.post("/v1/admin/access-tokens")
            .add_header("Authorization", format!("Bearer {}", get_session_id()))
            .add_header("User-Agent", MOCK_USER_AGENT)
            .json(&json!({"name": "synthetic priority test", "reason": "test", "expires_in_hours": 1, "permission": permission})).await;
        issued.assert_status_ok();
        let token = issued
            .json::<api::models::AdminAccessTokenResponse>()
            .access_token;
        for method in [Method::GET, Method::HEAD] {
            admin_call(&server, method, &org.id, &token, json!({}))
                .await
                .assert_status_ok();
        }
        let updated = admin_call(
            &server,
            Method::PATCH,
            &org.id,
            &token,
            json!({"priority": -2}),
        )
        .await;
        assert_eq!(
            updated.status_code(),
            if permission == "read_only" {
                StatusCode::FORBIDDEN
            } else {
                StatusCode::OK
            }
        );
    }

    // General organization writes cannot change the independent policy column.
    for (method, suffix, body, expected) in [
        (
            Method::PUT,
            "",
            json!({"request_priority": 1000}),
            StatusCode::UNPROCESSABLE_ENTITY,
        ),
        (
            Method::PUT,
            "",
            json!({"settings": {"request_priority": 999, "priority": 999}}),
            StatusCode::OK,
        ),
        (
            Method::PATCH,
            "/settings",
            json!({"priority": 1000, "request_priority": 1000}),
            StatusCode::OK,
        ),
    ] {
        let response = server
            .method(method, &format!("/v1/organizations/{}{suffix}", org.id))
            .add_header("Authorization", format!("Bearer {owner_session}"))
            .json(&body)
            .await;
        assert_eq!(response.status_code(), expected);
    }
    for suffix in ["", "/settings"] {
        let response = server
            .get(&format!("/v1/organizations/{}{suffix}", org.id))
            .add_header("Authorization", format!("Bearer {owner_session}"))
            .await;
        response.assert_status_ok();
        assert!(response.json::<Value>().get("request_priority").is_none());
        assert!(response.json::<Value>().get("priority").is_none());
    }
    let stored = admin_call(&server, Method::GET, &org.id, &get_session_id(), json!({})).await;
    assert_eq!(stored.json::<Value>()["priority"], -2);
}

#[tokio::test]
async fn priority_flows_through_chat_text_responses_and_updates_without_cache() {
    let (server, _, mock, _) = setup_test_server_with_pool().await;
    let org = setup_org_with_credits(&server, 100_000_000_000).await;
    let key = get_api_key_for_org(&server, org.id.clone()).await;
    for priority in [0, -2, 0] {
        set_priority(&server, &org.id, priority).await;
        for endpoint in ["chat/completions", "completions", "responses"] {
            for stream in [false, true] {
                let mut body = json!({"model": E2E_QWEN_MODEL_NAME, "stream": stream,
                    "priority": 999, "request_priority": 999, "max_tokens": 32});
                match endpoint {
                    "chat/completions" => {
                        body["messages"] = json!([{"role": "user", "content": "synthetic test"}])
                    }
                    "completions" => body["prompt"] = json!("synthetic test"),
                    _ => {
                        body["input"] = json!("synthetic test");
                        body.as_object_mut().unwrap().remove("max_tokens");
                    }
                }
                let response = server
                    .post(&format!("/v1/{endpoint}"))
                    .add_header("Authorization", format!("Bearer {key}"))
                    .add_header("X-NearAI-Priority", "1000")
                    .json(&body)
                    .await;
                assert_eq!(
                    response.status_code(),
                    200,
                    "{endpoint}: {}",
                    response.text()
                );
                let params = mock.last_chat_params().await.unwrap();
                assert_eq!(params.request_priority, priority);
                assert!(!params.extra.contains_key("priority"));
                assert!(!params.extra.contains_key("request_priority"));
                if let Some(original) = &params.original_request {
                    assert!(original.get("priority").is_none());
                    assert!(original.get("request_priority").is_none());
                }
                if stream {
                    assert!(response.text().contains(if endpoint == "responses" {
                        "response.completed"
                    } else {
                        "[DONE]"
                    }));
                }
            }
        }
    }
    let other = setup_org_with_credits(&server, 100_000_000_000).await;
    let other_key = get_api_key_for_org(&server, other.id.clone()).await;
    set_priority(&server, &org.id, -2).await;
    let before = mock.chat_request_priorities().await.len();
    let body = json!({"model": E2E_QWEN_MODEL_NAME, "messages": [{"role":"user", "content":"synthetic concurrent test"}]});
    let first = server
        .post("/v1/chat/completions")
        .add_header("Authorization", format!("Bearer {key}"))
        .json(&body);
    let second = server
        .post("/v1/chat/completions")
        .add_header("Authorization", format!("Bearer {other_key}"))
        .json(&body);
    let (first, second) = tokio::join!(first, second);
    first.assert_status_ok();
    second.assert_status_ok();
    let mut seen = mock.chat_request_priorities().await[before..].to_vec();
    seen.sort();
    assert_eq!(seen, vec![-2, 0]);
}

#[tokio::test]
async fn priority_updates_bypass_warm_real_auth_caches_on_both_instances() {
    let setup = setup_test_server().await;
    let org = setup_org_with_credits(&setup, 100_000_000_000).await;
    let key = get_api_key_for_org(&setup, org.id.clone()).await;
    // Independent AuthService instances retain their own API-key caches.
    let (first, _, first_provider, _) = setup_test_server_with_pool_and_config(|config| {
        config.auth.mock = false;
    })
    .await;
    let (second, _, second_provider, _) = setup_test_server_with_pool_and_config(|config| {
        config.auth.mock = false;
    })
    .await;
    for priority in [0, -2, 1000, 0] {
        set_priority(&setup, &org.id, priority).await;
        for (server, provider) in [(&first, &first_provider), (&second, &second_provider)] {
            let response = server
                .post("/v1/chat/completions")
                .add_header("Authorization", format!("Bearer {key}"))
                .json(&json!({"model": E2E_QWEN_MODEL_NAME,
                    "messages": [{"role": "user", "content": "synthetic cache test"}]}))
                .await;
            response.assert_status_ok();
            assert_eq!(
                provider.last_chat_params().await.unwrap().request_priority,
                priority
            );
        }
    }
}

/// Pause a synthetic tool so an admin update can occur between generations.
#[derive(Default)]
struct PausedContextSearch {
    entered: tokio::sync::Notify,
    resume: tokio::sync::Notify,
}

#[async_trait::async_trait]
impl services::responses::tools::WebContextSearchProviderTrait for PausedContextSearch {
    async fn search_context(
        &self,
        _: services::responses::tools::WebContextSearchParams,
    ) -> Result<
        Vec<services::responses::tools::WebSearchResult>,
        services::responses::tools::WebSearchError,
    > {
        self.entered.notify_one();
        self.resume.notified().await;
        Ok(vec![services::responses::tools::WebSearchResult {
            title: "Synthetic source".into(),
            url: "https://example.com/test".into(),
            snippet: "Synthetic context".into(),
        }])
    }
}

#[tokio::test]
async fn priority_is_preserved_for_tool_iterations_and_background_title() {
    use inference_providers::mock::{RequestMatcher, ResponseTemplate, ToolCall};
    use std::sync::Arc;
    let tool = Arc::new(PausedContextSearch::default());
    let (server, _, mock) = setup_test_server_with_search_providers(
        Arc::new(MockWebSearchProvider::default_results()),
        Some(tool.clone()),
    )
    .await;
    let org = setup_org_with_credits(&server, 100_000_000_000).await;
    let key = get_api_key_for_org(&server, org.id.clone()).await;
    set_priority(&server, &org.id, -2).await;
    let created = server
        .post("/v1/conversations")
        .add_header("Authorization", format!("Bearer {key}"))
        .json(&json!({}))
        .await;
    assert_eq!(created.status_code(), StatusCode::CREATED);
    let conversation = created.json::<api::models::ConversationObject>();
    let prompt = "Use context search for a synthetic test.";
    mock.when(RequestMatcher::PromptWithTools {
        prompt: mock_prompts::build_prompt(prompt),
        tool_names: vec!["web_context_search".into()],
    })
    .respond_with(
        ResponseTemplate::new("").with_tool_calls(vec![ToolCall::new(
            "web_context_search",
            json!({"query": "synthetic test"}).to_string(),
        )]),
    )
    .await;
    mock.set_default_response(ResponseTemplate::new("Synthetic result"))
        .await;
    let response = server
        .post("/v1/responses")
        .add_header("Authorization", format!("Bearer {key}"))
        .json(
            &json!({"model": E2E_QWEN_MODEL_NAME, "input": prompt, "stream": false,
            "conversation": {"id": conversation.id}, "tools": [{"type": "web_context_search"}]}),
        );
    let update_during_tool = async {
        tokio::time::timeout(std::time::Duration::from_secs(10), tool.entered.notified())
            .await
            .expect("web_context_search tool was not invoked within 10s");
        set_priority(&server, &org.id, 0).await;
        tool.resume.notify_one();
    };
    let (response, ()) = tokio::join!(response, update_during_tool);
    assert_eq!(response.status_code(), 200, "{}", response.text());
    let body = response.json::<Value>();
    assert_eq!(body["status"], "completed");
    assert!(body["output"]
        .as_array()
        .unwrap()
        .iter()
        .any(|item| item["type"] == "web_search_call" && item["status"] == "completed"));
    let seen = mock.chat_request_priorities().await;
    assert!(
        seen.len() >= 3,
        "initial generation, tool follow-up, and title must all run: {seen:?}"
    );
    assert!(seen.iter().all(|priority| *priority == -2));
    let conversation = server
        .get(&format!("/v1/conversations/{}", conversation.id))
        .add_header("Authorization", format!("Bearer {key}"))
        .await;
    conversation.assert_status_ok();
    assert_eq!(
        conversation.json::<Value>()["metadata"]["title"],
        "Synthetic result"
    );
}
