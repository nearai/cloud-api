//! E2E tests for document reranking endpoint
//!
//! These tests can run in two modes:
//! - With mocks (default, for CI pipeline): `cargo test --test e2e_rerank`
//! - With real providers (for dev testing): `USE_REAL_PROVIDERS=true cargo test --test e2e_rerank`

mod common;

use api::models::ErrorResponse;
use common::*;

/// Test basic rerank functionality
#[tokio::test]
async fn test_rerank_basic() {
    let (server, guard) = setup_test_server().await;
    let _guard = guard;

    setup_rerank_model(&server).await;
    let org = setup_org_with_credits(&server, 10_000_000_000i64).await; // $10.00 USD
    let api_key = get_api_key_for_org(&server, org.id).await;

    let response = server
        .post("/v1/rerank")
        .add_header("Authorization", format!("Bearer {api_key}"))
        .add_header("User-Agent", MOCK_USER_AGENT)
        .json(&serde_json::json!({
            "model": "Qwen/Qwen3-Reranker-0.6B",
            "query": "What is the capital of France?",
            "documents": [
                "The capital of Brazil is Brasilia.",
                "The capital of France is Paris.",
                "Horses and cows are both animals"
            ]
        }))
        .await;

    assert_eq!(response.status_code(), 200, "Rerank should succeed");

    let response_json: serde_json::Value = response.json();

    // Verify response structure
    assert!(response_json.get("id").is_some(), "Response should have id");
    assert!(
        response_json.get("model").is_some(),
        "Response should have model"
    );
    assert!(
        response_json.get("results").is_some(),
        "Response should have results"
    );

    let results = response_json["results"].as_array().unwrap();
    assert_eq!(results.len(), 3, "Should have 3 results");

    // Verify results have required fields
    for result in results {
        assert!(result.get("index").is_some(), "Result should have index");
        assert!(
            result.get("relevance_score").is_some(),
            "Result should have relevance_score"
        );
        assert!(
            result.get("document").is_some(),
            "Result should have document by default"
        );
    }

    // Verify sorted by relevance_score descending
    let score_0 = results[0]["relevance_score"].as_f64().unwrap();
    let score_1 = results[1]["relevance_score"].as_f64().unwrap();
    let score_2 = results[2]["relevance_score"].as_f64().unwrap();
    assert!(score_0 >= score_1, "Results should be sorted descending");
    assert!(score_1 >= score_2, "Results should be sorted descending");
}

/// Test rerank with multiple documents
#[tokio::test]
async fn test_rerank_with_multiple_documents() {
    let (server, guard) = setup_test_server().await;
    let _guard = guard;

    setup_rerank_model(&server).await;
    let org = setup_org_with_credits(&server, 10_000_000_000i64).await;
    let api_key = get_api_key_for_org(&server, org.id).await;

    let response = server
        .post("/v1/rerank")
        .add_header("Authorization", format!("Bearer {api_key}"))
        .add_header("User-Agent", MOCK_USER_AGENT)
        .json(&serde_json::json!({
            "model": "Qwen/Qwen3-Reranker-0.6B",
            "query": "Test query",
            "documents": [
                "Document 1",
                "Document 2",
                "Document 3",
                "Document 4",
                "Document 5"
            ]
        }))
        .await;

    assert_eq!(response.status_code(), 200, "Rerank should succeed");

    let response_json: serde_json::Value = response.json();
    let results = response_json["results"].as_array().unwrap();

    assert_eq!(results.len(), 5, "Should return all 5 results");
}

