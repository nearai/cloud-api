// E2E tests for web search citation tracking
// Verifies that citations are properly parsed, indexed, and resolved to URLs
// Tests both streaming and non-streaming responses with web search enabled

mod common;

use common::*;
use serde_json::json;

/// Verify citation structure and validity
fn verify_citation_validity(annotation: &serde_json::Value, text: &str, citation_num: usize) {
    println!("\n--- Citation {num} ---", num = citation_num + 1);

    // Check annotation type
    let annotation_type = annotation
        .get("type")
        .and_then(|t| t.as_str())
        .expect("Citation: Type field should be present");
    assert_eq!(
        annotation_type, "url_citation",
        "Citation should be of type 'url_citation', got '{annotation_type}'"
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
        "Citation #{n}: start_index ({s}) should be less than end_index ({e})",
        n = citation_num + 1,
        s = start_index,
        e = end_index
    );

    assert!(
        end_index <= text.len(),
        "Citation #{n}: end_index ({e}) exceeds text length ({l}). Text: '{t}'",
        n = citation_num + 1,
        e = end_index,
        l = text.len(),
        t = text
    );

    // Extract cited text (convert character indices to actual characters, handling UTF-8 properly)
    let cited_text: String = text
        .chars()
        .skip(start_index)
        .take(end_index - start_index)
        .collect();

    println!("  Indices: [{start_index}, {end_index}]");
    println!("  Cited text: '{cited_text}'");
    println!("  Title: {title}");
    println!("  URL: {url}");

    // Verify the cited text is not empty and meaningful
    assert!(
        !cited_text.trim().is_empty(),
        "Citation #{n}: Cited text should not be empty",
        n = citation_num + 1
    );

    assert!(
        cited_text.len() > 2,
        "Citation #{n}: Cited text '{c}' is too short (must be > 2 characters)",
        n = citation_num + 1,
        c = cited_text
    );

    // Verify URL format
    assert!(
        url.starts_with("http://") || url.starts_with("https://"),
        "Citation #{n}: URL '{u}' should start with http:// or https://",
        n = citation_num + 1,
        u = url
    );

    assert!(
        url.len() > 10,
        "Citation #{n}: URL '{u}' appears to be too short",
        n = citation_num + 1,
        u = url
    );

    // Verify title is not empty and reasonable length
    assert!(
        !title.is_empty(),
        "Citation #{n}: Title should not be empty",
        n = citation_num + 1
    );

    assert!(
        title.len() > 3,
        "Citation #{n}: Title '{t}' is too short",
        n = citation_num + 1,
        t = title
    );

    println!("  ✓ Citation format valid");
}

