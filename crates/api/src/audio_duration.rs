//! Duration-only metering of an uploaded file; no decoding, transcoding, or network I/O.
use lofty::{
    config::{ParseOptions, ParsingMode},
    file::{AudioFile, FileType},
    probe::Probe,
};
use matroska_demuxer::{Frame, MatroskaFile};
use std::io::Cursor;

pub(crate) fn seconds(bytes: Vec<u8>) -> Option<i32> {
    if bytes.starts_with(&[0x1a, 0x45, 0xdf, 0xa3]) {
        return webm_seconds(&bytes);
    }
    let options = ParseOptions::new()
        .read_tags(false)
        .read_cover_art(false)
        .parsing_mode(ParsingMode::Strict);
    let probe = Probe::new(Cursor::new(&bytes))
        .guess_file_type()
        .ok()?
        .options(options);
    let duration = match probe.file_type()? {
        // Scan MPEG frames (or their explicit Xing/Info frame count). Lofty's MPEG fallback
        // estimates duration from bitrate; such an estimate must not become an actual charge.
        FileType::Mpeg => mp3_duration::from_read(&mut Cursor::new(&bytes)).ok()?,
        // These containers carry sample counts or media timestamps. Ignore arbitrary tags.
        FileType::Wav | FileType::Flac | FileType::Mp4 | FileType::Vorbis | FileType::Opus => {
            probe.read().ok()?.properties().duration()
        }
        _ => return None,
    };
    // Round once, without floating point accumulation.
    positive_seconds(duration.as_nanos().div_ceil(1_000_000_000))
}

fn positive_seconds(value: u128) -> Option<i32> {
    i32::try_from(value).ok().filter(|seconds| *seconds > 0)
}

fn webm_seconds(bytes: &[u8]) -> Option<i32> {
    let mut file = MatroskaFile::open(Cursor::new(bytes)).ok()?;
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
    fn uploaded_formats_use_container_timing_and_invalid_files_are_not_estimated_as_actual_usage() {
        for bytes in [
            include_bytes!("../tests/fixtures/audio/silence.wav").as_slice(),
            include_bytes!("../tests/fixtures/audio/silence.mp3").as_slice(),
            include_bytes!("../tests/fixtures/audio/silence.flac").as_slice(),
            include_bytes!("../tests/fixtures/audio/silence.m4a").as_slice(),
            include_bytes!("../tests/fixtures/audio/silence.ogg").as_slice(),
            include_bytes!("../tests/fixtures/audio/silence.webm").as_slice(),
        ] {
            assert_eq!(seconds(bytes.to_vec()), Some(2));
        }
        // Valid audio can require provider usage instead of local metering.
        assert_eq!(
            seconds(include_bytes!("../tests/fixtures/audio/silence-vorbis.webm").to_vec()),
            None
        );
        for bytes in [vec![], vec![0; 4096], b"not audio".to_vec()] {
            assert_eq!(seconds(bytes), None);
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
