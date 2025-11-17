// E2E tests for web search citation tracking
// Verifies that citations are properly parsed, indexed, and resolved to URLs
// Tests both streaming and non-streaming responses with web search enabled

mod common;

use common::*;
use serde_json::json;

/// Verify citation structure and validity
fn verify_citation_validity(annotation: &serde_json::Value, text: &str, citation_num: usize) {
    println!("\n--- Citation {} ---", citation_num + 1);

    // Check annotation type
    let annotation_type = annotation
        .get("type")
        .and_then(|t| t.as_str())
        .expect("Citation: Type field should be present");
    assert_eq!(
        annotation_type, "url_citation",
        "Citation should be of type 'url_citation', got '{}'",
        annotation_type
    );

    // Get indices
    let start_index = annotation
        .get("start_index")
        .and_then(|s| s.as_u64())
        .expect("Citation: start_index field should be present") as usize;

    let end_index = annotation
        .get("end_index")
        .and_then(|e| e.as_u64())
        .expect("Citation: end_index field should be present") as usize;

    // Get title and URL
    let title = annotation
        .get("title")
        .and_then(|t| t.as_str())
        .expect("Citation: title field should be present");

    let url = annotation
        .get("url")
        .and_then(|u| u.as_str())
        .expect("Citation: url field should be present");

    // Verify indices are valid
    assert!(
        start_index < end_index,
        "Citation #{}: start_index ({}) should be less than end_index ({})",
        citation_num + 1,
        start_index,
        end_index
    );

    assert!(
        end_index <= text.len(),
        "Citation #{}: end_index ({}) exceeds text length ({}). Text: '{}'",
        citation_num + 1,
        end_index,
        text.len(),
        text
    );

    // Extract cited text (convert character indices to actual characters, handling UTF-8 properly)
    let cited_text: String = text.chars()
        .skip(start_index)
        .take(end_index - start_index)
        .collect();

    println!("  Indices: [{}, {}]", start_index, end_index);
    println!("  Cited text: '{}'", cited_text);
    println!("  Title: {}", title);
    println!("  URL: {}", url);

    // Verify the cited text is not empty and meaningful
    assert!(
        !cited_text.trim().is_empty(),
        "Citation #{}: Cited text should not be empty",
        citation_num + 1
    );

    assert!(
        cited_text.len() > 2,
        "Citation #{}: Cited text '{}' is too short (must be > 2 characters)",
        citation_num + 1,
        cited_text
    );

    // Verify URL format
    assert!(
        url.starts_with("http://") || url.starts_with("https://"),
        "Citation #{}: URL '{}' should start with http:// or https://",
        citation_num + 1,
        url
    );

    assert!(
        url.len() > 10,
        "Citation #{}: URL '{}' appears to be too short",
        citation_num + 1,
        url
    );

    // Verify title is not empty and reasonable length
    assert!(
        !title.is_empty(),
        "Citation #{}: Title should not be empty",
        citation_num + 1
    );

    assert!(
        title.len() > 3,
        "Citation #{}: Title '{}' is too short",
        citation_num + 1,
        title
    );

    println!("  ✓ Citation format valid");
}

