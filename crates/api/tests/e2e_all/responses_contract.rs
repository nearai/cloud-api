use crate::common::*;
use api::models::{ResponseOutputContent, ResponseOutputItem};
use uuid::Uuid;

#[tokio::test]
async fn test_responses_api_usage_limit_enforcement() {
    let (server, database) = setup_test_server_with_database().await;
    let org = setup_org_with_credits(&server, 1).await; // 1 nano-dollar (minimal)
    println!("Created organization: {org:?}");
    let api_key = get_api_key_for_org(&server, org.id.clone()).await;
    let model_name = setup_qwen_model(&server).await;

    // The first request consumes the test-owned organization's one nano-dollar.
    let response1 = server
        .post("/v1/responses")
        .add_header("Authorization", format!("Bearer {api_key}"))
        .json(&serde_json::json!({
            "input": "Hi",
            "model": model_name,
            "stream": false,
            "max_output_tokens": 10
        }))
        .await;

    println!("First request status: {}", response1.status_code());
    assert_eq!(response1.status_code(), 200, "{}", response1.text());
    wait_for_recorded_spend(&database, org.id.parse().unwrap()).await;

    // Second request should fail with payment required
    let response2 = server
        .post("/v1/responses")
        .add_header("Authorization", format!("Bearer {api_key}"))
        .json(&serde_json::json!({
            "input": "Hi again",
            "model": model_name,
            "stream": false,
            "max_output_tokens": 10
        }))
        .await;

    println!("Second request status: {}", response2.status_code());
    println!("Second request body: {}", response2.text());

    // Should get 402 Payment Required after exceeding limit
    assert!(
        response2.status_code() == 402,
        "Expected 402 Payment Required, got: {}",
        response2.status_code()
    );

    // Since we got 402, verify the error message
    let error_response = response2.json::<api::models::ErrorResponse>();
    assert!(
        error_response
            .error
            .message
            .contains("Credit limit exceeded."),
        "Error response should indicate no credits, got: {}",
        error_response.error.message
    );
}

#[tokio::test]
async fn test_responses_api_no_credits() {
    let server = setup_test_server().await;
    // Create org without credits (no limit set)
    let org = create_org(&server).await;
    let api_key = get_api_key_for_org(&server, org.id).await;
    let model_name = setup_qwen_model(&server).await;

    // Request should fail with payment required (no credits)
    let response = server
        .post("/v1/responses")
        .add_header("Authorization", format!("Bearer {api_key}"))
        .json(&serde_json::json!({
            "input": "Hi",
            "model": model_name,
            "stream": false,
            "max_output_tokens": 10
        }))
        .await;

    println!("Response status: {}", response.status_code());
    println!("Response body: {}", response.text());

    assert_eq!(
        response.status_code(),
        402,
        "Expected 402 Payment Required when no credits are available"
    );

    let error_response = response.json::<api::models::ErrorResponse>();
    assert!(
        error_response
            .error
            .message
            .contains("No spending limit configured"),
        "Error response should indicate no credits, got: {}",
        error_response.error.message
    );
}

#[tokio::test]
async fn test_responses_api_zero_credits() {
    let server = setup_test_server().await;
    // Create org with zero credits
    let org = setup_org_with_credits(&server, 0).await;
    let api_key = get_api_key_for_org(&server, org.id).await;
    let model_name = setup_qwen_model(&server).await;

    // Request should fail with payment required (zero credits)
    let response = server
        .post("/v1/responses")
        .add_header("Authorization", format!("Bearer {api_key}"))
        .json(&serde_json::json!({
            "input": "Hi",
            "model": model_name,
            "stream": false,
            "max_output_tokens": 10
        }))
        .await;

    println!("Response status: {}", response.status_code());
    println!("Response body: {}", response.text());

    assert_eq!(
        response.status_code(),
        402,
        "Expected 402 Payment Required when credits are zero"
    );

    let error_response = response.json::<api::models::ErrorResponse>();
    assert!(
        error_response
            .error
            .message
            .contains("Credit limit exceeded."),
        "Error response should indicate no credits, got: {}",
        error_response.error.message
    );
}

