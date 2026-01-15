//! Common HTTP-level helpers for API E2E tests.

use api::models::{ConversationItemList, ConversationObject, ResponseObject};
use api::routes::attestation::SignatureResponse;
use inference_providers::StreamChunk;

/// POST `/v1/chat/completions` and return the raw response body text.
///
/// This is intentionally low-level so tests can compute hashes over the exact body when needed.
pub async fn post_chat_completions_raw(
    server: &axum_test::TestServer,
    api_key: &str,
    request_body: &serde_json::Value,
) -> String {
    let response = server
        .post("/v1/chat/completions")
        .add_header("Authorization", format!("Bearer {api_key}"))
        .json(request_body)
        .await;

    assert_eq!(
        response.status_code(),
        200,
        "POST /v1/chat/completions should return 200, got {} body={}",
        response.status_code(),
        response.text()
    );

    response.text()
}

/// Extract chat completion id from a `/v1/chat/completions` streaming (SSE) body.
pub fn extract_chat_id_from_chat_completions_sse(response_text: &str) -> Option<String> {
    for line in response_text.lines() {
        let Some(data) = line.strip_prefix("data: ") else {
            continue;
        };

        if data.trim() == "[DONE]" {
            break;
        }

        if let Ok(StreamChunk::Chat(chat_chunk)) = serde_json::from_str::<StreamChunk>(data) {
            return Some(chat_chunk.id);
        }
    }

    None
}

/// Run a `/v1/chat/completions` streaming request and return `(chat_id, raw_body)`.
pub async fn create_chat_completion_stream_and_get_id(
    server: &axum_test::TestServer,
    api_key: &str,
    request_body: &serde_json::Value,
) -> (String, String) {
    let response_text = post_chat_completions_raw(server, api_key, request_body).await;
    let chat_id = extract_chat_id_from_chat_completions_sse(&response_text)
        .expect("Should extract chat_id from SSE stream");
    (chat_id, response_text)
}

/// Run a non-streaming `/v1/chat/completions` request and return `(chat_id, raw_body)`.
pub async fn create_chat_completion_non_stream_and_get_id(
    server: &axum_test::TestServer,
    api_key: &str,
    request_body: &serde_json::Value,
) -> (String, String) {
    let response_text = post_chat_completions_raw(server, api_key, request_body).await;
    let response_json: serde_json::Value =
        serde_json::from_str(&response_text).expect("Chat completion response must be valid JSON");
    let chat_id = response_json
        .get("id")
        .and_then(|v| v.as_str())
        .expect("Chat completion response must have id field")
        .to_string();
    (chat_id, response_text)
}

/// POST `/v1/responses` (without conversation) and return the raw response body text.
///
/// This matches the real-e2e tests that only provide `input`/`model` fields.
pub async fn post_responses_raw(
    server: &axum_test::TestServer,
    api_key: &str,
    request_body: &serde_json::Value,
) -> String {
    let response = server
        .post("/v1/responses")
        .add_header("Authorization", format!("Bearer {api_key}"))
        .json(request_body)
        .await;

    assert_eq!(
        response.status_code(),
        200,
        "POST /v1/responses should return 200, got {} body={}",
        response.status_code(),
        response.text()
    );

    response.text()
}

/// Extract response id from a `/v1/responses` streaming (SSE) body by looking for `response.created`.
pub fn extract_response_id_from_responses_sse(response_text: &str) -> Option<String> {
    for line_chunk in response_text.split("\n\n") {
        if line_chunk.trim().is_empty() {
            continue;
        }

        let mut event_type = "";
        let mut event_data = "";

        for line in line_chunk.lines() {
            if let Some(event_name) = line.strip_prefix("event: ") {
                event_type = event_name;
            } else if let Some(data) = line.strip_prefix("data: ") {
                event_data = data;
            }
        }

        if event_type != "response.created" || event_data.is_empty() {
            continue;
        }

        let Ok(event_json) = serde_json::from_str::<serde_json::Value>(event_data) else {
            continue;
        };

        if let Some(response_obj) = event_json.get("response") {
            if let Some(id) = response_obj.get("id").and_then(|v| v.as_str()) {
                return Some(id.to_string());
            }
        }
    }

    None
}

