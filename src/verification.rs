use crate::{
    media::{Probe, number, rate},
    model::{Cancel, Events, Evidence, now, progress},
};
use anyhow::{Result, ensure};
use sha2::{Digest, Sha256};
use std::{fs::File, io::Read};

pub fn hash(
    mut file: File,
    cancel: &Cancel,
    events: &Events,
    index: usize,
    stage: &str,
) -> Result<Evidence> {
    let total = file.metadata()?.len();
    let mut bytes = 0;
    let mut sha = Sha256::new();
    let mut buffer = vec![0; 1024 * 1024];
    loop {
        cancel.check()?;
        let n = file.read(&mut buffer)?;
        if n == 0 {
            break;
        }
        sha.update(&buffer[..n]);
        bytes += n as u64;
        progress(events, index, stage, bytes, total);
    }
    ensure!(bytes == total, "File size changed while hashing");
    Ok(Evidence {
        sha256: format!("{:x}", sha.finalize()),
        bytes,
        checked_at: now(),
        checks: vec!["SHA-256 readback through client filesystem".into()],
    })
}
pub fn compare_hash(actual: &Evidence, expected: &Evidence) -> Result<()> {
    ensure!(
        actual.bytes == expected.bytes && actual.sha256 == expected.sha256,
        "Checksum mismatch. Preserve the files and investigate storage/source changes before retrying."
    );
    Ok(())
}
pub fn structure(source: &Probe, output: &Probe) -> Result<Vec<String>> {
    let video = source
        .video()
        .ok_or_else(|| anyhow::anyhow!("No source motion video"))?;
    let dest = output
        .video()
        .ok_or_else(|| anyhow::anyhow!("Editing copy has no motion video"))?;
    ensure!(
        output
            .streams
            .iter()
            .filter(|s| s.codec_type == "video")
            .count()
            == 1,
        "Unexpected video stream count"
    );
    ensure!(
        video.codec_name == dest.codec_name
            && video.width == dest.width
            && video.height == dest.height
            && video.pix_fmt == dest.pix_fmt,
        "Video format changed unexpectedly"
    );
    let frame = video
        .avg_frame_rate
        .as_deref()
        .and_then(rate)
        .filter(|v| *v > 0.)
        .map(|v| 1. / v)
        .unwrap_or(1. / 24.);
    if let (Some(a), Some(b)) = (
        video.avg_frame_rate.as_deref().and_then(rate),
        dest.avg_frame_rate.as_deref().and_then(rate),
    ) {
        ensure!((a - b).abs() < 0.02, "Video frame rate changed");
    }
    ensure!(
        video.color_range == dest.color_range
            && video.color_space == dest.color_space
            && video.color_transfer == dest.color_transfer
            && video.color_primaries == dest.color_primaries,
        "Video color metadata changed"
    );
    let audios: Vec<_> = source.audio().collect();
    let outputs: Vec<_> = output.audio().collect();
    ensure!(
        audios.len() == outputs.len(),
        "Audio stream count differs from expected manifest"
    );
    let mut tolerance = frame * 2.;
    for (a, b) in audios.iter().zip(outputs.iter()) {
        ensure!(
            b.codec_name == "pcm_s24le"
                && a.sample_rate == b.sample_rate
                && a.channels == b.channels
                && channel_layout(a) == channel_layout(b),
            "PCM audio format/sample rate/channel layout mismatch"
        );
        let sr = number(a.sample_rate.as_deref()).unwrap_or(48000.);
        let audio_tolerance = if a.codec_name == "aac" {
            2048. / sr
        } else {
            2. / sr
        };
        tolerance = tolerance.max(audio_tolerance);
        compare_timing(
            a.duration.as_deref(),
            b.duration.as_deref(),
            audio_tolerance + frame,
            "audio duration",
        )?;
        let source_offset = number(a.start_time.as_deref()).unwrap_or(0.)
            - number(video.start_time.as_deref()).unwrap_or(0.);
        let output_offset = number(b.start_time.as_deref()).unwrap_or(0.)
            - number(dest.start_time.as_deref()).unwrap_or(0.);
        ensure!(
            (source_offset - output_offset).abs() <= audio_tolerance + frame,
            "A/V timestamp offset changed"
        );
    }
    ensure!(output.duration() > 0., "Empty editing output");
    ensure!(
        source.duration() > 0.
            && (source.duration() - output.duration()).abs() <= tolerance + 0.005,
        "Unexpectedly truncated or extended editing copy"
    );
    compare_timing(
        video.duration.as_deref(),
        dest.duration.as_deref(),
        frame * 2.,
        "video duration",
    )?;
    let mut notices = Vec::new();
    for key in ["timecode"] {
        if video.tags[key] != dest.tags[key] {
            notices.push(format!(
                "Editing metadata differs: video {key}. Original retains source metadata."
            ));
        }
    }
    let rotation = |s: &crate::media::Stream| {
        s.side_data_list
            .as_array()
            .and_then(|a| a.iter().find_map(|v| v.get("rotation")))
            .cloned()
    };
    ensure!(
        rotation(video) == rotation(dest),
        "Video rotation metadata changed"
    );
    Ok(notices)
}
fn channel_layout(stream: &crate::media::Stream) -> Option<&str> {
    stream.channel_layout.as_deref().or(match stream.channels {
        Some(1) => Some("mono"),
        Some(2) => Some("stereo"),
        _ => None,
    })
}
fn compare_timing(a: Option<&str>, b: Option<&str>, tolerance: f64, label: &str) -> Result<()> {
    if let Some(a) = number(a) {
        let b = number(b).ok_or_else(|| anyhow::anyhow!("Missing output {label}"))?;
        ensure!(
            b > 0. && (a - b).abs() <= tolerance + 0.005,
            "Unexpected {label}: {a} → {b}"
        );
    }
    Ok(())
}
