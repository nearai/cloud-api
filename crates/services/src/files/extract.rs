//! Text extraction for chat `file` content parts (nearai/cloud-api#1153).
//!
//! No engine we route to reads OpenAI `file` parts, so the gateway turns each
//! PDF into plain text before dispatch. Like OpenAI Chat Completions, only
//! PDFs are accepted, detected by the `%PDF-` header rather than the client's
//! label.
//!
//! Parsing untrusted PDFs never happens in the gateway process. Each file is
//! handed to a fresh `pdf-extract-worker` process (same image, env cleared,
//! OS resource limits) over stdin/stdout, so a parser crash, abort or runaway
//! allocation kills one worker instead of every in-flight stream. At most
//! `max_workers` workers are alive at once; beyond that callers get `Busy`
//! immediately rather than queueing while holding request bodies in memory.
//!
//! Never log file bytes, extracted text, or filenames — sizes and outcomes only.

use async_trait::async_trait;
use base64::Engine;
use serde::{Deserialize, Serialize};
use std::borrow::Cow;
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::process::{Child, Command};
use tokio::sync::Semaphore;

/// How far into the file the `%PDF-` header may appear.
const PDF_HEADER_WINDOW: usize = 1024;

/// Name of the worker binary shipped next to `api` in the image.
pub const PDF_WORKER_BINARY: &str = "pdf-extract-worker";

/// Worker exit status when it cannot lower its own resource limits (Linux).
/// It exits before reading any input; the gateway reports it as an outage,
/// not as a bad PDF.
pub const WORKER_EXIT_LOCKDOWN_FAILED: i32 = 2;

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum FileExtractError {
    #[error("file_data must be a base64 data URL or base64 string")]
    InvalidData,
    #[error(
        "unsupported file type; Chat Completions accepts only PDF files, send other file contents as a text content part"
    )]
    UnsupportedType,
    #[error("the PDF could not be parsed")]
    Parse,
    #[error("the file contains no extractable text")]
    NoText,
    #[error("the file exceeds the {0} limit")]
    TooLarge(&'static str),
    #[error("file processing exceeded the time limit")]
    Timeout,
    #[error("file processing is at capacity")]
    Busy,
    #[error("file processing is unavailable")]
    Unavailable,
}

/// Limits shared by the gateway (parent) and the worker. Both are built from
/// the same commit, so the worker reads `ExtractLimits::default()` directly
/// instead of receiving limits over argv.
#[derive(Debug, Clone, Copy)]
pub struct ExtractLimits {
    /// Largest file (inline or stored) accepted; matches the chat body limit.
    pub max_file_bytes: usize,
    /// Per stream while loading and per page while extracting (bomb guard).
    pub max_decompressed_bytes: usize,
    pub max_pdf_pages: usize,
    /// Extracted characters, per file in the worker and per request in the
    /// resolver.
    pub max_chars: usize,
    /// Whole-request budget; callers pass one deadline for every file part.
    pub timeout: Duration,
    /// Live worker processes across the whole gateway.
    pub max_workers: usize,
    /// File parts accepted in one request.
    pub max_file_parts: usize,
}

impl Default for ExtractLimits {
    fn default() -> Self {
        Self {
            max_file_bytes: 25 * 1024 * 1024,
            max_decompressed_bytes: 16 * 1024 * 1024,
            max_pdf_pages: 500,
            max_chars: 1_000_000,
            timeout: Duration::from_secs(30),
            // Leave most cores to the async workers serving streams.
            max_workers: std::thread::available_parallelism()
                .map(|n| (n.get() / 4).max(1))
                .unwrap_or(1),
            max_file_parts: 4,
        }
    }
}

/// Worker → gateway reply, one JSON object on stdout.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkerReply {
    Ok(String),
    Err(WorkerFailure),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkerFailure {
    Parse,
    NoText,
    TooLargePages,
    TooLargeDecompressed,
    TooLargeChars,
    TooLargeFile,
    Timeout,
}

