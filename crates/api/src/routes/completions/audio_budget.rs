//! Normalize audio before monetary admission so neither container metadata nor compression
//! can understate the duration sent upstream. FFmpeg is restricted to pipes (no URLs/files).
use std::process::Stdio;
use std::time::Duration;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWriteExt};
use tokio::process::Command;
use tokio::sync::Semaphore;

static DECODERS: Semaphore = Semaphore::const_new(4);
const MAX_ENCODED_BYTES: usize = 25 * 1024 * 1024;
const MAX_DURATION_SECONDS: i64 = 3600;

async fn read_bounded<R: AsyncRead + Unpin>(
    reader: R,
    limit: usize,
) -> Result<Vec<u8>, &'static str> {
    let mut data = Vec::new();
    reader
        .take(limit as u64 + 1)
        .read_to_end(&mut data)
        .await
        .map_err(|_| "Cannot read decoded audio")?;
    if data.len() > limit {
        return Err("Decoded audio exceeds supported size");
    }
    Ok(data)
}

/// Returns FLAC and a rounded-up duration ceiling. No customer media is written to disk.
pub(super) async fn normalize(bytes: Vec<u8>) -> Result<(Vec<u8>, i32), &'static str> {
    let operation = async {
        let _slot = DECODERS
            .acquire()
            .await
            .map_err(|_| "Audio decoder unavailable")?;
        let mut child = Command::new("ffmpeg")
            .args([
                "-nostdin",
                "-v",
                "error",
                "-protocol_whitelist",
                "pipe",
                "-i",
                "pipe:0",
                "-map",
                "0:a:0",
                "-ac",
                "1",
                "-ar",
                "16000",
                "-af",
                "asetpts=N/SR/TB",
                "-c:a",
                "flac",
                "-f",
                "flac",
                "-progress",
                "pipe:2",
                "pipe:1",
            ])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .map_err(|_| "Audio decoder unavailable")?;
        let mut input = child.stdin.take().ok_or("Audio decoder unavailable")?;
        let output = child.stdout.take().ok_or("Audio decoder unavailable")?;
        let progress = child.stderr.take().ok_or("Audio decoder unavailable")?;
        let (_, data, progress, status) = tokio::try_join!(
            async {
                input
                    .write_all(&bytes)
                    .await
                    .map_err(|_| "Invalid audio input")?;
                input.shutdown().await.map_err(|_| "Invalid audio input")?;
                drop(input);
                Ok(())
            },
            read_bounded(output, MAX_ENCODED_BYTES),
            read_bounded(progress, 65536),
            async { child.wait().await.map_err(|_| "Audio decoding failed") },
        )?;
        if !status.success() || !data.starts_with(b"fLaC") {
            return Err("Invalid or unsupported audio");
        }
        let progress = std::str::from_utf8(&progress).map_err(|_| "Invalid audio duration")?;
        if !progress.lines().any(|line| line == "progress=end") {
            return Err("Incomplete audio decoding");
        }
        let micros = progress
            .lines()
            .filter_map(|line| line.strip_prefix("out_time_us="))
            .filter_map(|value| value.parse::<i64>().ok())
            .max()
            .ok_or("Missing audio duration")?;
        // One extra second covers codec framing and the upstream's second rounding.
        let seconds = micros
            .checked_add(999_999)
            .and_then(|v| v.checked_div(1_000_000))
            .and_then(|v| v.checked_add(1))
            .filter(|v| *v > 0 && *v <= MAX_DURATION_SECONDS)
            .ok_or("Audio exceeds the one-hour duration limit")?;
        Ok((data, seconds as i32))
    };
    tokio::time::timeout(Duration::from_secs(30), operation)
        .await
        .map_err(|_| "Audio decoding timed out")?
}

#[cfg(test)]
mod tests {
    use super::*;
    fn wav(samples: u32) -> Vec<u8> {
        let mut bytes = Vec::new();
        bytes.extend(b"RIFF");
        bytes.extend((36 + samples * 2).to_le_bytes());
        bytes.extend(b"WAVEfmt ");
        bytes.extend(16_u32.to_le_bytes());
        bytes.extend(1_u16.to_le_bytes());
        bytes.extend(1_u16.to_le_bytes());
        bytes.extend(16000_u32.to_le_bytes());
        bytes.extend(32000_u32.to_le_bytes());
        bytes.extend(2_u16.to_le_bytes());
        bytes.extend(16_u16.to_le_bytes());
        bytes.extend(b"data");
        bytes.extend((samples * 2).to_le_bytes());
        bytes.resize(44 + samples as usize * 2, 0);
        bytes
    }
    #[tokio::test]
    async fn normalized_samples_determine_duration() {
        let (encoded, bound) = normalize(wav(16001))
            .await
            .expect("ffmpeg must be installed for audio tests");
        assert!(encoded.starts_with(b"fLaC"));
        assert_eq!(bound, 3);
    }
    #[tokio::test]
    async fn rejects_container_without_audio() {
        assert!(normalize(b"not an audio container".to_vec()).await.is_err());
    }
}