#[tokio::test]
async fn test_non_streaming_web_search_with_citations() {
    let server = setup_test_server().await;
    let org = setup_org_with_credits(&server, 10000000000i64).await;
    let api_key = get_api_key_for_org(&server, org.id).await;

    // Create a conversation
    let conversation_response = server
        .post("/v1/conversations")
        .add_header("Authorization", format!("Bearer {api_key}"))
        .json(&json!({
            "metadata": {
                "title": "Non-Streaming Web Search Citation Test"
            }
        }))
        .await;

    assert_eq!(conversation_response.status_code(), 201);

    let conversation_data = conversation_response.json::<serde_json::Value>();
    let conversation_id = conversation_data
        .get("id")
        .and_then(|v| v.as_str())
        .expect("Conversation ID should be present");

    println!("✓ Created conversation: {}", conversation_id);

    // Create non-streaming response with web search
    // Use a factual query that typically requires citations
    let response = server
        .post("/v1/responses")
        .add_header("Authorization", format!("Bearer {api_key}"))
        .json(&json!({
            "conversation": conversation_id,
            "model": "zai-org/GLM-4.6",
            "input": "What is the weather like in NY? Search the web for information.",
            "stream": false,
            "max_output_tokens": 512,
            "temperature": 0.7,
            "tools": [
                {
                    "type": "web_search"
                }
            ]
        }))
        .await;

    assert_eq!(response.status_code(), 200);

    let response_data = response.json::<serde_json::Value>();

    // Extract the final message
    let output = response_data
        .get("output")
        .and_then(|v| v.as_array())
        .expect("Output should be an array");

    let final_message = output
        .iter()
        .rev()
        .find(|item| {
            item.get("type")
                .and_then(|t| t.as_str())
                .map(|t| t == "message")
                .unwrap_or(false)
        })
        .expect("Should have at least one message");

    let content = final_message
        .get("content")
        .and_then(|c| c.as_array())
        .expect("Content should be an array");

    let output_text = content
        .iter()
        .find(|item| {
            item.get("type")
                .and_then(|t| t.as_str())
                .map(|t| t == "output_text")
                .unwrap_or(false)
        })
        .expect("Should have output_text");

    let text = output_text
        .get("text")
        .and_then(|t| t.as_str())
        .expect("Text should be present");

    let annotations = output_text
        .get("annotations")
        .and_then(|a| a.as_array())
        .expect("Annotations should be present");

    println!("\n=== Non-Streaming Response ===");
    println!("Text length: {} characters", text.len());
    println!("Text (first 300 chars): {}", &text[..text.len().min(300)]);

    println!("Annotations found: {}", annotations.len());
    
    // CRITICAL: Web search with citations MUST produce citations
    assert!(
        !annotations.is_empty(),
        "Web search response should include at least one citation with URL. Got {} citations",
        annotations.len()
    );
    
    println!("✓ Found {} citations in response", annotations.len());
    
    // Verify each citation has correct structure and valid indices
    for (idx, annotation) in annotations.iter().enumerate() {
        verify_citation_validity(annotation, text, idx);
    }

    // Verify that citation indices don't overlap
    let mut sorted_annotations: Vec<_> = annotations
        .iter()
        .enumerate()
        .map(|(i, a)| {
            let start = a.get("start_index").and_then(|s| s.as_u64()).unwrap() as usize;
            let end = a.get("end_index").and_then(|e| e.as_u64()).unwrap() as usize;
            (i, start, end)
        })
        .collect();

    sorted_annotations.sort_by_key(|a| a.1);

    println!("\n=== Citation Index Overlap Check ===");
    for window in sorted_annotations.windows(2) {
        let (idx1, start1, end1) = window[0];
        let (idx2, start2, end2) = window[1];

        println!(
            "Citation {} [{}-{}] vs Citation {} [{}-{}]",
            idx1, start1, end1, idx2, start2, end2
        );

        assert!(
            end1 <= start2,
            "Citations should not overlap: Citation {} ends at {} but Citation {} starts at {}",
            idx1,
            end1,
            idx2,
            start2
        );
    }

    println!("\n✅ Non-streaming test PASSED with {} citations verified", annotations.len());
}