impl From<WorkerFailure> for FileExtractError {
    fn from(failure: WorkerFailure) -> Self {
        match failure {
            WorkerFailure::Parse => Self::Parse,
            WorkerFailure::NoText => Self::NoText,
            WorkerFailure::TooLargePages => Self::TooLarge("page"),
            WorkerFailure::TooLargeDecompressed => Self::TooLarge("decompressed size"),
            WorkerFailure::TooLargeChars => Self::TooLarge("character"),
            WorkerFailure::TooLargeFile => Self::TooLarge("file size"),
            WorkerFailure::Timeout => Self::Timeout,
        }
    }
}

#[async_trait]
pub trait FileTextExtractor: Send + Sync {
    fn limits(&self) -> &ExtractLimits;
    /// Plain text of a PDF. Fails fast with `Busy` when no worker slot is free.
    async fn extract_text(
        &self,
        bytes: Vec<u8>,
        deadline: tokio::time::Instant,
    ) -> Result<String, FileExtractError>;
}

pub struct WorkerPdfExtractor {
    limits: ExtractLimits,
    program: PathBuf,
    args: Vec<String>,
    permits: Arc<Semaphore>,
}

impl WorkerPdfExtractor {
    pub fn new(limits: ExtractLimits, program: PathBuf, args: Vec<String>) -> Self {
        Self {
            permits: Arc::new(Semaphore::new(limits.max_workers)),
            limits,
            program,
            args,
        }
    }

    /// Largest stdout the gateway reads back: the character cap in 4-byte
    /// UTF-8 plus JSON escaping headroom.
    fn max_output_bytes(&self) -> usize {
        self.limits.max_chars.saturating_mul(4) + 64 * 1024
    }
}

#[async_trait]
impl FileTextExtractor for WorkerPdfExtractor {
    fn limits(&self) -> &ExtractLimits {
        &self.limits
    }

    async fn extract_text(
        &self,
        bytes: Vec<u8>,
        deadline: tokio::time::Instant,
    ) -> Result<String, FileExtractError> {
        if bytes.len() > self.limits.max_file_bytes {
            return Err(FileExtractError::TooLarge("file size"));
        }
        if !is_pdf(&bytes) {
            return Err(FileExtractError::UnsupportedType);
        }
        if tokio::time::Instant::now() >= deadline {
            return Err(FileExtractError::Timeout);
        }
        let permit = self
            .permits
            .clone()
            .try_acquire_owned()
            .map_err(|_| FileExtractError::Busy)?;

        // env_clear: the gateway's environment holds database, OAuth and
        // provider secrets; the worker needs none of them.
        // kill_on_drop: a cancelled request (client disconnect) kills the
        // worker; tokio reaps it in the background. On that path the permit
        // drops with the future, so a dying worker can briefly exceed
        // `max_workers` until the reaper collects it.
        let mut child = Command::new(&self.program)
            .args(&self.args)
            .env_clear()
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .kill_on_drop(true)
            .spawn()
            .map_err(|e| {
                tracing::error!(error_kind = ?e.kind(), "chat file part: failed to spawn PDF worker");
                FileExtractError::Unavailable
            })?;

        let cap = self.max_output_bytes();
        let result = match tokio::time::timeout_at(deadline, exchange(&mut child, bytes, cap)).await
        {
            Ok(result) => result,
            Err(_) => {
                let _ = child.start_kill();
                let _ = child.wait().await;
                Err(FileExtractError::Timeout)
            }
        };
        // Released only once the worker has exited and been reaped.
        drop(permit);
        result
    }
}

