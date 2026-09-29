//! Chat `file` content parts are resolved to text before dispatch
//! (nearai/cloud-api#1153), through the real `pdf-extract-worker` binary.

use crate::common::*;
use base64::Engine;
use serde_json::json;

const HELLO_PDF: &[u8] = include_bytes!("../fixtures/pdf/hello.pdf");
const NO_TEXT_PDF: &[u8] = include_bytes!("../fixtures/pdf/no_text.pdf");
const HELLO_TEXT: &str = "Quarterly report\nRevenue grew 12 percent in Q3.";

fn b64(bytes: &[u8]) -> String {
    base64::engine::general_purpose::STANDARD.encode(bytes)
}

fn pdf_part(bytes: &[u8], filename: &str) -> serde_json::Value {
    json!({
        "type": "file",
        "file": {"filename": filename, "file_data": format!("data:application/pdf;base64,{}", b64(bytes))}
    })
}

async fn post_messages(
    server: &axum_test::TestServer,
    api_key: &str,
    model: &str,
    messages: serde_json::Value,
) -> axum_test::TestResponse {
    server
        .post("/v1/chat/completions")
        .add_header("Authorization", format!("Bearer {api_key}"))
        .add_header("User-Agent", MOCK_USER_AGENT)
        .json(&json!({
            "model": model,
            "messages": messages,
            "stream": false,
            "max_tokens": 20
        }))
        .await
}

async fn post_chat(
    server: &axum_test::TestServer,
    api_key: &str,
    model: &str,
    content: serde_json::Value,
) -> axum_test::TestResponse {
    post_messages(
        server,
        api_key,
        model,
        json!([{"role": "user", "content": content}]),
    )
    .await
}

async fn upload_pdf(server: &axum_test::TestServer, api_key: &str, name: &str) -> String {
    let response = server
        .post("/v1/files")
        .add_header("Authorization", format!("Bearer {api_key}"))
        .multipart(
            axum_test::multipart::MultipartForm::new()
                .add_text("purpose", "user_data")
                .add_part(
                    "file",
                    axum_test::multipart::Part::bytes(HELLO_PDF.to_vec())
                        .file_name(name)
                        .mime_type("application/pdf"),
                ),
        )
        .await;
    assert_eq!(
        response.status_code(),
        201,
        "upload failed: {}",
        response.text()
    );
    response.json::<serde_json::Value>()["id"]
        .as_str()
        .unwrap()
        .to_string()
}

fn user_contents(params: &inference_providers::ChatCompletionParams) -> Vec<serde_json::Value> {
    params
        .messages
        .iter()
        .filter(|m| m.role == inference_providers::MessageRole::User)
        .map(|m| m.content.clone().unwrap_or_default())
        .collect()
}

#[tokio::test]
async fn inline_pdf_reaches_provider_as_text() {
    let (server, _pool, mock_provider, _db) = setup_test_server_with_pool().await;
    let model = setup_qwen_model(&server).await;
    let org = setup_org_with_credits(&server, 10_000_000_000i64).await;
    let api_key = get_api_key_for_org(&server, org.id).await;

    let response = post_chat(
        &server,
        &api_key,
        &model,
        json!([{"type": "text", "text": "Summarize"}, pdf_part(HELLO_PDF, "report.pdf")]),
    )
    .await;
    assert_eq!(response.status_code(), 200, "{}", response.text());

    let params = mock_provider.last_chat_params().await.unwrap();
    let expected = json!([
        {"type": "text", "text": "Summarize"},
        {"type": "text", "text": format!("File: report.pdf\nContent:\n{HELLO_TEXT}")},
    ]);
    assert_eq!(user_contents(&params), vec![expected.clone()]);
    // The Anthropic adapter converts from the raw body: patched, not dropped.
    let original = params
        .original_request
        .as_ref()
        .expect("original_request kept");
    assert_eq!(original["messages"][0]["content"], expected);
}

#[tokio::test]
async fn file_parts_across_turns_all_resolve() {
    let (server, _pool, mock_provider, _db) = setup_test_server_with_pool().await;
    let model = setup_qwen_model(&server).await;
    let org = setup_org_with_credits(&server, 10_000_000_000i64).await;
    let api_key = get_api_key_for_org(&server, org.id).await;

    let response = post_messages(
        &server,
        &api_key,
        &model,
        json!([
            {"role": "user", "content": [pdf_part(HELLO_PDF, "r.pdf")]},
            {"role": "assistant", "content": "ok"},
            // Bare base64, no filename: falls back to "document".
            {"role": "user", "content": [{"type": "file", "file": {"file_data": b64(HELLO_PDF)}}]},
        ]),
    )
    .await;
    assert_eq!(response.status_code(), 200, "{}", response.text());

    let params = mock_provider.last_chat_params().await.unwrap();
    assert_eq!(
        user_contents(&params),
        vec![
            json!([{"type": "text", "text": format!("File: r.pdf\nContent:\n{HELLO_TEXT}")}]),
            json!([{"type": "text", "text": format!("File: document\nContent:\n{HELLO_TEXT}")}]),
        ]
    );
}

