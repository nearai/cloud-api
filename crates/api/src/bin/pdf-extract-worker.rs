//! `pdf-extract-worker`: parses one untrusted PDF and prints its text.
//!
//! Spawned by `services::files::extract::WorkerPdfExtractor`, one process per
//! file (nearai/cloud-api#1153). Reads the PDF from stdin, writes a single
//! JSON `WorkerReply` to stdout, exits 0. This is the only code in the image
//! that links `lopdf`; keeping it in its own process means a parser abort,
//! stack overflow or runaway allocation kills this worker, not the gateway.
//!
//! Before reading input the worker lowers its own resource limits (CPU time,
//! address space on Linux, no file writes, few descriptors). On Linux a limit
//! that cannot be applied is fatal: the worker exits non-zero without
//! parsing, which the gateway reports as a parse failure.
//!
//! Never writes file bytes or extracted text anywhere but stdout, and never
//! logs.

use services::files::extract::{
    ExtractLimits, WorkerFailure, WorkerReply, WORKER_EXIT_LOCKDOWN_FAILED,
};
use std::io::{Read, Write};
use std::time::Instant;

/// Address-space cap for the worker (Linux only; macOS does not enforce it).
#[cfg(target_os = "linux")]
const WORKER_ADDRESS_SPACE_BYTES: u64 = 1024 * 1024 * 1024;
/// CPU-seconds backstop above the gateway's 30s request deadline.
const WORKER_CPU_SECONDS: u64 = 35;
/// stdin, stdout, stderr and a little slack; the parser opens no files.
const WORKER_MAX_OPEN_FILES: u64 = 16;

fn main() {
    if lock_down().is_err() && cfg!(target_os = "linux") {
        std::process::exit(WORKER_EXIT_LOCKDOWN_FAILED);
    }
    let reply = match run(std::io::stdin().lock(), ExtractLimits::default()) {
        Ok(text) => WorkerReply::Ok(text),
        Err(failure) => WorkerReply::Err(failure),
    };
    let mut stdout = std::io::stdout().lock();
    if serde_json::to_writer(&mut stdout, &reply).is_err() || stdout.flush().is_err() {
        std::process::exit(3);
    }
}

fn lock_down() -> std::io::Result<()> {
    use rustix::process::{setrlimit, Resource, Rlimit};
    let cap = |limit: u64| Rlimit {
        current: Some(limit),
        maximum: Some(limit),
    };
    setrlimit(
        Resource::Cpu,
        Rlimit {
            current: Some(WORKER_CPU_SECONDS),
            maximum: Some(WORKER_CPU_SECONDS + 1),
        },
    )?;
    setrlimit(Resource::Fsize, cap(0))?;
    setrlimit(Resource::Core, cap(0))?;
    setrlimit(Resource::Nofile, cap(WORKER_MAX_OPEN_FILES))?;
    #[cfg(target_os = "linux")]
    setrlimit(Resource::As, cap(WORKER_ADDRESS_SPACE_BYTES))?;
    Ok(())
}

fn run(input: impl Read, limits: ExtractLimits) -> Result<String, WorkerFailure> {
    let mut bytes = Vec::new();
    input
        .take(limits.max_file_bytes as u64 + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| WorkerFailure::Parse)?;
    if bytes.len() > limits.max_file_bytes {
        return Err(WorkerFailure::TooLargeFile);
    }
    pdf_text(&bytes, &limits, Instant::now() + limits.timeout)
}

fn map_lopdf(error: lopdf::Error) -> WorkerFailure {
    match error {
        lopdf::Error::Decompress(lopdf::DecompressError::MemoryLimitExceeded { .. }) => {
            WorkerFailure::TooLargeDecompressed
        }
        _ => WorkerFailure::Parse,
    }
}