/// Feed the worker its input, read its bounded reply, and reap it.
async fn exchange(
    child: &mut Child,
    bytes: Vec<u8>,
    cap: usize,
) -> Result<String, FileExtractError> {
    let mut stdin = child.stdin.take().ok_or(FileExtractError::Unavailable)?;
    let mut stdout = child.stdout.take().ok_or(FileExtractError::Unavailable)?;
    let write = async move {
        // A worker that dies early closes its stdin (EPIPE); its exit status
        // is the outcome that matters, so write errors are ignored.
        let _ = stdin.write_all(&bytes).await;
        let _ = stdin.shutdown().await;
    };
    let read = async {
        let mut out = Vec::new();
        (&mut stdout)
            .take(cap as u64 + 1)
            .read_to_end(&mut out)
            .await
            .map(|_| out)
    };
    let ((), output) = tokio::join!(write, read);
    let output = output.map_err(|_| FileExtractError::Parse)?;
    if output.len() > cap {
        let _ = child.start_kill();
        let _ = child.wait().await;
        return Err(FileExtractError::TooLarge("output"));
    }
    let status = child
        .wait()
        .await
        .map_err(|_| FileExtractError::Unavailable)?;
    if status.code() == Some(WORKER_EXIT_LOCKDOWN_FAILED) {
        tracing::error!("chat file part: PDF worker could not apply its resource limits");
        return Err(FileExtractError::Unavailable);
    }
    if !status.success() {
        tracing::warn!(
            exit_code = ?status.code(),
            "chat file part: PDF worker exited abnormally"
        );
        return Err(FileExtractError::Parse);
    }
    match serde_json::from_slice::<WorkerReply>(&output) {
        Ok(WorkerReply::Ok(text)) => Ok(text),
        Ok(WorkerReply::Err(failure)) => Err(failure.into()),
        Err(_) => Err(FileExtractError::Parse),
    }
}

/// Bytes of a `file_data` value: a base64 data URL (media type ignored —
/// content is sniffed) or bare base64. URLs are never fetched.
pub fn decode_file_data(file_data: &str) -> Result<Vec<u8>, FileExtractError> {
    let payload = match file_data.strip_prefix("data:") {
        Some(rest) => {
            let (meta, payload) = rest.split_once(',').ok_or(FileExtractError::InvalidData)?;
            if !meta
                .split(';')
                .any(|p| p.trim().eq_ignore_ascii_case("base64"))
            {
                return Err(FileExtractError::InvalidData);
            }
            payload
        }
        None => file_data,
    };
    let compact: Cow<'_, str> = if payload.bytes().any(|b| b.is_ascii_whitespace()) {
        Cow::Owned(
            payload
                .chars()
                .filter(|c| !c.is_ascii_whitespace())
                .collect(),
        )
    } else {
        Cow::Borrowed(payload)
    };
    match base64::engine::general_purpose::STANDARD.decode(compact.as_bytes()) {
        Ok(bytes) if !bytes.is_empty() => Ok(bytes),
        _ => Err(FileExtractError::InvalidData),
    }
}

/// Text part body for a resolved file; same layout as the Responses API's
/// `input_file` inlining so models see one convention.
pub fn format_file_text(filename: &str, text: &str) -> String {
    format!("File: {filename}\nContent:\n{text}")
}

/// PDF detection by content: `%PDF-` within the first 1024 bytes.
pub fn is_pdf(bytes: &[u8]) -> bool {
    bytes[..bytes.len().min(PDF_HEADER_WINDOW)]
        .windows(5)
        .any(|w| w == b"%PDF-")
}

#[cfg(test)]
mod tests {
    use super::*;
    use base64::Engine;

    /// Just enough of a header for the parent's `%PDF-` sniff; the fake
    /// workers below never parse it.
    const FAKE_PDF: &[u8] = b"%PDF-1.7\nfake body";

    fn b64(bytes: &[u8]) -> String {
        base64::engine::general_purpose::STANDARD.encode(bytes)
    }

    fn soon() -> tokio::time::Instant {
        tokio::time::Instant::now() + Duration::from_secs(10)
    }

    /// A worker played by `/bin/sh -c <script>`, so each test pins one
    /// process outcome without needing the real PDF worker binary.
    fn sh(script: &str, limits: ExtractLimits) -> WorkerPdfExtractor {
        WorkerPdfExtractor::new(
            limits,
            PathBuf::from("/bin/sh"),
            vec!["-c".to_string(), script.to_string()],
        )
    }

