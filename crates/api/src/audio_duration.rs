//! Duration-only metering of an uploaded file; no decoding, transcoding, or network I/O.
use lofty::{
    config::{ParseOptions, ParsingMode},
    file::{AudioFile, FileType},
    probe::Probe,
};
use matroska_demuxer::{Frame, MatroskaFile};
use std::io::{self, Cursor, Read, Seek, SeekFrom};
use std::time::{Duration, Instant};

static PARSERS: tokio::sync::Semaphore = tokio::sync::Semaphore::const_new(4);

/// Optional local metering must not queue unlimited work or change accepted uploads.
pub(crate) async fn measure(bytes: bytes::Bytes) -> Option<i32> {
    let permit = PARSERS.try_acquire().ok()?;
    tokio::task::spawn_blocking(move || {
        let _permit = permit;
        seconds(&bytes)
    })
    .await
    .ok()
    .flatten()
}

struct MeterReader<'a> {
    cursor: Cursor<&'a [u8]>,
    deadline: Instant,
    operations: u32,
}
impl<'a> MeterReader<'a> {
    fn new(bytes: &'a [u8]) -> Self {
        Self {
            cursor: Cursor::new(bytes),
            deadline: Instant::now() + Duration::from_secs(1),
            operations: 0,
        }
    }
    fn check(&mut self) -> io::Result<()> {
        self.operations += 1;
        if self.operations > 1_000_000 || Instant::now() >= self.deadline {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "audio metering budget exhausted",
            ));
        }
        Ok(())
    }
}
impl Read for MeterReader<'_> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        self.check()?;
        self.cursor.read(buf)
    }
}
impl Seek for MeterReader<'_> {
    fn seek(&mut self, pos: SeekFrom) -> io::Result<u64> {
        self.check()?;
        self.cursor.seek(pos)
    }
}

fn seconds(bytes: &[u8]) -> Option<i32> {
    if bytes.starts_with(&[0x1a, 0x45, 0xdf, 0xa3]) {
        return webm_seconds(bytes);
    }
    let options = ParseOptions::new()
        .read_tags(false)
        .read_cover_art(false)
        .parsing_mode(ParsingMode::Strict);
    let probe = Probe::new(MeterReader::new(bytes))
        .guess_file_type()
        .ok()?
        .options(options);
    let duration = match probe.file_type()? {
        // Count complete MPEG frames, never the mutable Xing/Info summary.
        FileType::Mpeg => return mp3_seconds(bytes),
        // These containers carry sample counts or media timestamps. Ignore arbitrary tags.
        FileType::Wav | FileType::Flac | FileType::Mp4 | FileType::Vorbis | FileType::Opus => {
            probe.read().ok()?.properties().duration()
        }
        _ => return None,
    };
    // Round once, without floating point accumulation.
    positive_seconds(duration.as_nanos().div_ceil(1_000_000_000))
}

// MPEG Layer III header fields determine each frame's size and sample count.
// A single initial Xing/Info frame is metadata, not an audio frame. No decoding is needed.
fn mp3_seconds(mut bytes: &[u8]) -> Option<i32> {
    let deadline = Instant::now() + Duration::from_secs(1);
    if bytes.starts_with(b"ID3") {
        let header = bytes.get(..10)?;
        let mut length = 0_usize;
        for &b in header.get(6..10)? {
            if b & 0x80 != 0 {
                return None;
            }
            length = (length << 7) | usize::from(b);
        }
        let footer = if header[3] == 4 && header[5] & 0x10 != 0 {
            10
        } else {
            0
        };
        bytes = bytes.get(10 + length + footer..)?;
    }
    let mut ticks = 0_u64;
    let mut first = true;
    while !bytes.is_empty() {
        if Instant::now() >= deadline {
            return None;
        }
        if (bytes.len() == 128 && bytes.starts_with(b"TAG")) || bytes.starts_with(b"APETAGEX") {
            break;
        }
        let h = bytes.get(..4)?;
        let version = (h[1] >> 3) & 3;
        if h[0] != 0xff || h[1] & 0xe0 != 0xe0 || h[1] & 6 != 2 || version == 1 {
            return None;
        }
        let rate = *[44100_u64, 48000, 32000].get(usize::from((h[2] >> 2) & 3))?
            / match version {
                3 => 1,
                2 => 2,
                _ => 4,
            };
        let rates = if version == 3 {
            [
                0_u64, 32, 40, 48, 56, 64, 80, 96, 112, 128, 160, 192, 224, 256, 320, 0,
            ]
        } else {
            [
                0_u64, 8, 16, 24, 32, 40, 48, 56, 64, 80, 96, 112, 128, 144, 160, 0,
            ]
        };
        let bitrate = *rates.get(usize::from(h[2] >> 4))? * 1000;
        if bitrate == 0 {
            return None;
        }
        let samples = if version == 3 { 1152 } else { 576 };
        let length =
            usize::try_from((samples / 8) * bitrate / rate + u64::from((h[2] >> 1) & 1)).ok()?;
        let frame = bytes.get(..length)?;
        let mono = h[3] >> 6 == 3;
        let side = match (version == 3, mono) {
            (true, false) => 32,
            (true, true) | (false, false) => 17,
            (false, true) => 9,
        };
        let marker = frame.get(4 + side..8 + side);
        if !(first && matches!(marker, Some(b"Xing" | b"Info"))) {
            // Common integer timebase for all MPEG sample rates; round only once.
            ticks = ticks.checked_add(samples * (14_112_000 / rate))?;
        }
        first = false;
        bytes = bytes.get(length..)?;
    }
    positive_seconds(u128::from(ticks).div_ceil(14_112_000))
}