/// Test that documents are included in results
#[tokio::test]
async fn test_rerank_documents_included() {
    let (server, guard) = setup_test_server().await;
    let _guard = guard;

    setup_rerank_model(&server).await;
    let org = setup_org_with_credits(&server, 10_000_000_000i64).await;
    let api_key = get_api_key_for_org(&server, org.id).await;

    let response = server
        .post("/v1/rerank")
        .add_header("Authorization", format!("Bearer {api_key}"))
        .add_header("User-Agent", MOCK_USER_AGENT)
        .json(&serde_json::json!({
            "model": "Qwen/Qwen3-Reranker-0.6B",
            "query": "Test query",
            "documents": [
                "Document 1",
                "Document 2",
                "Document 3"
            ]
        }))
        .await;

    assert_eq!(response.status_code(), 200, "Rerank should succeed");

    let response_json: serde_json::Value = response.json();
    let results = response_json["results"].as_array().unwrap();

    // Verify documents are included in results
    for result in results {
        assert!(
            result.get("document").is_some(),
            "Document should be included in results"
        );
        assert!(!result["document"].is_null(), "Document should not be null");
    }
}

/// Test validation: empty documents array
#[tokio::test]
async fn test_rerank_validation_empty_documents() {
    let (server, guard) = setup_test_server().await;
    let _guard = guard;

    setup_rerank_model(&server).await;
    let org = setup_org_with_credits(&server, 10_000_000_000i64).await;
    let api_key = get_api_key_for_org(&server, org.id).await;

    let response = server
        .post("/v1/rerank")
        .add_header("Authorization", format!("Bearer {api_key}"))
        .add_header("User-Agent", MOCK_USER_AGENT)
        .json(&serde_json::json!({
            "model": "Qwen/Qwen3-Reranker-0.6B",
            "query": "Test query",
            "documents": []
        }))
        .await;

    assert_eq!(
        response.status_code(),
        400,
        "Should reject empty documents array"
    );

    let error: ErrorResponse = response.json();
    assert!(
        error.error.message.contains("at least 1"),
        "Error message should mention minimum documents"
    );
}

/// Test validation: empty query
#[tokio::test]
async fn test_rerank_validation_empty_query() {
    let (server, guard) = setup_test_server().await;
    let _guard = guard;

    setup_rerank_model(&server).await;
    let org = setup_org_with_credits(&server, 10_000_000_000i64).await;
    let api_key = get_api_key_for_org(&server, org.id).await;

    let response = server
        .post("/v1/rerank")
        .add_header("Authorization", format!("Bearer {api_key}"))
        .add_header("User-Agent", MOCK_USER_AGENT)
        .json(&serde_json::json!({
            "model": "Qwen/Qwen3-Reranker-0.6B",
            "query": "",
            "documents": ["Document 1"]
        }))
        .await;

    assert_eq!(response.status_code(), 400, "Should reject empty query");
}

/// Test validation: document count exceeds limit (1001 documents)
#[tokio::test]
async fn test_rerank_validation_too_many_documents() {
    let (server, guard) = setup_test_server().await;
    let _guard = guard;

    setup_rerank_model(&server).await;
    let org = setup_org_with_credits(&server, 10_000_000_000i64).await;
    let api_key = get_api_key_for_org(&server, org.id).await;

    // Create 1001 documents
    let documents: Vec<String> = (0..1001).map(|i| format!("Document {}", i)).collect();

    let response = server
        .post("/v1/rerank")
        .add_header("Authorization", format!("Bearer {api_key}"))
        .add_header("User-Agent", MOCK_USER_AGENT)
        .json(&serde_json::json!({
            "model": "Qwen/Qwen3-Reranker-0.6B",
            "query": "Test query",
            "documents": documents
        }))
        .await;

    assert_eq!(
        response.status_code(),
        400,
        "Should reject more than 1000 documents"
    );

    let error: ErrorResponse = response.json();
    assert!(
        error.error.message.contains("at most 1000"),
        "Error message should mention max documents limit"
    );
}

/// Test validation: empty/whitespace-only documents
#[tokio::test]
async fn test_rerank_validation_empty_document_items() {
    let (server, guard) = setup_test_server().await;
    let _guard = guard;

    setup_rerank_model(&server).await;
    let org = setup_org_with_credits(&server, 10_000_000_000i64).await;
    let api_key = get_api_key_for_org(&server, org.id).await;

    let response = server
        .post("/v1/rerank")
        .add_header("Authorization", format!("Bearer {api_key}"))
        .add_header("User-Agent", MOCK_USER_AGENT)
        .json(&serde_json::json!({
            "model": "Qwen/Qwen3-Reranker-0.6B",
            "query": "Test query",
            "documents": ["Document 1", "", "Document 3"]
        }))
        .await;

    assert_eq!(
        response.status_code(),
        400,
        "Should reject documents with empty strings"
    );

    let error: ErrorResponse = response.json();
    assert!(
        error.error.message.contains("empty") || error.error.message.contains("index 1"),
        "Error message should mention empty document at index 1"
    );
}

