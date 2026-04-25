use std::{collections::HashMap, path::Path};

use serde::Deserialize;
use time::OffsetDateTime;
use tokio::process::Command;

use crate::{
    clock::{format_timestamp, round_to},
    library::playback::{VideoPlaybackInfo, classify_playback_mode},
    persistence::ProbeOutcome,
    protocol::{AudioStream, PlaybackMode, SubtitleStream},
};

#[derive(Debug, Deserialize)]
struct FfprobeOutput {
    #[serde(default)]
    streams: Vec<FfprobeStream>,
    format: Option<FfprobeFormat>,
}

#[derive(Debug, Deserialize)]
struct FfprobeStream {
    #[serde(default)]
    index: Option<u32>,
    codec_type: Option<String>,
    codec_name: Option<String>,
    profile: Option<String>,
    #[serde(default)]
    level: Option<i32>,
    pix_fmt: Option<String>,
    bits_per_raw_sample: Option<serde_json::Value>,
    width: Option<u32>,
    height: Option<u32>,
    channels: Option<u32>,
    channel_layout: Option<String>,
    #[serde(default)]
    tags: HashMap<String, String>,
    #[serde(default)]
    disposition: HashMap<String, u8>,
}

#[derive(Debug, Deserialize)]
struct FfprobeFormat {
    format_name: Option<String>,
    duration: Option<String>,
}

/// Run ffprobe against `path` and turn its JSON output into a
/// `ProbeOutcome` ready to be persisted by the probe pool. `probe_command`
/// is required because pool callers check for it upfront and skip
/// enqueueing if disabled.
pub(crate) async fn probe_media_metadata(
    path: &Path,
    file_extension: Option<&str>,
    probe_command: Option<&Path>,
) -> ProbeOutcome {
    let probed_at = format_timestamp(OffsetDateTime::now_utc());
    let Some(probe_command) = probe_command else {
        return probe_outcome_with_error(probed_at, "no ffprobe configured");
    };

    let output = Command::new(probe_command)
        .arg("-v")
        .arg("error")
        .arg("-show_entries")
        .arg(
            "format=format_name,duration:stream=index,codec_type,codec_name,profile,level,\
             pix_fmt,bits_per_raw_sample,width,height,channels,channel_layout:\
             stream_tags=language,title:stream_disposition=default,forced",
        )
        .arg("-of")
        .arg("json")
        .arg(path)
        .output()
        .await;

    match output {
        Ok(output) if output.status.success() => {
            match parse_probe_output(&output.stdout, file_extension, probed_at.clone()) {
                Ok(metadata) => metadata,
                Err(error) => probe_outcome_with_error(probed_at, &error),
            }
        }
        Ok(output) => {
            let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
            let error = if stderr.is_empty() {
                format!("ffprobe exited with status {}", output.status)
            } else {
                format!("ffprobe failed: {stderr}")
            };
            probe_outcome_with_error(probed_at, &error)
        }
        Err(error) => {
            probe_outcome_with_error(probed_at, &format!("ffprobe could not start: {error}"))
        }
    }
}

fn probe_outcome_with_error(probed_at: String, error: &str) -> ProbeOutcome {
    ProbeOutcome {
        probed_at,
        probe_error: Some(error.to_string()),
        duration_seconds: None,
        container_name: None,
        video_codec: None,
        audio_codec: None,
        width: None,
        height: None,
        playback_mode: PlaybackMode::Direct,
        video_profile: None,
        video_level: None,
        video_pix_fmt: None,
        video_bit_depth: None,
        audio_streams: Vec::new(),
        subtitle_streams: Vec::new(),
    }
}

