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

#[tokio::test]
async fn real_worker_reports_no_text_and_parse_failures() {
    assert_eq!(extract(NO_TEXT_PDF).await, Err(FileExtractError::NoText));
    assert_eq!(
        extract(b"%PDF-1.7\n not really a pdf \x00\x01").await,
        Err(FileExtractError::Parse)
    );
}