/// Run a `/v1/responses` streaming request (without conversation) and return `(response_id, raw_body)`.
pub async fn create_response_stream_no_conversation_and_get_id(
    server: &axum_test::TestServer,
    api_key: &str,
    request_body: &serde_json::Value,
) -> (String, String) {
    let response_text = post_responses_raw(server, api_key, request_body).await;
    let response_id = extract_response_id_from_responses_sse(&response_text)
        .expect("Should have extracted response_id from stream");
    (response_id, response_text)
}

/// Run a non-streaming `/v1/responses` request (without conversation) and return `(response_id, raw_body)`.
pub async fn create_response_non_stream_no_conversation_and_get_id(
    server: &axum_test::TestServer,
    api_key: &str,
    request_body: &serde_json::Value,
) -> (String, String) {
    let response_text = post_responses_raw(server, api_key, request_body).await;
    let response_json: serde_json::Value =
        serde_json::from_str(&response_text).expect("Response must be valid JSON");
    let response_id = response_json
        .get("id")
        .and_then(|v| v.as_str())
        .expect("Response must have id field")
        .to_string();
    (response_id, response_text)
}

pub async fn create_conversation(
    server: &axum_test::TestServer,
    api_key: String,
) -> ConversationObject {
    create_conversation_with_metadata(server, api_key, None).await
}

pub async fn create_conversation_with_metadata(
    server: &axum_test::TestServer,
    api_key: String,
    metadata: Option<serde_json::Value>,
) -> ConversationObject {
    let response = server
        .post("/v1/conversations")
        .add_header("Authorization", format!("Bearer {api_key}"))
        .json(&serde_json::json!({
            "metadata": metadata.unwrap_or_else(|| serde_json::json!({}))
        }))
        .await;

    assert_eq!(
        response.status_code(),
        201,
        "Create conversation should return 201, got {} body={}",
        response.status_code(),
        response.text()
    );
    response.json::<ConversationObject>()
}

pub async fn get_conversation(
    server: &axum_test::TestServer,
    conversation_id: String,
    api_key: String,
) -> ConversationObject {
    let response = server
        .get(format!("/v1/conversations/{conversation_id}").as_str())
        .add_header("Authorization", format!("Bearer {api_key}"))
        .await;

    assert_eq!(
        response.status_code(),
        200,
        "Get conversation should return 200, got {} body={}",
        response.status_code(),
        response.text()
    );
    response.json::<ConversationObject>()
}

pub async fn list_conversation_items(
    server: &axum_test::TestServer,
    conversation_id: String,
    api_key: String,
) -> ConversationItemList {
    let response = server
        .get(format!("/v1/conversations/{conversation_id}/items").as_str())
        .add_header("Authorization", format!("Bearer {api_key}"))
        .await;

    assert_eq!(
        response.status_code(),
        200,
        "List conversation items should return 200, got {} body={}",
        response.status_code(),
        response.text()
    );
    response.json::<ConversationItemList>()
}

pub async fn create_response(
    server: &axum_test::TestServer,
    conversation_id: String,
    model: String,
    message: String,
    max_tokens: i64,
    api_key: String,
) -> ResponseObject {
    create_response_with_temperature(
        server,
        conversation_id,
        model,
        message,
        max_tokens,
        api_key,
        0.7,
    )
    .await
}

pub async fn create_response_with_temperature(
    server: &axum_test::TestServer,
    conversation_id: String,
    model: String,
    message: String,
    max_tokens: i64,
    api_key: String,
    temperature: f64,
) -> ResponseObject {
    let response = server
        .post("/v1/responses")
        .add_header("Authorization", format!("Bearer {api_key}"))
        .json(&serde_json::json!({
            "conversation": {
                "id": conversation_id,
            },
            "input": message,
            "temperature": temperature,
            "max_output_tokens": max_tokens,
            "stream": false,
            "model": model
        }))
        .await;

    assert_eq!(
        response.status_code(),
        200,
        "Create response should return 200, got {} body={}",
        response.status_code(),
        response.text()
    );
    response.json::<ResponseObject>()
}