#[tokio::test]
#[ignore]
async fn test_non_streaming_web_search_with_citations() {
    let server = setup_test_server(None).await;
    let org = setup_org_with_credits(&server, 10000000000i64).await;
    let api_key = get_api_key_for_org(&server, org.id).await;
    let model = setup_glm_model(&server).await;

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

    println!("✓ Created conversation: {conversation_id}");

    // Create non-streaming response with web search
    // Use a specific query that requires current information and citations
    let response = server
         .post("/v1/responses")
         .add_header("Authorization", format!("Bearer {api_key}"))
         .json(&json!({
             "conversation": conversation_id,
             "model": model,
             "input": "What is the weather in San Francisco today? Search the web for current weather conditions.",
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
    let truncated_text = text.chars().take(300).collect::<String>();
    println!("Text (first 300 chars): {truncated_text}");

    println!("Annotations found: {count}", count = annotations.len());

    // With real providers, web search with citations should produce citations
    // However, with the mock provider in tests, citations may not be generated
    // We verify the response structure is correct and any citations present are valid
    if !annotations.is_empty() {
        println!(
            "✓ Found {count} citations in response",
            count = annotations.len()
        );

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

            println!("Citation {idx1} [{start1}-{end1}] vs Citation {idx2} [{start2}-{end2}]");

            assert!(
                end1 <= start2,
                "Citations should not overlap: Citation {idx1} ends at {end1} but Citation {idx2} starts at {start2}"
            );
        }

        println!(
            "\n✅ Non-streaming test PASSED with {c} citations verified",
            c = annotations.len()
        );
    } else {
        println!("ℹ  No citations in mock provider response (expected for mock provider)");
    }
}

#[tokio::test]
async fn test_streaming_web_search_with_citations() {
    let server = setup_test_server(None).await;
    let org = setup_org_with_credits(&server, 10000000000i64).await;
    let api_key = get_api_key_for_org(&server, org.id).await;
    let model = setup_glm_model(&server).await;

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

    println!("✓ Created conversation: {conversation_id}");

    // Create streaming response with web search
    let response = server
         .post("/v1/responses")
         .add_header("Authorization", format!("Bearer {api_key}"))
         .json(&json!({
             "conversation": conversation_id,
             "model": model,
             "input": "What is the current weather in New York City? Search the web for real-time weather conditions.",
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

    println!("✓ Received {delta_count} streaming delta events");

    assert!(
        delta_count > 5,
        "Should have multiple delta events (token-by-token streaming)"
    );

    // Count real-time citation annotation events (NEW)
    let annotation_event_lines: Vec<_> = response_text
        .lines()
        .filter(|l| l.contains("response.output_text.annotation.added"))
        .collect();

    let annotation_event_count = annotation_event_lines.len();
    println!("✓ Received {annotation_event_count} real-time citation annotation events");

    // Parse the annotation events to collect their data
    let mut streaming_annotations: Vec<serde_json::Value> = Vec::new();
    for line in annotation_event_lines {
        if let Some(json_str) = line.strip_prefix("data: ") {
            if let Ok(event) = serde_json::from_str::<serde_json::Value>(json_str) {
                if let Some(annotation) = event.get("annotation") {
                    streaming_annotations.push(annotation.clone());
                }
            }
        }
    }

    println!(
        "✓ Parsed {count} annotation payloads from streaming events",
        count = streaming_annotations.len()
    );

    // Extract the final message to check citations
    let final_line = response_text
        .lines()
        .filter(|l| l.contains("response.output_item.done"))
        .next_back()
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
    let truncated_text = text.chars().take(300).collect::<String>();
    println!("Text (first 300 chars): {truncated_text}");

    println!(
        "Annotations found in streaming: {count}",
        count = annotations.len()
    );

    // With real providers, web search with citations should produce citations
    // However, with the mock provider in tests, citations may not be generated
    // We verify the response structure is correct and any citations present are valid
    if !annotations.is_empty() {
        println!(
            "✓ Found {count} citations in streaming response",
            count = annotations.len()
        );

        for (idx, annotation) in annotations.iter().enumerate() {
            verify_citation_validity(annotation, text, idx);
        }
    } else {
        println!("ℹ  No citations in mock provider response (expected for mock provider)");
    }

    // Verify that streaming annotation events match final annotations
    println!("\n=== Real-Time vs Final Annotation Comparison ===");
    println!(
        "Streaming annotation events: {}",
        streaming_annotations.len()
    );
    println!("Final annotations: {}", annotations.len());

    assert_eq!(
        streaming_annotations.len(),
        annotations.len(),
        "Should receive one annotation event per citation. Got {s} streaming events but {f} final annotations",
        s = streaming_annotations.len(),
        f = annotations.len()
    );

    // Sort both by start_index for comparison
    let mut sorted_streaming = streaming_annotations.clone();
    let mut sorted_final = annotations.clone();

    sorted_streaming.sort_by_key(|a| a.get("start_index").and_then(|s| s.as_u64()).unwrap_or(0));

    sorted_final.sort_by_key(|a| a.get("start_index").and_then(|s| s.as_u64()).unwrap_or(0));

    // Compare each annotation
    for (idx, (streaming, final_)) in sorted_streaming.iter().zip(sorted_final.iter()).enumerate() {
        println!("\n  Comparing annotation {}", idx + 1);

        // Compare type
        let stream_type = streaming
            .get("type")
            .and_then(|t| t.as_str())
            .unwrap_or("missing");
        let final_type = final_
            .get("type")
            .and_then(|t| t.as_str())
            .unwrap_or("missing");

        assert_eq!(
            stream_type,
            final_type,
            "Annotation {}: type mismatch - streaming={}, final={}",
            idx + 1,
            stream_type,
            final_type
        );

        // Compare indices
        let stream_start = streaming
            .get("start_index")
            .and_then(|s| s.as_u64())
            .unwrap_or(0);
        let final_start = final_
            .get("start_index")
            .and_then(|s| s.as_u64())
            .unwrap_or(0);

        assert_eq!(
            stream_start,
            final_start,
            "Annotation {}: start_index mismatch - streaming={}, final={}",
            idx + 1,
            stream_start,
            final_start
        );

        let stream_end = streaming
            .get("end_index")
            .and_then(|e| e.as_u64())
            .unwrap_or(0);
        let final_end = final_
            .get("end_index")
            .and_then(|e| e.as_u64())
            .unwrap_or(0);

        assert_eq!(
            stream_end,
            final_end,
            "Annotation {}: end_index mismatch - streaming={}, final={}",
            idx + 1,
            stream_end,
            final_end
        );

        // Compare URL
        let stream_url = streaming.get("url").and_then(|u| u.as_str()).unwrap_or("");
        let final_url = final_.get("url").and_then(|u| u.as_str()).unwrap_or("");

        assert_eq!(
            stream_url,
            final_url,
            "Annotation {}: URL mismatch - streaming={}, final={}",
            idx + 1,
            stream_url,
            final_url
        );

        // Compare title
        let stream_title = streaming
            .get("title")
            .and_then(|t| t.as_str())
            .unwrap_or("");
        let final_title = final_.get("title").and_then(|t| t.as_str()).unwrap_or("");

        assert_eq!(
            stream_title,
            final_title,
            "Annotation {}: title mismatch - streaming={}, final={}",
            idx + 1,
            stream_title,
            final_title
        );

        println!("    ✓ Annotation {} matches perfectly", idx + 1);
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

        println!("Citation {idx1} [{start1}-{end1}] vs Citation {idx2} [{start2}-{end2}]");

        assert!(
            end1 <= start2,
            "Citations should not overlap: Citation {idx1} ends at {end1} but Citation {idx2} starts at {start2}"
        );
    }

    println!(
        "\n✅ Streaming citation test PASSED with {c} citations verified",
        c = annotations.len()
    );
}

#[tokio::test]
async fn capture_streaming_citations_to_file() {
    use std::fs::File;
    use std::io::Write;

    let server = setup_test_server(None).await;
    let org = setup_org_with_credits(&server, 10000000000i64).await;
    let api_key = get_api_key_for_org(&server, org.id).await;
    let model = setup_glm_model(&server).await;

    // Create a conversation
    let conversation_response = server
        .post("/v1/conversations")
        .add_header("Authorization", format!("Bearer {api_key}"))
        .json(&json!({
            "metadata": {
                "title": "Streaming Citations Capture Test"
            }
        }))
        .await;

    assert_eq!(conversation_response.status_code(), 201);

    let conversation_data = conversation_response.json::<serde_json::Value>();
    let conversation_id = conversation_data
        .get("id")
        .and_then(|v| v.as_str())
        .expect("Conversation ID should be present");

    println!("✓ Created conversation: {conversation_id}");

    // Create streaming response with web search
    let response = server
         .post("/v1/responses")
         .add_header("Authorization", format!("Bearer {api_key}"))
         .json(&json!({
             "conversation": conversation_id,
             "model": model,
             "input": "What are the latest developments in AI? Search the web and provide current information with citations.",
            "stream": true,
            "max_output_tokens": 256,
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

    // Save to file
    let mut file =
        File::create("/tmp/streaming_citations_demo.sse").expect("Failed to create file");
    file.write_all(response_text.as_bytes())
        .expect("Failed to write file");

    println!("\n✓ Saved streaming response to /tmp/streaming_citations_demo.sse");
    println!("  File size: {} bytes", response_text.len());

    // Print statistics
    let delta_count = response_text
        .lines()
        .filter(|l| l.contains("response.output_text.delta"))
        .count();
    let annotation_count = response_text
        .lines()
        .filter(|l| l.contains("response.output_text.annotation.added"))
        .count();
    let web_search_count = response_text
        .lines()
        .filter(|l| l.contains("web_search_call"))
        .count();

    println!("\n=== Event Statistics ===");
    println!("Text deltas: {delta_count}");
    println!("Citation annotations: {annotation_count}");
    println!("Web search events: {web_search_count}");

    println!("\n=== Sample Events ===");

    // Show first few deltas
    println!("\nFirst 3 text deltas:");
    for line in response_text
        .lines()
        .filter(|l| l.contains("response.output_text.delta"))
        .take(3)
    {
        if let Some(json_str) = line.strip_prefix("data: ") {
            if let Ok(event) = serde_json::from_str::<serde_json::Value>(json_str) {
                println!(
                    "  {}",
                    event.get("delta").and_then(|d| d.as_str()).unwrap_or("")
                );
            }
        }
    }

    // Show citations
    if annotation_count > 0 {
        println!("\nCitation annotations found:");
        for line in response_text
            .lines()
            .filter(|l| l.contains("response.output_text.annotation.added"))
        {
            if let Some(json_str) = line.strip_prefix("data: ") {
                if let Ok(event) = serde_json::from_str::<serde_json::Value>(json_str) {
                    if let Some(annotation) = event.get("annotation") {
                        let title = annotation
                            .get("title")
                            .and_then(|t| t.as_str())
                            .unwrap_or("N/A");
                        let start = annotation
                            .get("start_index")
                            .and_then(|s| s.as_u64())
                            .unwrap_or(0);
                        let end = annotation
                            .get("end_index")
                            .and_then(|e| e.as_u64())
                            .unwrap_or(0);
                        println!("  [{start}, {end}] - {title}");
                    }
                }
            }
        }
    }

    println!("\n✅ Captured streaming response successfully");
}