#[tokio::test]
async fn test_responses_api_sufficient_credits() {
    let server = setup_test_server().await;
    // Create org with sufficient credits
    let org = setup_org_with_credits(&server, 10000000000i64).await; // $10.00 USD
    let api_key = get_api_key_for_org(&server, org.id).await;
    let model_name = setup_qwen_model(&server).await;

    // Request should succeed with sufficient credits
    let response = server
        .post("/v1/responses")
        .add_header("Authorization", format!("Bearer {api_key}"))
        .json(&serde_json::json!({
            "input": "Say hello in exactly 5 words.",
            "model": model_name,
            "stream": false,
            "max_output_tokens": 50
        }))
        .await;

    println!("Response status: {}", response.status_code());
    assert_eq!(
        response.status_code(),
        200,
        "Expected 200 OK when sufficient credits are available"
    );

    let response_obj = response.json::<api::models::ResponseObject>();
    assert_eq!(
        response_obj.status,
        api::models::ResponseStatus::Completed,
        "Response should complete successfully"
    );
}

#[tokio::test]
async fn test_responses_api_streaming_usage_limit() {
    let (server, database) = setup_test_server_with_database().await;
    let org = setup_org_with_credits(&server, 1).await; // 1 nano-dollar (minimal)
    println!("Created organization: {org:?}");
    let api_key = get_api_key_for_org(&server, org.id.clone()).await;
    let model_name = setup_qwen_model(&server).await;

    // The first stream consumes the test-owned organization's one nano-dollar.
    let response1 = server
        .post("/v1/responses")
        .add_header("Authorization", format!("Bearer {api_key}"))
        .json(&serde_json::json!({
            "input": "Hi",
            "model": model_name,
            "stream": true,
            "max_output_tokens": 10
        }))
        .await;

    println!(
        "First streaming request status: {}",
        response1.status_code()
    );
    assert_eq!(response1.status_code(), 200, "{}", response1.text());
    wait_for_recorded_spend(&database, org.id.parse().unwrap()).await;

    // Second streaming request should fail with payment required
    let response2 = server
        .post("/v1/responses")
        .add_header("Authorization", format!("Bearer {api_key}"))
        .json(&serde_json::json!({
            "input": "Hi again",
            "model": model_name,
            "stream": true,
            "max_output_tokens": 10
        }))
        .await;

    println!(
        "Second streaming request status: {}",
        response2.status_code()
    );
    println!("Second streaming request body: {}", response2.text());

    // Should get 402 Payment Required after exceeding limit
    assert!(
        response2.status_code() == 402,
        "Expected 402 Payment Required, got: {}",
        response2.status_code()
    );

    // If we got 402, verify the error message
    let error_response = response2.json::<api::models::ErrorResponse>();
    assert!(
        error_response
            .error
            .message
            .contains("Credit limit exceeded."),
        "Error response should indicate no credits, got: {}",
        error_response.error.message
    );
}

#[tokio::test]
async fn test_chat_completions_with_json_schema() {
    use crate::common::mock_prompts;
    use inference_providers::mock::{RequestMatcher, ResponseTemplate};

    let (server, _pool, mock, _db) = setup_test_server_with_pool().await;
    setup_qwen_model(&server).await;
    let org = setup_org_with_credits(&server, 10000000000i64).await;
    let api_key = get_api_key_for_org(&server, org.id).await;

    // Configure mock to match exact prompt and return structured JSON
    // Chat completions API sends messages directly without language instruction
    let user_message = "Generate a user profile";
    let expected_prompt = mock_prompts::build_simple_prompt(user_message);
    let expected_json = r#"{"name": "Alice Johnson", "age": 28, "email": "alice@example.com"}"#;

    mock.when(RequestMatcher::ExactPrompt(expected_prompt))
        .respond_with(ResponseTemplate::new(expected_json))
        .await;

    // Make a chat completion request with response_format
    let response = server
        .post("/v1/chat/completions")
        .add_header("Authorization", format!("Bearer {api_key}"))
        .json(&serde_json::json!({
            "model": "Qwen/Qwen3-30B-A3B-Instruct-2507",
            "messages": [
                {
                    "role": "user",
                    "content": user_message
                }
            ],
            "response_format": {
                "type": "json_schema",
                "json_schema": {
                    "name": "user_profile",
                    "schema": {
                        "type": "object",
                        "properties": {
                            "name": {"type": "string"},
                            "age": {"type": "integer"},
                            "email": {"type": "string"}
                        },
                        "required": ["name", "age", "email"]
                    },
                    "strict": true
                }
            }
        }))
        .await;

    if response.status_code() != 200 {
        let error = response.text();
        println!("Error response: {}", error);
    }
    assert_eq!(response.status_code(), 200);
    let completion = response.json::<serde_json::Value>();

    // Verify the response contains the expected JSON
    let content = completion["choices"][0]["message"]["content"]
        .as_str()
        .expect("Expected content in response");

    assert_eq!(content, expected_json);

    // Verify it's valid JSON matching the schema
    let json_obj: serde_json::Value =
        serde_json::from_str(content).expect("Content should be valid JSON");
    assert_eq!(json_obj["name"], "Alice Johnson");
    assert_eq!(json_obj["age"], 28);
    assert_eq!(json_obj["email"], "alice@example.com");

    println!("✅ Chat completions with JSON schema returned structured output");
}