fn positive_seconds(value: u128) -> Option<i32> {
    i32::try_from(value).ok().filter(|seconds| *seconds > 0)
}

fn webm_seconds(bytes: &[u8]) -> Option<i32> {
    let mut file = MatroskaFile::open(MeterReader::new(bytes)).ok()?;
    let track = file.tracks().iter().find(|track| track.audio().is_some())?;
    if track.codec_id() != "A_OPUS" {
        return None;
    }
    let id = track.track_number().get();
    let mut samples = 0_u64;
    let mut frame = Frame::default();
    while file.next_frame(&mut frame).ok()? {
        if frame.track == id {
            // WebM may omit block durations; Opus has an explicit duration in every packet.
            samples = samples.checked_add(opus_samples(&frame.data)?)?;
        }
    }
    positive_seconds(u128::from(samples).div_ceil(48_000))
}

// RFC 6716 sections 3.1 and 3.2: frame size and count are in the packet header.
fn opus_samples(packet: &[u8]) -> Option<u64> {
    let toc = *packet.first()?;
    let config = toc >> 3;
    let samples = match config {
        0..=11 => [480_u64, 960, 1920, 2880][usize::from(config & 3)],
        12..=15 => [480, 960][usize::from(config & 1)],
        _ => [120, 240, 480, 960][usize::from(config & 3)],
    };
    let count = match toc & 3 {
        0 => 1,
        1 | 2 => 2,
        _ => u64::from(*packet.get(1)? & 0x3f),
    };
    let total = samples * count;
    (count > 0 && total <= 5760).then_some(total)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mp3_summary_counts_do_not_replace_frame_metering() {
        let bytes = include_bytes!("../tests/fixtures/audio/silence.mp3");
        let marker = bytes
            .windows(4)
            .position(|b| b == b"Xing" || b == b"Info")
            .unwrap();
        let expected = seconds(bytes);
        assert_eq!(expected, Some(2));
        for count in [0_u32, 1, u32::MAX] {
            let mut altered = bytes.to_vec();
            altered[marker + 8..marker + 12].copy_from_slice(&count.to_be_bytes());
            assert_eq!(seconds(&altered), expected);
        }
        assert_eq!(seconds(&bytes[..bytes.len() - 1]), None);
    }

    #[tokio::test]
    async fn optional_metering_bounds_workers_and_parser_io() {
        let permits = PARSERS.acquire_many(4).await.unwrap();
        let bytes =
            bytes::Bytes::from_static(include_bytes!("../tests/fixtures/audio/silence.wav"));
        assert_eq!(measure(bytes.clone()).await, None);
        drop(permits);
        assert_eq!(measure(bytes).await, Some(2));
        let mut reader = MeterReader::new(b"audio");
        reader.deadline = Instant::now();
        assert_eq!(
            reader.read(&mut [0_u8; 1]).unwrap_err().kind(),
            io::ErrorKind::TimedOut
        );
        assert_eq!(
            reader.seek(SeekFrom::Start(0)).unwrap_err().kind(),
            io::ErrorKind::TimedOut
        );
    }

    #[test]
    fn uploaded_formats_use_container_timing_and_invalid_files_are_not_estimated_as_actual_usage() {
        for bytes in [
            include_bytes!("../tests/fixtures/audio/silence.wav").as_slice(),
            include_bytes!("../tests/fixtures/audio/silence.mp3").as_slice(),
            include_bytes!("../tests/fixtures/audio/silence.flac").as_slice(),
            include_bytes!("../tests/fixtures/audio/silence.m4a").as_slice(),
            include_bytes!("../tests/fixtures/audio/silence.ogg").as_slice(),
            include_bytes!("../tests/fixtures/audio/silence.webm").as_slice(),
        ] {
            assert_eq!(seconds(bytes), Some(2));
        }
        // Valid audio can require provider usage instead of local metering.
        assert_eq!(
            seconds(include_bytes!(
                "../tests/fixtures/audio/silence-vorbis.webm"
            )),
            None
        );
        for bytes in [vec![], vec![0; 4096], b"not audio".to_vec()] {
            assert_eq!(seconds(&bytes), None);
        }
    }

    #[test]
    fn opus_packet_duration_covers_frame_modes_and_rejects_invalid_counts() {
        for (packet, expected) in [
            (vec![0], Some(480)),
            (vec![3 << 3], Some(2880)),
            (vec![13 << 3], Some(960)),
            (vec![16 << 3], Some(120)),
            (vec![(19 << 3) | 1], Some(1920)),
            (vec![(19 << 3) | 3, 6], Some(5760)),
            (vec![(19 << 3) | 3, 7], None),
            (vec![3, 0], None),
            (vec![3], None),
        ] {
            assert_eq!(opus_samples(&packet), expected);
        }
    }
}
