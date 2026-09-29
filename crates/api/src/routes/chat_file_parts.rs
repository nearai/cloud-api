//! Resolve OpenAI chat `file` content parts to text parts before dispatch
//! (nearai/cloud-api#1153). No engine we route to reads `file` parts (vLLM and
//! SGLang reject them, Gemini drops them, anthropic_compat 400s), so the
//! gateway extracts the PDF's text — in a sandboxed worker process, see
//! `services::files::extract` — as OpenAI does server-side.
//!
//! Never log file bytes, extracted text, or filenames.
//!
//! Attestation: the request hash sent upstream (`X-Request-Hash`) and signed by
//! the model TEE is the SHA-256 of the client's original bytes, while the model
//! sees the extracted text. This is intentional and mirrors auto-redact; the
//! hash binds the response to what the client sent, not to the rewritten body.

use crate::models::{ErrorResponse, FilePartSource, MessageContentPart};
use axum::{http::StatusCode, response::IntoResponse, Json as ResponseJson};
use serde_json::Value;
use services::completions::ports::CompletionMessage;
use services::files::extract::{
    decode_file_data, format_file_text, ExtractLimits, FileExtractError, FileTextExtractor,
};
use services::files::{FileServiceError, FileServiceTrait};
use services::metrics::{consts, MetricsServiceTrait};
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use uuid::Uuid;

const DEFAULT_FILENAME: &str = "document";
/// Client filenames are prompt text; cap them so a huge `filename` cannot
/// dwarf the extracted content.
const MAX_FILENAME_CHARS: usize = 255;

enum FilePartError {
    TooManyParts(usize),
    Invalid(&'static str),
    Extract(FileExtractError),
    NotFound,
    Storage,
}

impl FilePartError {
    fn outcome(&self) -> &'static str {
        match self {
            Self::TooManyParts(_) => "too_many_parts",
            Self::Invalid(_) | Self::Extract(FileExtractError::InvalidData) => "invalid",
            Self::Extract(FileExtractError::UnsupportedType) => "unsupported",
            Self::Extract(FileExtractError::Parse) => "parse_error",
            Self::Extract(FileExtractError::NoText) => "no_text",
            Self::Extract(FileExtractError::TooLarge(_)) => "too_large",
            Self::Extract(FileExtractError::Timeout) => "timeout",
            Self::Extract(FileExtractError::Busy) => "busy",
            Self::Extract(FileExtractError::Unavailable) => "unavailable",
            Self::NotFound => "not_found",
            Self::Storage => "storage_error",
        }
    }
}

fn is_file_part(part: &Value) -> bool {
    part.get("type").and_then(Value::as_str) == Some("file")
}

/// True when any message content array carries a `file` part.
pub fn has_file_parts(messages: &[CompletionMessage]) -> bool {
    messages
        .iter()
        .filter_map(|m| m.content.as_array())
        .any(|parts| parts.iter().any(is_file_part))
}

/// Replace every `file` part with `{"type":"text","text":"File: …"}` in both
/// the service messages and, when present, the raw `original_request` (the
/// Anthropic adapter converts from it). One deadline and one character budget
/// cover the whole request; identical sources are extracted once. On failure
/// the returned response names the offending `messages[i].content[j]`.
pub async fn resolve_file_parts(
    messages: &mut [CompletionMessage],
    mut original_request: Option<&mut Value>,
    workspace_id: Uuid,
    files: &(dyn FileServiceTrait + Send + Sync),
    extractor: &dyn FileTextExtractor,
    metrics: &dyn MetricsServiceTrait,
) -> Result<(), axum::response::Response> {
    let started = std::time::Instant::now();
    let result = resolve_all(
        messages,
        &mut original_request,
        workspace_id,
        files,
        extractor,
    )
    .await;
    let outcome = match &result {
        Ok(()) => "ok",
        Err((e, _)) => e.outcome(),
    };
    let tag = format!("outcome:{outcome}");
    metrics.record_latency(
        consts::METRIC_CHAT_FILE_PARTS_LATENCY,
        started.elapsed(),
        &[tag.as_str()],
    );
    metrics.record_count(consts::METRIC_CHAT_FILE_PARTS_COUNT, 1, &[tag.as_str()]);
    result.map_err(|(e, param)| error_response(e, &param))
}