#[tokio::test]
async fn uploaded_file_id_resolves_and_other_orgs_cannot_read_it() {
    let (server, _pool, mock_provider, _db) = setup_test_server_with_pool().await;
    let model = setup_qwen_model(&server).await;
    let org = setup_org_with_credits(&server, 10_000_000_000i64).await;
    let api_key = get_api_key_for_org(&server, org.id).await;
    let file_id = upload_pdf(&server, &api_key, "spec.pdf").await;

    // Nested OpenAI shape and the legacy flat shape both resolve.
    for part in [
        json!({"type": "file", "file": {"file_id": file_id}}),
        json!({"type": "file", "file_id": file_id}),
    ] {
        let response = post_chat(&server, &api_key, &model, json!([part])).await;
        assert_eq!(response.status_code(), 200, "{}", response.text());
        let params = mock_provider.last_chat_params().await.unwrap();
        assert_eq!(
            user_contents(&params),
            vec![
                json!([{"type": "text", "text": format!("File: spec.pdf\nContent:\n{HELLO_TEXT}")}])
            ]
        );
    }

    let other_org = setup_org_with_credits(&server, 10_000_000_000i64).await;
    let other_key = get_api_key_for_org(&server, other_org.id).await;
    let response = post_chat(
        &server,
        &other_key,
        &model,
        json!([{"type": "file", "file": {"file_id": file_id}}]),
    )
    .await;
    assert_eq!(response.status_code(), 400);
    let text = response.text();
    assert!(text.contains("file not found"), "{text}");
    assert!(!text.contains("Quarterly"), "must not leak content: {text}");
}

#[tokio::test]
async fn unreadable_or_unsupported_files_are_clear_400s() {
    let (server, _pool, _mock, _db) = setup_test_server_with_pool().await;
    let model = setup_qwen_model(&server).await;
    let org = setup_org_with_credits(&server, 10_000_000_000i64).await;
    let api_key = get_api_key_for_org(&server, org.id).await;

    // Scanned / image-only PDF: no text layer.
    let response = post_chat(
        &server,
        &api_key,
        &model,
        json!([pdf_part(NO_TEXT_PDF, "scan.pdf")]),
    )
    .await;
    assert_eq!(response.status_code(), 400);
    assert!(
        response.text().contains("no extractable text"),
        "{}",
        response.text()
    );

    // Not base64.
    let response = post_chat(
        &server,
        &api_key,
        &model,
        json!([{"type": "file", "file": {"file_data": "%%%"}}]),
    )
    .await;
    assert_eq!(response.status_code(), 400);

    // No source at all.
    let response = post_chat(
        &server,
        &api_key,
        &model,
        json!([{"type": "file", "file": {"filename": "x.pdf"}}]),
    )
    .await;
    assert_eq!(response.status_code(), 400);
    assert!(
        response.text().contains("exactly one of"),
        "{}",
        response.text()
    );

    // OpenAI parity: non-PDF files are refused with guidance.
    let response = post_chat(
        &server,
        &api_key,
        &model,
        json!([{"type": "file", "file": {"filename": "n.md", "file_data": format!("data:text/markdown;base64,{}", b64(b"# notes"))}}]),
    )
    .await;
    assert_eq!(response.status_code(), 400);
    assert!(response.text().contains("only PDF"), "{}", response.text());
}

#[tokio::test]
async fn file_parts_with_e2ee_are_rejected() {
    let (server, _pool, _mock, _db) = setup_test_server_with_pool().await;
    let model = setup_qwen_model(&server).await;
    let org = setup_org_with_credits(&server, 10_000_000_000i64).await;
    let api_key = get_api_key_for_org(&server, org.id).await;

    let response = server
        .post("/v1/chat/completions")
        .add_header("Authorization", format!("Bearer {api_key}"))
        .add_header("User-Agent", MOCK_USER_AGENT)
        .add_header("X-Signing-Algo", "ecdsa")
        .add_header(
            "X-Client-Pub-Key",
            "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef",
        )
        .json(&json!({
            "model": model,
            "messages": [{"role": "user", "content": [pdf_part(HELLO_PDF, "r.pdf")]}],
            "stream": false,
            "max_tokens": 20
        }))
        .await;
    assert_eq!(response.status_code(), 400, "{}", response.text());
    assert!(
        response.text().contains("end-to-end encryption"),
        "{}",
        response.text()
    );
}

#[tokio::test]
async fn unknown_model_is_rejected_without_file_work() {
    let (server, _pool, _mock, _db) = setup_test_server_with_pool().await;
    let org = setup_org_with_credits(&server, 10_000_000_000i64).await;
    let api_key = get_api_key_for_org(&server, org.id).await;

    let model = format!("no-such-model-{}", uuid::Uuid::new_v4());
    let response = post_chat(
        &server,
        &api_key,
        &model,
        json!([{"type": "file", "file": {"file_data": "%%%"}}]),
    )
    .await;
    assert_eq!(response.status_code(), 400);
    let body: serde_json::Value = response.json();
    let message = body["error"]["message"].as_str().unwrap_or_default();
    // The model error, not a file-processing error (which would name the
    // part via `param` and mean the file was parsed first).
    assert!(message.contains(&model), "{body}");
    assert_ne!(body["error"]["param"], "messages[0].content[0]", "{body}");
}