#[tokio::test]
async fn test_streaming_web_search_with_citations() {
    let server = setup_test_server().await;
    let org = setup_org_with_credits(&server, 10000000000i64).await;
    let api_key = get_api_key_for_org(&server, org.id).await;

    // Create a conversation
    let conversation_response = server
        .post("/v1/conversations")
        .add_header("Authorization", format!("Bearer {api_key}"))
        .json(&json!({
            "metadata": {
                "title": "Streaming Web Search Citation Test"
            }
        }))
        .await;

    assert_eq!(conversation_response.status_code(), 201);

    let conversation_data = conversation_response.json::<serde_json::Value>();
    let conversation_id = conversation_data
        .get("id")
        .and_then(|v| v.as_str())
        .expect("Conversation ID should be present");

    println!("✓ Created conversation: {}", conversation_id);

    // Create streaming response with web search
    let response = server
        .post("/v1/responses")
        .add_header("Authorization", format!("Bearer {api_key}"))
        .json(&json!({
            "conversation": conversation_id,
            "model": "zai-org/GLM-4.6",
            "input": "What is the weather like in SF right now? Please search the web for current information.",
            "stream": true,
            "max_output_tokens": 512,
            "temperature": 0.7,
            "tools": [
                {
                    "type": "web_search"
                }
            ]
        }))
        .await;

    assert_eq!(response.status_code(), 200);

    let response_text = response.text();

    // Count streaming delta events to verify streaming is working
    let delta_count = response_text
        .lines()
        .filter(|l| l.contains("response.output_text.delta"))
        .count();

    println!("✓ Received {} streaming delta events", delta_count);

    assert!(
        delta_count > 5,
        "Should have multiple delta events (token-by-token streaming)"
    );

    // Extract the final message to check citations
    let final_line = response_text
        .lines()
        .filter(|l| l.contains("response.output_item.done"))
        .last()
        .and_then(|l| {
            l.strip_prefix("data: ")
                .and_then(|json_str| serde_json::from_str::<serde_json::Value>(json_str).ok())
        })
        .expect("Should find completion event");

    let output_text = final_line
        .get("item")
        .and_then(|item| item.get("content"))
        .and_then(|c| c.as_array())
        .and_then(|content| {
            content.iter().find(|item| {
                item.get("type")
                    .and_then(|t| t.as_str())
                    .map(|t| t == "output_text")
                    .unwrap_or(false)
            })
        })
        .expect("Should have output_text");

    let text = output_text
        .get("text")
        .and_then(|t| t.as_str())
        .expect("Text should be present");

    let annotations = output_text
        .get("annotations")
        .and_then(|a| a.as_array())
        .expect("Annotations should be present");

    println!("\n=== Streaming Response ===");
    println!("Text length: {} characters", text.len());
    println!("Text (first 300 chars): {}", &text[..text.len().min(300)]);

    println!("Annotations found in streaming: {}", annotations.len());
    
    // CRITICAL: Web search with citations MUST produce citations
    assert!(
        !annotations.is_empty(),
        "Streaming web search response should include at least one citation with URL. Got {} citations",
        annotations.len()
    );
    
    println!(
        "✓ Found {} citations in streaming response",
        annotations.len()
    );

    for (idx, annotation) in annotations.iter().enumerate() {
        verify_citation_validity(annotation, text, idx);
    }

    // Verify that citation indices don't overlap
    let mut sorted_annotations: Vec<_> = annotations
        .iter()
        .enumerate()
        .map(|(i, a)| {
            let start = a.get("start_index").and_then(|s| s.as_u64()).unwrap() as usize;
            let end = a.get("end_index").and_then(|e| e.as_u64()).unwrap() as usize;
            (i, start, end)
        })
        .collect();

    sorted_annotations.sort_by_key(|a| a.1);

    println!("\n=== Citation Index Overlap Check ===");
    for window in sorted_annotations.windows(2) {
        let (idx1, start1, end1) = window[0];
        let (idx2, start2, end2) = window[1];

        println!(
            "Citation {} [{}-{}] vs Citation {} [{}-{}]",
            idx1, start1, end1, idx2, start2, end2
        );

        assert!(
            end1 <= start2,
            "Citations should not overlap: Citation {} ends at {} but Citation {} starts at {}",
            idx1,
            end1,
            idx2,
            start2
        );
    }

    println!("\n✅ Streaming citation test PASSED with {} citations verified", annotations.len());
}
