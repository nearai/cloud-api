//! The real `pdf-extract-worker` binary driven by the real
//! `WorkerPdfExtractor` (nearai/cloud-api#1153): protocol, limits and error
//! mapping end to end, without a database.

use services::files::extract::{
    ExtractLimits, FileExtractError, FileTextExtractor, WorkerPdfExtractor,
};
use std::path::PathBuf;
use std::time::Duration;

const HELLO_PDF: &[u8] = include_bytes!("fixtures/pdf/hello.pdf");
const NO_TEXT_PDF: &[u8] = include_bytes!("fixtures/pdf/no_text.pdf");
const BOMB_PDF: &[u8] = include_bytes!("fixtures/pdf/bomb.pdf");

fn worker() -> WorkerPdfExtractor {
    WorkerPdfExtractor::new(
        ExtractLimits::default(),
        PathBuf::from(env!("CARGO_BIN_EXE_pdf-extract-worker")),
        vec![],
    )
}

async fn extract(bytes: &[u8]) -> Result<String, FileExtractError> {
    worker()
        .extract_text(
            bytes.to_vec(),
            tokio::time::Instant::now() + Duration::from_secs(20),
        )
        .await
}

#[tokio::test]
async fn real_worker_extracts_pdf_text() {
    assert_eq!(
        extract(HELLO_PDF).await,
        Ok("Quarterly report\nRevenue grew 12 percent in Q3.".to_string())
    );
}

#[tokio::test]
async fn real_worker_refuses_decompression_bomb() {
    assert_eq!(
        extract(BOMB_PDF).await,
        Err(FileExtractError::TooLarge("decompressed size"))
    );
}

/// The worker's containment is only as good as the lockdown it actually
/// applies: inspect a live worker (parked on stdin, after lockdown and before
/// parsing) through /proc and check every limit and the seccomp filter.
#[cfg(target_os = "linux")]
#[test]
fn real_worker_applies_linux_lockdown() {
    use std::process::{Command, Stdio};
    use std::time::Instant;

    let mut child = Command::new(env!("CARGO_BIN_EXE_pdf-extract-worker"))
        .env_clear()
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .spawn()
        .expect("spawn worker");
    let proc_dir = format!("/proc/{}", child.id());
    let read = |name: &str| std::fs::read_to_string(format!("{proc_dir}/{name}")).unwrap();

    // Lockdown finishes before the worker blocks on stdin; wait for its own
    // seccomp filter (the last step). Count filters rather than checking the
    // mode: containers (Docker's default profile) already run every process
    // in filter mode, so only an extra filter proves the worker's is in place.
    let filters = |status: &str| -> u32 {
        status
            .lines()
            .find_map(|l| l.strip_prefix("Seccomp_filters:"))
            .and_then(|v| v.trim().parse().ok())
            .expect("kernel reports Seccomp_filters")
    };
    let inherited = filters(&std::fs::read_to_string("/proc/self/status").unwrap());
    let started = Instant::now();
    let status = loop {
        let status = read("status");
        if filters(&status) > inherited {
            break status;
        }
        assert!(
            started.elapsed().as_secs() < 5,
            "seccomp filter never applied:\n{status}"
        );
        std::thread::sleep(std::time::Duration::from_millis(10));
    };
    assert!(status.lines().any(|l| l == "Seccomp:\t2"), "{status}");
    assert!(status.lines().any(|l| l == "NoNewPrivs:\t1"), "{status}");

    let limits = read("limits");
    let limit = |name: &str| -> Vec<String> {
        let line = limits
            .lines()
            .find(|l| l.starts_with(name))
            .unwrap_or_else(|| panic!("{name} missing:\n{limits}"));
        line[name.len()..]
            .split_whitespace()
            .take(2)
            .map(str::to_string)
            .collect()
    };
    assert_eq!(limit("Max cpu time"), ["35", "36"]);
    assert_eq!(limit("Max file size"), ["0", "0"]);
    assert_eq!(limit("Max core file size"), ["0", "0"]);
    assert_eq!(limit("Max open files"), ["16", "16"]);
    assert_eq!(limit("Max address space"), ["1073741824", "1073741824"]);

    child.kill().unwrap();
    child.wait().unwrap();
}

#[tokio::test]
async fn real_worker_reports_no_text_and_parse_failures() {
    assert_eq!(extract(NO_TEXT_PDF).await, Err(FileExtractError::NoText));
    assert_eq!(
        extract(b"%PDF-1.7\n not really a pdf \x00\x01").await,
        Err(FileExtractError::Parse)
    );
}