    async fn run(script: &str) -> Result<String, FileExtractError> {
        sh(script, ExtractLimits::default())
            .extract_text(FAKE_PDF.to_vec(), soon())
            .await
    }

    #[test]
    fn decodes_data_url_and_bare_base64() {
        assert_eq!(
            decode_file_data(&format!("data:application/pdf;base64,{}", b64(b"hi"))).unwrap(),
            b"hi"
        );
        assert_eq!(decode_file_data(&b64(b"hi")).unwrap(), b"hi");
        assert_eq!(
            decode_file_data("data:text/plain;charset=utf-8;BASE64,aGk=").unwrap(),
            b"hi"
        );
        assert_eq!(
            decode_file_data("aG\nk=").unwrap(),
            b"hi",
            "embedded whitespace is tolerated"
        );
    }

    #[test]
    fn rejects_invalid_file_data() {
        for bad in [
            "data:text/plain,hi",
            "data:application/pdf;base64",
            "not base64!",
            "",
            "https://example.com/a.pdf",
        ] {
            assert_eq!(
                decode_file_data(bad),
                Err(FileExtractError::InvalidData),
                "{bad:?}"
            );
        }
    }

    #[test]
    fn formats_like_responses_input_file() {
        assert_eq!(format_file_text("a.pdf", "hi"), "File: a.pdf\nContent:\nhi");
    }

    #[test]
    fn unsupported_type_message_matches_openai_guidance() {
        let msg = FileExtractError::UnsupportedType.to_string();
        assert!(
            msg.contains("only PDF") && msg.contains("text content part"),
            "{msg}"
        );
    }