pub async fn create_response_stream(
    server: &axum_test::TestServer,
    conversation_id: String,
    model: String,
    message: String,
    max_tokens: i64,
    api_key: String,
) -> (String, ResponseObject) {
    create_response_stream_with_temperature(
        server,
        conversation_id,
        model,
        message,
        max_tokens,
        api_key,
        0.7,
    )
    .await
}

pub async fn create_response_stream_with_temperature(
    server: &axum_test::TestServer,
    conversation_id: String,
    model: String,
    message: String,
    max_tokens: i64,
    api_key: String,
    temperature: f64,
) -> (String, ResponseObject) {
    let response = server
        .post("/v1/responses")
        .add_header("Authorization", format!("Bearer {api_key}"))
        .json(&serde_json::json!({
            "conversation": {
                "id": conversation_id,
            },
            "input": message,
            "temperature": temperature,
            "max_output_tokens": max_tokens,
            "stream": true,
            "model": model
        }))
        .await;

    assert_eq!(
        response.status_code(),
        200,
        "Create streaming response should return 200, got {} body={}",
        response.status_code(),
        response.text()
    );

    // For streaming responses, we get SSE events as text: "event: <type>\ndata: <json>\n\n"
    let response_text = response.text();

    let mut content = String::new();
    let mut final_response: Option<ResponseObject> = None;

    for line_chunk in response_text.split("\n\n") {
        if line_chunk.trim().is_empty() {
            continue;
        }

        let mut event_type = "";
        let mut event_data = "";

        for line in line_chunk.lines() {
            if let Some(event_name) = line.strip_prefix("event: ") {
                event_type = event_name;
            } else if let Some(data) = line.strip_prefix("data: ") {
                event_data = data;
            }
        }

        if event_data.is_empty() {
            continue;
        }

        let Ok(event_json) = serde_json::from_str::<serde_json::Value>(event_data) else {
            continue;
        };

        match event_type {
            "response.output_text.delta" => {
                if let Some(delta) = event_json.get("delta").and_then(|v| v.as_str()) {
                    content.push_str(delta);
                }
            }
            "response.completed" => {
                if let Some(response_obj) = event_json.get("response") {
                    final_response = Some(
                        serde_json::from_value::<ResponseObject>(response_obj.clone())
                            .expect("Failed to parse response.completed event"),
                    );
                }
            }
            _ => {}
        }
    }

    let final_resp = final_response.expect("Expected response.completed event from stream");
    (content, final_resp)
}

pub async fn upload_file(
    server: &axum_test::TestServer,
    api_key: &str,
    filename: &str,
    body: &[u8],
    mimetype: &str,
    purpose: &str,
) -> axum_test::TestResponse {
    server
        .post("/v1/files")
        .add_header("Authorization", format!("Bearer {api_key}"))
        .multipart(
            axum_test::multipart::MultipartForm::new()
                .add_text("purpose", purpose)
                .add_part(
                    "file",
                    axum_test::multipart::Part::bytes(body.to_vec())
                        .file_name(filename)
                        .mime_type(mimetype),
                ),
        )
        .await
}

/// GET `/v1/signature/{id}` for a given model and signing algorithm.
///
/// This helper is used by real_e2e signature tests. It ensures that the `model`
/// query parameter is URL-encoded before sending the request.
pub async fn get_signature_for_id(
    server: &axum_test::TestServer,
    api_key: &str,
    id: &str,
    model_name: &str,
    signing_algo: &str,
) -> SignatureResponse {
    let encoded_model =
        url::form_urlencoded::byte_serialize(model_name.as_bytes()).collect::<String>();
    let signature_url =
        format!("/v1/signature/{id}?model={encoded_model}&signing_algo={signing_algo}");

    let response = server
        .get(&signature_url)
        .add_header("Authorization", format!("Bearer {api_key}"))
        .await;

    assert_eq!(
        response.status_code(),
        200,
        "Signature endpoint should return success, got {} body={}",
        response.status_code(),
        response.text()
    );

    response.json::<SignatureResponse>()
}