async fn resolve_all(
    messages: &mut [CompletionMessage],
    original_request: &mut Option<&mut Value>,
    workspace_id: Uuid,
    files: &(dyn FileServiceTrait + Send + Sync),
    extractor: &dyn FileTextExtractor,
) -> Result<(), (FilePartError, String)> {
    let limits = *extractor.limits();
    let count = messages
        .iter()
        .filter_map(|m| m.content.as_array())
        .flat_map(|parts| parts.iter())
        .filter(|p| is_file_part(p))
        .count();
    if count > limits.max_file_parts {
        return Err((
            FilePartError::TooManyParts(limits.max_file_parts),
            "messages".to_string(),
        ));
    }
    let deadline = tokio::time::Instant::now() + limits.timeout;
    // Source key → (extracted text, stored filename).
    let mut extracted: HashMap<String, (String, Option<String>)> = HashMap::new();
    let mut total_chars = 0usize;

    for (i, message) in messages.iter_mut().enumerate() {
        let Some(parts) = message.content.as_array_mut() else {
            continue;
        };
        for (j, part) in parts.iter_mut().enumerate() {
            if !is_file_part(part) {
                continue;
            }
            let param = format!("messages[{i}].content[{j}]");
            // Move (not clone) the part out: `file_data` can be tens of MB.
            let typed: MessageContentPart =
                serde_json::from_value(std::mem::take(part)).map_err(|_| {
                    (
                        FilePartError::Invalid("invalid file content part"),
                        param.clone(),
                    )
                })?;
            let source = typed
                .file_source()
                .map_err(|m| (FilePartError::Invalid(m), param.clone()))?
                .ok_or((
                    FilePartError::Invalid("invalid file content part"),
                    param.clone(),
                ))?;
            let (key, filename) = match source {
                FilePartSource::Inline { data, filename } => (
                    format!("inline:{}", hex::encode(Sha256::digest(data.as_bytes()))),
                    filename.map(str::to_string),
                ),
                FilePartSource::Stored { file_id, filename } => {
                    (format!("stored:{file_id}"), filename.map(str::to_string))
                }
            };
            let (text, stored_name) = match extracted.get(&key) {
                Some(hit) => hit.clone(),
                None => {
                    // Storage reads share the request deadline with extraction.
                    let (bytes, stored_name) = tokio::time::timeout_at(
                        deadline,
                        load_bytes(source, workspace_id, files, &limits),
                    )
                    .await
                    .map_err(|_| {
                        (
                            FilePartError::Extract(FileExtractError::Timeout),
                            param.clone(),
                        )
                    })?
                    .map_err(|e| (e, param.clone()))?;
                    let text = extractor
                        .extract_text(bytes, deadline)
                        .await
                        .map_err(|e| (FilePartError::Extract(e), param.clone()))?;
                    extracted.insert(key, (text.clone(), stored_name.clone()));
                    (text, stored_name)
                }
            };
            total_chars += text.chars().count();
            if total_chars > limits.max_chars {
                return Err((
                    FilePartError::Extract(FileExtractError::TooLarge("character")),
                    param,
                ));
            }
            let name: String = filename
                .or(stored_name)
                .as_deref()
                .unwrap_or(DEFAULT_FILENAME)
                .chars()
                .take(MAX_FILENAME_CHARS)
                .collect();
            let replacement =
                serde_json::json!({ "type": "text", "text": format_file_text(&name, &text) });
            if let Some(slot) = original_request
                .as_deref_mut()
                .and_then(|o| o.pointer_mut(&format!("/messages/{i}/content/{j}")))
                .filter(|slot| is_file_part(slot))
            {
                *slot = replacement.clone();
            }
            *part = replacement;
        }
    }
    Ok(())
}

/// Bytes for one source. Stored files are size-checked from metadata before
/// download (uploads may be up to 512 MB); the file service enforces
/// workspace ownership.
async fn load_bytes(
    source: FilePartSource<'_>,
    workspace_id: Uuid,
    files: &(dyn FileServiceTrait + Send + Sync),
    limits: &ExtractLimits,
) -> Result<(Vec<u8>, Option<String>), FilePartError> {
    match source {
        FilePartSource::Inline { data, .. } => Ok((
            decode_file_data(data).map_err(FilePartError::Extract)?,
            None,
        )),
        FilePartSource::Stored { file_id, .. } => {
            let id = Uuid::parse_str(
                file_id
                    .strip_prefix(services::id_prefixes::PREFIX_FILE)
                    .unwrap_or(file_id),
            )
            .map_err(|_| FilePartError::NotFound)?;
            let map = |e| match e {
                FileServiceError::NotFound => FilePartError::NotFound,
                _ => FilePartError::Storage,
            };
            let meta = files.get_file(id, workspace_id).await.map_err(map)?;
            if usize::try_from(meta.bytes).map_or(true, |b| b > limits.max_file_bytes) {
                return Err(FilePartError::Extract(FileExtractError::TooLarge(
                    "file size",
                )));
            }
            let (file, bytes) = files
                .get_file_content(id, workspace_id)
                .await
                .map_err(map)?;
            Ok((bytes, Some(file.filename)))
        }
    }
}