/// Test validation: whitespace-only documents
#[tokio::test]
async fn test_rerank_validation_whitespace_only_document() {
    let (server, guard) = setup_test_server().await;
    let _guard = guard;

    setup_rerank_model(&server).await;
    let org = setup_org_with_credits(&server, 10_000_000_000i64).await;
    let api_key = get_api_key_for_org(&server, org.id).await;

    let response = server
        .post("/v1/rerank")
        .add_header("Authorization", format!("Bearer {api_key}"))
        .add_header("User-Agent", MOCK_USER_AGENT)
        .json(&serde_json::json!({
            "model": "Qwen/Qwen3-Reranker-0.6B",
            "query": "Test query",
            "documents": ["Document 1", "   ", "Document 3"]
        }))
        .await;

    assert_eq!(
        response.status_code(),
        400,
        "Should reject documents with only whitespace"
    );

    let error: ErrorResponse = response.json();
    assert!(
        error.error.message.contains("empty") || error.error.message.contains("whitespace"),
        "Error message should mention whitespace-only document"
    );
}

/// Test model not found
#[tokio::test]
async fn test_rerank_model_not_found() {
    let (server, guard) = setup_test_server().await;
    let _guard = guard;

    let org = setup_org_with_credits(&server, 10_000_000_000i64).await;
    let api_key = get_api_key_for_org(&server, org.id).await;

    let response = server
        .post("/v1/rerank")
        .add_header("Authorization", format!("Bearer {api_key}"))
        .add_header("User-Agent", MOCK_USER_AGENT)
        .json(&serde_json::json!({
            "model": "NonExistent/Model",
            "query": "Test query",
            "documents": ["Document 1"]
        }))
        .await;

    assert_eq!(
        response.status_code(),
        404,
        "Should return 404 for non-existent model"
    );

    let error: ErrorResponse = response.json();
    assert!(
        error.error.message.contains("not found"),
        "Error message should mention model not found"
    );
}

/// Test authentication: missing API key
#[tokio::test]
async fn test_rerank_missing_api_key() {
    let (server, guard) = setup_test_server().await;
    let _guard = guard;

    setup_rerank_model(&server).await;

    let response = server
        .post("/v1/rerank")
        .add_header("User-Agent", MOCK_USER_AGENT)
        .json(&serde_json::json!({
            "model": "Qwen/Qwen3-Reranker-0.6B",
            "query": "Test query",
            "documents": ["Document 1"]
        }))
        .await;

    assert_eq!(
        response.status_code(),
        401,
        "Should reject request without API key"
    );
}

/// Test authentication: invalid API key
#[tokio::test]
async fn test_rerank_invalid_api_key() {
    let (server, guard) = setup_test_server().await;
    let _guard = guard;

    setup_rerank_model(&server).await;

    let response = server
        .post("/v1/rerank")
        .add_header("Authorization", "Bearer invalid-api-key-12345")
        .add_header("User-Agent", MOCK_USER_AGENT)
        .json(&serde_json::json!({
            "model": "Qwen/Qwen3-Reranker-0.6B",
            "query": "Test query",
            "documents": ["Document 1"]
        }))
        .await;

    assert_eq!(
        response.status_code(),
        401,
        "Should reject request with invalid API key"
    );
}