#[tokio::test]
async fn test_responses_api_with_json_schema() {
    use crate::common::mock_prompts;
    use inference_providers::mock::{RequestMatcher, ResponseTemplate};

    let (server, _pool, mock, _db) = setup_test_server_with_pool().await;
    setup_qwen_model(&server).await;
    let org = setup_org_with_credits(&server, 10000000000i64).await;
    let api_key = get_api_key_for_org(&server, org.id).await;

    // Configure mock to match exact prompt and return structured JSON
    let user_message = "Generate a book description";
    let expected_prompt = mock_prompts::build_prompt(user_message);
    let expected_json = r#"{"title": "The Great Adventure", "author": "John Smith", "year": 2024, "genre": "Fiction"}"#;

    mock.when(RequestMatcher::ExactPrompt(expected_prompt))
        .respond_with(ResponseTemplate::new(expected_json))
        .await;

    // Make a response request with text.format.json_schema
    let response = server
        .post("/v1/responses")
        .add_header("Authorization", format!("Bearer {api_key}"))
        .json(&serde_json::json!({
            "input": user_message,
            "model": "Qwen/Qwen3-30B-A3B-Instruct-2507",
            "max_output_tokens": 100,
            "stream": false,
            "text": {
                "format": {
                    "type": "json_schema",
                    "json_schema": {
                        "name": "book",
                        "schema": {
                            "type": "object",
                            "properties": {
                                "title": {"type": "string"},
                                "author": {"type": "string"},
                                "year": {"type": "integer"},
                                "genre": {"type": "string"}
                            },
                            "required": ["title", "author", "year", "genre"]
                        },
                        "strict": true
                    }
                }
            }
        }))
        .await;

    assert_eq!(response.status_code(), 200);
    let response_obj = response.json::<api::models::ResponseObject>();

    // Verify the response contains structured output
    assert!(!response_obj.output.is_empty());

    if let ResponseOutputItem::Message { content, .. } = &response_obj.output[0] {
        assert!(!content.is_empty());

        if let ResponseOutputContent::OutputText { text, .. } = &content[0] {
            // Verify it's valid JSON matching the schema
            let json_obj: serde_json::Value =
                serde_json::from_str(text).expect("Content should be valid JSON");
            assert_eq!(json_obj["title"], "The Great Adventure");
            assert_eq!(json_obj["author"], "John Smith");
            assert_eq!(json_obj["year"], 2024);
            assert_eq!(json_obj["genre"], "Fiction");

            println!("✅ Responses API with JSON schema returned structured output");
        } else {
            panic!("Expected OutputText content");
        }
    } else {
        panic!("Expected Message output item");
    }
}

/// Synchronize on the actual debit before testing the next request's rejection.
async fn wait_for_recorded_spend(database: &std::sync::Arc<database::Database>, org: Uuid) {
    use services::usage::ports::UsageRepository;

    let usage = database::repositories::OrganizationUsageRepository::new(database.pool().clone());
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(10);
    loop {
        let balance = UsageRepository::get_balance(&usage, org).await.unwrap();
        if balance.is_some_and(|balance| balance.total_spent >= 1) {
            return;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "response usage was not recorded"
        );
        tokio::time::sleep(std::time::Duration::from_millis(25)).await;
    }
}