pub(crate) fn parse_probe_output(
    output: &[u8],
    file_extension: Option<&str>,
    probed_at: String,
) -> Result<ProbeOutcome, String> {
    let parsed: FfprobeOutput = serde_json::from_slice(output)
        .map_err(|error| format!("ffprobe returned invalid JSON: {error}"))?;

    let video_stream = parsed
        .streams
        .iter()
        .find(|stream| stream.codec_type.as_deref() == Some("video"));

    let mut audio_streams: Vec<AudioStream> = parsed
        .streams
        .iter()
        .filter(|stream| stream.codec_type.as_deref() == Some("audio"))
        .enumerate()
        .map(|(fallback_index, stream)| AudioStream {
            index: stream.index.unwrap_or(fallback_index as u32),
            codec: stream.codec_name.clone(),
            channels: stream.channels,
            channel_layout: stream.channel_layout.clone(),
            language: lookup_tag(&stream.tags, "language"),
            title: lookup_tag(&stream.tags, "title"),
            default: stream.disposition.get("default").copied().unwrap_or(0) != 0,
        })
        .collect();

    if !audio_streams.iter().any(|stream| stream.default)
        && let Some(first) = audio_streams.first_mut()
    {
        first.default = true;
    }

    let subtitle_streams: Vec<SubtitleStream> = parsed
        .streams
        .iter()
        .filter(|stream| stream.codec_type.as_deref() == Some("subtitle"))
        .enumerate()
        .map(|(fallback_index, stream)| SubtitleStream {
            index: stream.index.unwrap_or(fallback_index as u32),
            codec: stream.codec_name.clone(),
            language: lookup_tag(&stream.tags, "language"),
            title: lookup_tag(&stream.tags, "title"),
            default: stream.disposition.get("default").copied().unwrap_or(0) != 0,
            forced: stream.disposition.get("forced").copied().unwrap_or(0) != 0,
        })
        .collect();

    let container_name = parsed
        .format
        .as_ref()
        .and_then(|format| format.format_name.clone());
    let video_profile = video_stream.and_then(|stream| stream.profile.clone());
    let video_level = video_stream
        .and_then(|stream| stream.level)
        .filter(|value| *value > 0)
        .map(|value| value as u32);
    let video_pix_fmt = video_stream.and_then(|stream| stream.pix_fmt.clone());
    let video_bit_depth = video_stream.and_then(|stream| {
        stream
            .bits_per_raw_sample
            .as_ref()
            .and_then(parse_json_u8_like)
    });

    let playback_mode = classify_playback_mode(
        container_name.as_deref(),
        file_extension,
        VideoPlaybackInfo {
            codec: video_stream.and_then(|stream| stream.codec_name.as_deref()),
            profile: video_profile.as_deref(),
            pix_fmt: video_pix_fmt.as_deref(),
            level: video_level,
            bit_depth: video_bit_depth,
        },
        &audio_streams,
    );

    Ok(ProbeOutcome {
        duration_seconds: parsed
            .format
            .as_ref()
            .and_then(|format| format.duration.as_deref())
            .and_then(|duration| duration.parse::<f64>().ok())
            .map(|duration| round_to(duration, 3)),
        container_name,
        video_codec: video_stream.and_then(|stream| stream.codec_name.clone()),
        audio_codec: audio_streams
            .first()
            .and_then(|stream| stream.codec.clone()),
        width: video_stream.and_then(|stream| stream.width),
        height: video_stream.and_then(|stream| stream.height),
        probed_at,
        probe_error: None,
        playback_mode,
        video_profile,
        video_level,
        video_pix_fmt,
        video_bit_depth,
        audio_streams,
        subtitle_streams,
    })
}

fn lookup_tag(tags: &HashMap<String, String>, key: &str) -> Option<String> {
    for (tag_key, tag_value) in tags {
        if tag_key.eq_ignore_ascii_case(key) && !tag_value.trim().is_empty() {
            return Some(tag_value.clone());
        }
    }
    None
}

fn parse_json_u8_like(value: &serde_json::Value) -> Option<u8> {
    match value {
        serde_json::Value::Number(number) => number.as_u64().and_then(|v| u8::try_from(v).ok()),
        serde_json::Value::String(text) => text.parse::<u8>().ok(),
        _ => None,
    }
}