/// Pages that fail to parse are skipped (one odd font should not sink a
/// document); a decompression bomb on any page, or every page failing,
/// rejects the file.
fn pdf_text(
    bytes: &[u8],
    limits: &ExtractLimits,
    deadline: Instant,
) -> Result<String, WorkerFailure> {
    let options = lopdf::LoadOptions::with_max_decompressed_size(limits.max_decompressed_bytes);
    let doc = lopdf::Document::load_mem_with_options(bytes, options).map_err(map_lopdf)?;
    let pages: Vec<u32> = doc.get_pages().keys().copied().collect();
    if pages.len() > limits.max_pdf_pages {
        return Err(WorkerFailure::TooLargePages);
    }
    let mut text = String::new();
    let mut chars = 0usize;
    let mut failed_pages = 0usize;
    for page in &pages {
        if Instant::now() >= deadline {
            return Err(WorkerFailure::Timeout);
        }
        let page_text = match doc.extract_text_with_limit(&[*page], limits.max_decompressed_bytes) {
            Ok(page_text) => page_text,
            Err(e @ lopdf::Error::Decompress(_)) => return Err(map_lopdf(e)),
            Err(_) => {
                failed_pages += 1;
                continue;
            }
        };
        // Drop the trailing padding PDF generators emit per line.
        for line in page_text.lines() {
            let line = line.trim_end();
            chars += line.chars().count() + 1;
            if chars > limits.max_chars + 1 {
                return Err(WorkerFailure::TooLargeChars);
            }
            text.push_str(line);
            text.push('\n');
        }
    }
    if !pages.is_empty() && failed_pages == pages.len() {
        return Err(WorkerFailure::Parse);
    }
    let text = text.trim();
    if text.is_empty() {
        return Err(WorkerFailure::NoText);
    }
    Ok(text.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    const HELLO_PDF: &[u8] = include_bytes!("../../tests/fixtures/pdf/hello.pdf");
    const NO_TEXT_PDF: &[u8] = include_bytes!("../../tests/fixtures/pdf/no_text.pdf");
    /// 66 KB on disk; its page content inflates to 64 MiB.
    const BOMB_PDF: &[u8] = include_bytes!("../../tests/fixtures/pdf/bomb.pdf");

    fn extract(bytes: &[u8]) -> Result<String, WorkerFailure> {
        run(bytes, ExtractLimits::default())
    }

    #[test]
    fn extracts_text_layer() {
        assert_eq!(
            extract(HELLO_PDF),
            Ok("Quarterly report\nRevenue grew 12 percent in Q3.".to_string())
        );
    }

    #[test]
    fn page_without_text_is_no_text() {
        assert_eq!(extract(NO_TEXT_PDF), Err(WorkerFailure::NoText));
    }

    #[test]
    fn corrupt_pdf_is_parse_failure() {
        assert_eq!(
            extract(b"%PDF-1.7\n not really a pdf \x00\x01"),
            Err(WorkerFailure::Parse)
        );
    }

    #[test]
    fn decompression_bomb_is_refused_quickly() {
        let started = Instant::now();
        assert_eq!(extract(BOMB_PDF), Err(WorkerFailure::TooLargeDecompressed));
        assert!(started.elapsed().as_secs() < 2);
    }

    #[test]
    fn caps_reject() {
        let d = ExtractLimits::default();
        assert_eq!(
            run(
                HELLO_PDF,
                ExtractLimits {
                    max_file_bytes: 100,
                    ..d
                }
            ),
            Err(WorkerFailure::TooLargeFile)
        );
        assert_eq!(
            run(
                HELLO_PDF,
                ExtractLimits {
                    max_pdf_pages: 0,
                    ..d
                }
            ),
            Err(WorkerFailure::TooLargePages)
        );
        assert_eq!(
            run(HELLO_PDF, ExtractLimits { max_chars: 5, ..d }),
            Err(WorkerFailure::TooLargeChars)
        );
    }

    #[test]
    fn passed_deadline_is_timeout() {
        assert_eq!(
            pdf_text(HELLO_PDF, &ExtractLimits::default(), Instant::now()),
            Err(WorkerFailure::Timeout)
        );
    }
}