/// Test with max documents (1000)
#[tokio::test]
async fn test_rerank_max_documents() {
    let (server, guard) = setup_test_server().await;
    let _guard = guard;

    setup_rerank_model(&server).await;
    let org = setup_org_with_credits(&server, 100_000_000_000i64).await; // $100 for many tokens
    let api_key = get_api_key_for_org(&server, org.id).await;

    // Create exactly 1000 documents
    let documents: Vec<String> = (0..1000).map(|i| format!("Document {}", i)).collect();

    let response = server
        .post("/v1/rerank")
        .add_header("Authorization", format!("Bearer {api_key}"))
        .add_header("User-Agent", MOCK_USER_AGENT)
        .json(&serde_json::json!({
            "model": "Qwen/Qwen3-Reranker-0.6B",
            "query": "Test query",
            "documents": documents
        }))
        .await;

    assert_eq!(
        response.status_code(),
        200,
        "Should accept exactly 1000 documents"
    );

    let response_json: serde_json::Value = response.json();
    let results = response_json["results"].as_array().unwrap();
    assert_eq!(results.len(), 1000, "Should return all 1000 results");
}

/// Test usage tracking
#[tokio::test]
async fn test_rerank_usage_tracking() {
    let (server, guard) = setup_test_server().await;
    let _guard = guard;

    setup_rerank_model(&server).await;
    let org = setup_org_with_credits(&server, 10_000_000_000i64).await; // $10.00 USD
    let api_key = get_api_key_for_org(&server, org.id).await;

    let response = server
        .post("/v1/rerank")
        .add_header("Authorization", format!("Bearer {api_key}"))
        .add_header("User-Agent", MOCK_USER_AGENT)
        .json(&serde_json::json!({
            "model": "Qwen/Qwen3-Reranker-0.6B",
            "query": "Test query",
            "documents": ["Document 1", "Document 2"]
        }))
        .await;

    assert_eq!(response.status_code(), 200, "Rerank should succeed");

    // Give usage tracking async task time to complete
    tokio::time::sleep(tokio::time::Duration::from_millis(500)).await;

    // Note: In a real test, we would query the database to verify usage was recorded
    // For now, we just verify the request succeeded
}

/// Test response structure matches spec
#[tokio::test]
async fn test_rerank_response_structure() {
    let (server, guard) = setup_test_server().await;
    let _guard = guard;

    setup_rerank_model(&server).await;
    let org = setup_org_with_credits(&server, 10_000_000_000i64).await;
    let api_key = get_api_key_for_org(&server, org.id).await;

    let response = server
        .post("/v1/rerank")
        .add_header("Authorization", format!("Bearer {api_key}"))
        .add_header("User-Agent", MOCK_USER_AGENT)
        .json(&serde_json::json!({
            "model": "Qwen/Qwen3-Reranker-0.6B",
            "query": "What is AI?",
            "documents": [
                "Artificial Intelligence is...",
                "Machine Learning is...",
                "AI is about...",
                "Neural networks..."
            ]
        }))
        .await;

    assert_eq!(response.status_code(), 200);

    let response_json: serde_json::Value = response.json();

    // Verify all required fields are present
    assert!(response_json["id"].is_string(), "id should be string");
    assert!(response_json["model"].is_string(), "model should be string");
    assert_eq!(
        response_json["model"].as_str().unwrap(),
        "Qwen/Qwen3-Reranker-0.6B",
        "model should match request"
    );

    let results = response_json["results"].as_array().unwrap();
    assert!(!results.is_empty(), "results should not be empty");

    // Check each result
    for (idx, result) in results.iter().enumerate() {
        assert!(
            result["index"].is_number(),
            "index should be number at position {}",
            idx
        );
        assert!(
            result["relevance_score"].is_number(),
            "relevance_score should be number at position {}",
            idx
        );

        let relevance_score = result["relevance_score"].as_f64().unwrap();
        assert!(
            (0.0..=1.0).contains(&relevance_score),
            "relevance_score should be between 0 and 1, got {} at position {}",
            relevance_score,
            idx
        );

        assert!(
            result["document"].is_string(),
            "document should be string (or object with string value) at position {}",
            idx
        );
    }

    // Check usage if present
    if let Some(usage) = response_json.get("usage") {
        assert!(
            usage["total_tokens"].is_null() || usage["total_tokens"].is_number(),
            "total_tokens should be null or number"
        );
    }
}