    #[tokio::test]
    async fn worker_ok_reply_is_returned() {
        assert_eq!(
            run(r#"cat >/dev/null; printf '{"ok":"hello"}'"#).await,
            Ok("hello".to_string())
        );
    }

    #[tokio::test]
    async fn worker_receives_the_exact_bytes_on_stdin() {
        let n = FAKE_PDF.len();
        assert_eq!(
            run(r#"printf '{"ok":"%s"}' "$(wc -c | tr -d ' ')""#).await,
            Ok(n.to_string())
        );
    }

    #[tokio::test]
    async fn worker_does_not_inherit_the_gateway_environment() {
        // HOME is set for any test process; env_clear() must drop it.
        assert!(std::env::var_os("HOME").is_some());
        assert_eq!(
            run(r#"cat >/dev/null; printf '{"ok":"home=%s"}' "$HOME""#).await,
            Ok("home=".to_string())
        );
    }

    #[tokio::test]
    async fn worker_error_replies_map_to_typed_errors() {
        for (reply, want) in [
            ("parse", FileExtractError::Parse),
            ("no_text", FileExtractError::NoText),
            ("too_large_pages", FileExtractError::TooLarge("page")),
            (
                "too_large_decompressed",
                FileExtractError::TooLarge("decompressed size"),
            ),
            ("too_large_chars", FileExtractError::TooLarge("character")),
            ("too_large_file", FileExtractError::TooLarge("file size")),
            ("timeout", FileExtractError::Timeout),
        ] {
            let script = format!(r#"cat >/dev/null; printf '{{"err":"{reply}"}}'"#);
            assert_eq!(run(&script).await, Err(want), "{reply}");
        }
    }

    #[tokio::test]
    async fn crashed_failed_or_garbled_worker_is_parse_error() {
        for script in [
            "cat >/dev/null; kill -9 $$",
            "cat >/dev/null; exit 3",
            "cat >/dev/null; printf 'not json'",
            r#"cat >/dev/null; printf '{"ok":"x"}'; exit 1"#,
        ] {
            assert_eq!(run(script).await, Err(FileExtractError::Parse), "{script}");
        }
    }

    #[tokio::test]
    async fn worker_lockdown_failure_is_unavailable_not_a_parse_error() {
        // An infra problem (rlimits refused) must not be blamed on the PDF.
        let script = format!("cat >/dev/null; exit {WORKER_EXIT_LOCKDOWN_FAILED}");
        assert_eq!(run(&script).await, Err(FileExtractError::Unavailable));
    }

    #[tokio::test]
    async fn oversize_worker_output_is_rejected() {
        let limits = ExtractLimits {
            max_chars: 1,
            ..ExtractLimits::default()
        };
        let ex = sh("cat >/dev/null; head -c 200000 /dev/zero", limits);
        assert_eq!(
            ex.extract_text(FAKE_PDF.to_vec(), soon()).await,
            Err(FileExtractError::TooLarge("output"))
        );
    }

    #[tokio::test]
    async fn hung_worker_is_killed_at_the_deadline_and_frees_its_slot() {
        let limits = ExtractLimits {
            max_workers: 1,
            ..ExtractLimits::default()
        };
        let hung = sh("sleep 30", limits);
        let started = std::time::Instant::now();
        let deadline = tokio::time::Instant::now() + Duration::from_millis(300);
        assert_eq!(
            hung.extract_text(FAKE_PDF.to_vec(), deadline).await,
            Err(FileExtractError::Timeout)
        );
        assert!(started.elapsed() < Duration::from_secs(5));
        assert_eq!(hung.permits.available_permits(), 1, "slot must be freed");
    }

    #[tokio::test]
    async fn expired_deadline_is_timeout_without_spawning() {
        let ex = WorkerPdfExtractor::new(
            ExtractLimits::default(),
            PathBuf::from("/nonexistent/worker"),
            vec![],
        );
        let past = tokio::time::Instant::now();
        assert_eq!(
            ex.extract_text(FAKE_PDF.to_vec(), past).await,
            Err(FileExtractError::Timeout)
        );
    }

    #[tokio::test]
    async fn full_worker_pool_fails_fast_with_busy() {
        let limits = ExtractLimits {
            max_workers: 0,
            ..ExtractLimits::default()
        };
        assert_eq!(
            sh("sleep 30", limits)
                .extract_text(FAKE_PDF.to_vec(), soon())
                .await,
            Err(FileExtractError::Busy)
        );
    }

    #[tokio::test]
    async fn non_pdf_and_oversize_input_are_rejected_before_spawning() {
        let ex = WorkerPdfExtractor::new(
            ExtractLimits {
                max_file_bytes: 64,
                ..ExtractLimits::default()
            },
            PathBuf::from("/nonexistent/worker"),
            vec![],
        );
        assert_eq!(
            ex.extract_text(b"# plain markdown".to_vec(), soon()).await,
            Err(FileExtractError::UnsupportedType)
        );
        assert_eq!(
            ex.extract_text([FAKE_PDF, &[b' '; 64]].concat(), soon())
                .await,
            Err(FileExtractError::TooLarge("file size"))
        );
        // Declared type is irrelevant: detection is by content.
        assert!(is_pdf(&[b"\n\n", FAKE_PDF].concat()));
        assert!(!is_pdf(b"plain"));
    }

    #[tokio::test]
    async fn missing_worker_binary_is_unavailable() {
        let ex = WorkerPdfExtractor::new(
            ExtractLimits::default(),
            PathBuf::from("/nonexistent/worker"),
            vec![],
        );
        assert_eq!(
            ex.extract_text(FAKE_PDF.to_vec(), soon()).await,
            Err(FileExtractError::Unavailable)
        );
    }

    #[test]
    fn worker_reply_wire_format_is_stable() {
        assert_eq!(
            serde_json::to_string(&WorkerReply::Ok("t".into())).unwrap(),
            r#"{"ok":"t"}"#
        );
        assert_eq!(
            serde_json::to_string(&WorkerReply::Err(WorkerFailure::TooLargeDecompressed)).unwrap(),
            r#"{"err":"too_large_decompressed"}"#
        );
    }
}