fn error_response(error: FilePartError, param: &str) -> axum::response::Response {
    let bad_request = |message: String| (StatusCode::BAD_REQUEST, "invalid_request_error", message);
    let (status, error_type, message) = match error {
        FilePartError::Extract(FileExtractError::Busy) => (
            crate::routes::common::status_overloaded(),
            "service_overloaded",
            "File processing is at capacity. Please retry with exponential backoff.".to_string(),
        ),
        FilePartError::Extract(FileExtractError::Unavailable) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            "server_error",
            "File processing is unavailable".to_string(),
        ),
        // The deadline is a server-side budget (worker contention included),
        // so report it as one rather than blaming the request.
        FilePartError::Extract(FileExtractError::Timeout) => (
            StatusCode::GATEWAY_TIMEOUT,
            "server_error",
            format!("{param}: file processing exceeded the time limit"),
        ),
        FilePartError::Storage => {
            tracing::error!("chat file part: failed to read stored file");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                "server_error",
                "Failed to read file".to_string(),
            )
        }
        FilePartError::TooManyParts(max) => bad_request(format!(
            "at most {max} file content parts are allowed per request"
        )),
        FilePartError::NotFound => bad_request(format!("{param}: file not found")),
        FilePartError::Invalid(msg) => bad_request(format!("{param}: {msg}")),
        FilePartError::Extract(e) => bad_request(format!("{param}: {e}")),
    };
    (
        status,
        ResponseJson(ErrorResponse::with_param(
            message,
            error_type.to_string(),
            param.to_string(),
        )),
    )
        .into_response()
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use base64::Engine;
    use serde_json::json;
    use services::files::extract::ExtractLimits;
    use services::files::{File, UploadFileParams};
    use services::metrics::MockMetricsService;
    use std::sync::atomic::{AtomicUsize, Ordering};

    const WS: Uuid = Uuid::from_u128(7);
    const STORED: Uuid = Uuid::from_u128(42);
    const HUGE: Uuid = Uuid::from_u128(43);
    /// A stored file whose metadata read stalls far past any deadline.
    const STALLED: Uuid = Uuid::from_u128(44);

    fn stored_file(id: Uuid, bytes: i64) -> File {
        File {
            id,
            filename: "stored.pdf".into(),
            bytes,
            content_type: "application/pdf".into(),
            purpose: "user_data".into(),
            storage_key: "k".into(),
            workspace_id: WS,
            uploaded_by_api_key_id: Uuid::nil(),
            created_at: chrono::Utc::now(),
            expires_at: None,
        }
    }

    struct Files;
    #[async_trait]
    impl FileServiceTrait for Files {
        async fn upload_file(&self, _: UploadFileParams) -> Result<File, FileServiceError> {
            unimplemented!()
        }
        async fn get_file(&self, id: Uuid, ws: Uuid) -> Result<File, FileServiceError> {
            if id == STALLED {
                tokio::time::sleep(std::time::Duration::from_secs(60)).await;
            }
            match (id, ws) {
                (STORED, WS) => Ok(stored_file(STORED, 12)),
                (HUGE, WS) => Ok(stored_file(HUGE, 512 * 1024 * 1024)),
                _ => Err(FileServiceError::NotFound),
            }
        }
        async fn get_file_content(
            &self,
            id: Uuid,
            ws: Uuid,
        ) -> Result<(File, Vec<u8>), FileServiceError> {
            assert_ne!(
                id, HUGE,
                "oversize stored file must be rejected before download"
            );
            Ok((self.get_file(id, ws).await?, b"stored-bytes".to_vec()))
        }
        async fn list_files(
            &self,
            _: Uuid,
            _: Option<Uuid>,
            _: i64,
            _: &str,
            _: Option<String>,
        ) -> Result<Vec<File>, FileServiceError> {
            unimplemented!()
        }
        async fn delete_file(&self, _: Uuid, _: Uuid) -> Result<bool, FileServiceError> {
            unimplemented!()
        }
    }

    /// Stands in for the PDF worker so these tests pin only the resolver's
    /// plumbing; PDF behavior is covered in `services::files::extract`, the
    /// `pdf_worker` integration test and e2e.
    struct Echo {
        limits: ExtractLimits,
        calls: AtomicUsize,
        fail: Option<FileExtractError>,
    }
    impl Echo {
        fn new() -> Self {
            Self {
                limits: ExtractLimits::default(),
                calls: AtomicUsize::new(0),
                fail: None,
            }
        }
    }
    #[async_trait]
    impl FileTextExtractor for Echo {
        fn limits(&self) -> &ExtractLimits {
            &self.limits
        }
        async fn extract_text(
            &self,
            bytes: Vec<u8>,
            _: tokio::time::Instant,
        ) -> Result<String, FileExtractError> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            match &self.fail {
                Some(e) => Err(e.clone()),
                None => Ok(String::from_utf8(bytes).unwrap()),
            }
        }
    }

    fn msg(content: serde_json::Value) -> CompletionMessage {
        CompletionMessage {
            role: "user".into(),
            content,
            tool_call_id: None,
            tool_calls: None,
            reasoning_content: None,
        }
    }

    fn inline(text: &str, filename: Option<&str>) -> serde_json::Value {
        let data = format!(
            "data:application/pdf;base64,{}",
            base64::engine::general_purpose::STANDARD.encode(text)
        );
        match filename {
            Some(name) => json!({"type": "file", "file": {"file_data": data, "filename": name}}),
            None => json!({"type": "file", "file": {"file_data": data}}),
        }
    }

    async fn run(
        messages: &mut [CompletionMessage],
        original: Option<&mut serde_json::Value>,
        ex: &dyn FileTextExtractor,
    ) -> Result<(), axum::response::Response> {
        resolve_file_parts(messages, original, WS, &Files, ex, &MockMetricsService).await
    }

    async fn error_json(resp: axum::response::Response) -> (u16, serde_json::Value) {
        let status = resp.status().as_u16();
        let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        (status, serde_json::from_slice(&body).unwrap())
    }

    #[tokio::test]
    async fn parts_become_text_in_messages_and_original_request() {
        let contents = [
            json!("plain string content is untouched"),
            json!([{"type": "text", "text": "q"}, inline("alpha", Some("a.pdf")), inline("beta", None)]),
            json!([{"type": "file", "file_id": format!("file-{STORED}")}]),
        ];
        let mut messages: Vec<_> = contents.iter().cloned().map(msg).collect();
        let mut original = json!({
            "model": "m",
            "messages": contents.iter().map(|c| json!({"role": "user", "content": c})).collect::<Vec<_>>()
        });
        assert!(has_file_parts(&messages));
        run(&mut messages, Some(&mut original), &Echo::new())
            .await
            .unwrap();
        assert!(!has_file_parts(&messages));
        let expected = [
            json!("plain string content is untouched"),
            json!([
                {"type": "text", "text": "q"},
                {"type": "text", "text": "File: a.pdf\nContent:\nalpha"},
                {"type": "text", "text": "File: document\nContent:\nbeta"},
            ]),
            json!([{"type": "text", "text": "File: stored.pdf\nContent:\nstored-bytes"}]),
        ];
        for (i, want) in expected.iter().enumerate() {
            assert_eq!(&messages[i].content, want);
            // The Anthropic adapter converts from the raw body; it must see
            // the same text, not the file part.
            assert_eq!(&original["messages"][i]["content"], want);
        }
    }

    #[tokio::test]
    async fn nested_file_id_filename_overrides_stored_name() {
        let mut messages = vec![msg(
            json!([{"type": "file", "file": {"file_id": format!("file-{STORED}"), "filename": "mine.pdf"}}]),
        )];
        run(&mut messages, None, &Echo::new()).await.unwrap();
        assert_eq!(
            messages[0].content[0]["text"],
            "File: mine.pdf\nContent:\nstored-bytes"
        );
    }

    #[tokio::test]
    async fn repeated_source_is_extracted_once() {
        // Multi-turn history re-sends the same file every turn.
        let echo = Echo::new();
        let mut messages = vec![
            msg(json!([inline("same", Some("a.pdf"))])),
            msg(json!([inline("same", Some("b.pdf"))])),
        ];
        run(&mut messages, None, &echo).await.unwrap();
        assert_eq!(echo.calls.load(Ordering::SeqCst), 1);
        assert_eq!(
            messages[1].content[0]["text"],
            "File: b.pdf\nContent:\nsame"
        );
    }

    #[tokio::test]
    async fn repeated_stored_file_keeps_its_stored_name() {
        let echo = Echo::new();
        let part = json!({"type": "file", "file_id": format!("file-{STORED}")});
        let mut messages = vec![msg(json!([part.clone()])), msg(json!([part]))];
        run(&mut messages, None, &echo).await.unwrap();
        assert_eq!(echo.calls.load(Ordering::SeqCst), 1);
        for message in &messages {
            assert_eq!(
                message.content[0]["text"],
                "File: stored.pdf\nContent:\nstored-bytes"
            );
        }
    }

    #[tokio::test]
    async fn stalled_stored_file_read_hits_the_request_deadline() {
        let echo = Echo {
            limits: ExtractLimits {
                timeout: std::time::Duration::from_millis(100),
                ..ExtractLimits::default()
            },
            ..Echo::new()
        };
        let mut messages = vec![msg(
            json!([{"type": "file", "file": {"file_id": format!("file-{STALLED}")}}]),
        )];
        let started = std::time::Instant::now();
        let (status, body) = error_json(run(&mut messages, None, &echo).await.unwrap_err()).await;
        assert!(started.elapsed() < std::time::Duration::from_secs(5));
        assert_eq!(status, 504, "{body}");
        assert_eq!(body["error"]["param"], "messages[0].content[0]");
        assert_eq!(echo.calls.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn oversized_client_filename_is_truncated() {
        let long = "a".repeat(10_000);
        let mut messages = vec![msg(json!([inline("x", Some(&long))]))];
        run(&mut messages, None, &Echo::new()).await.unwrap();
        let expected = format!("File: {}\nContent:\nx", "a".repeat(MAX_FILENAME_CHARS));
        assert_eq!(messages[0].content[0]["text"], expected);
    }

    #[tokio::test]
    async fn too_many_file_parts_is_400_before_any_work() {
        let echo = Echo::new();
        let parts: Vec<_> = (0..5).map(|i| inline(&format!("f{i}"), None)).collect();
        let mut messages = vec![msg(json!(parts))];
        let (status, body) = error_json(run(&mut messages, None, &echo).await.unwrap_err()).await;
        assert_eq!(status, 400);
        assert_eq!(body["error"]["param"], "messages");
        assert_eq!(echo.calls.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn aggregate_char_budget_is_enforced() {
        let echo = Echo {
            limits: ExtractLimits {
                max_chars: 8,
                ..ExtractLimits::default()
            },
            ..Echo::new()
        };
        let mut messages = vec![msg(json!([inline("12345", None), inline("67890", None)]))];
        let (status, body) = error_json(run(&mut messages, None, &echo).await.unwrap_err()).await;
        assert_eq!(status, 400);
        assert_eq!(body["error"]["param"], "messages[0].content[1]");
    }

    #[tokio::test]
    async fn unknown_foreign_or_oversize_file_id_is_400_with_param() {
        for (id, needle) in [(Uuid::from_u128(99), "file not found"), (HUGE, "file size")] {
            let mut messages = vec![
                msg(json!([inline("ok", None)])),
                msg(json!([
                    {"type": "text", "text": "x"},
                    {"type": "file", "file": {"file_id": format!("file-{id}")}}
                ])),
            ];
            let (status, body) =
                error_json(run(&mut messages, None, &Echo::new()).await.unwrap_err()).await;
            assert_eq!(status, 400);
            assert_eq!(body["error"]["type"], "invalid_request_error");
            assert_eq!(body["error"]["param"], "messages[1].content[1]");
            assert!(
                body["error"]["message"].as_str().unwrap().contains(needle),
                "{body}"
            );
        }
    }

    #[tokio::test]
    async fn malformed_file_data_is_400_without_echo() {
        let mut messages = vec![msg(
            json!([{"type": "file", "file": {"file_data": "SECRET-not-base64!"}}]),
        )];
        let (status, body) =
            error_json(run(&mut messages, None, &Echo::new()).await.unwrap_err()).await;
        assert_eq!(status, 400);
        assert_eq!(body["error"]["param"], "messages[0].content[0]");
        assert!(!body.to_string().contains("SECRET"));
    }

    #[tokio::test]
    async fn extractor_errors_map_to_status_codes() {
        for (err, status, error_type) in [
            (FileExtractError::Busy, 429, "service_overloaded"),
            (FileExtractError::Unavailable, 500, "server_error"),
            (FileExtractError::Timeout, 504, "server_error"),
            (
                FileExtractError::UnsupportedType,
                400,
                "invalid_request_error",
            ),
            (FileExtractError::NoText, 400, "invalid_request_error"),
        ] {
            let echo = Echo {
                fail: Some(err.clone()),
                ..Echo::new()
            };
            let mut messages = vec![msg(json!([inline("x", None)]))];
            let (got, body) = error_json(run(&mut messages, None, &echo).await.unwrap_err()).await;
            assert_eq!(got, status, "{err:?}");
            assert_eq!(body["error"]["type"], error_type, "{err:?}");
        }
    }
}
