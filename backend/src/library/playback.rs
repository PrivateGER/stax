use crate::protocol::{AudioStream, PlaybackMode};

#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct VideoPlaybackInfo<'a> {
    pub codec: Option<&'a str>,
    pub profile: Option<&'a str>,
    pub pix_fmt: Option<&'a str>,
    pub level: Option<u32>,
    pub bit_depth: Option<u8>,
}

pub(crate) fn classify_playback_mode(
    container: Option<&str>,
    file_extension: Option<&str>,
    video: VideoPlaybackInfo<'_>,
    audio_streams: &[AudioStream],
) -> PlaybackMode {
    let container_ok = container
        .map(|value| is_client_demuxable_container(value, file_extension))
        .unwrap_or(false);
    let has_any_stream = video.codec.is_some() || !audio_streams.is_empty();

    if !has_any_stream {
        return PlaybackMode::Unsupported;
    }

    let audio_ok = audio_streams.is_empty()
        || audio_streams.iter().all(|stream| {
            stream
                .codec
                .as_deref()
                .map(is_client_decodable_audio_codec)
                .unwrap_or(false)
        });

    let audio_transcodable = audio_streams.iter().all(|stream| {
        stream
            .codec
            .as_deref()
            .map(|codec| {
                is_client_decodable_audio_codec(codec) || is_transcodable_audio_codec(codec)
            })
            .unwrap_or(false)
    });

    let video_client_decodable = match video.codec {
        None => true,
        Some(codec) => is_browser_safe_video_codec(
            codec,
            video.profile,
            video.pix_fmt,
            video.level,
            video.bit_depth,
        ),
    };

    if video_client_decodable && audio_ok && container_ok {
        return PlaybackMode::Direct;
    }

    if !audio_transcodable {
        return PlaybackMode::Unsupported;
    }

    PlaybackMode::NeedsPreparation
}

pub(crate) fn is_client_demuxable_container(container: &str, file_extension: Option<&str>) -> bool {
    // ffprobe's format_name is a comma-separated list of equivalent demuxer tags.
    // Critically, "matroska,webm" applies to BOTH .mkv and .webm files. MediaBunny
    // can demux both, but keep trusting the extension when the format tag is ambiguous.
    let normalized_extension = file_extension.map(|value| value.to_ascii_lowercase());
    let tokens = container
        .split(',')
        .map(|token| token.trim().to_ascii_lowercase())
        .collect::<Vec<_>>();

    // Matroska/WebM is not native-HTML-safe, but the MediaBunny player can demux it directly.
    if tokens.iter().any(|token| token == "matroska") {
        return matches!(normalized_extension.as_deref(), Some("mkv" | "webm"));
    }

    tokens.iter().any(|token| {
        matches!(
            token.as_str(),
            "mov"
                | "mp4"
                | "m4a"
                | "m4v"
                | "3gp"
                | "3g2"
                | "mj2"
                | "webm"
                | "mp3"
                | "ogg"
                | "flac"
                | "wav"
        )
    })
}

pub(crate) fn is_browser_safe_video_codec(
    codec: &str,
    profile: Option<&str>,
    pix_fmt: Option<&str>,
    level: Option<u32>,
    bit_depth: Option<u8>,
) -> bool {
    let codec = codec.to_ascii_lowercase();
    match codec.as_str() {
        "h264" | "avc" | "avc1" => {
            let profile_ok = profile
                .map(|value| {
                    let value = value.to_ascii_lowercase();
                    value.contains("baseline") || value.contains("main") || value.contains("high")
                })
                .unwrap_or(true);
            let pix_fmt_ok = pix_fmt
                .map(|value| matches!(value.to_ascii_lowercase().as_str(), "yuv420p" | "yuvj420p"))
                .unwrap_or(true);
            let level_ok = level.map(|value| value <= 51).unwrap_or(true);
            let bit_depth_ok = bit_depth.map(|value| value <= 8).unwrap_or(true);
            profile_ok && pix_fmt_ok && level_ok && bit_depth_ok
        }
        "vp9" | "av1" => pix_fmt
            .map(|value| {
                matches!(
                    value.to_ascii_lowercase().as_str(),
                    "yuv420p" | "yuvj420p" | "yuv420p10le"
                )
            })
            .unwrap_or(true),
        "vp8" => true,
        _ => false,
    }
}

fn is_client_decodable_audio_codec(codec: &str) -> bool {
    let codec = codec.to_ascii_lowercase();
    let normalized = codec.replace('_', "-");

    matches!(
        normalized.as_str(),
        // Native WebCodecs / browser decode targets in the MediaBunny player.
        "aac" | "opus" | "flac" | "vorbis"
            // MP3 is additionally backed by the mpg123 WASM decoder when native
            // WebCodecs support is missing.
            | "mp3"
            // MediaBunny has an internal PCM decoder wrapper for these variants.
            | "pcm-s8"
            | "pcm-u8"
            | "pcm-s16"
            | "pcm-s16le"
            | "pcm-s16be"
            | "pcm-s24"
            | "pcm-s24le"
            | "pcm-s24be"
            | "pcm-s32"
            | "pcm-s32le"
            | "pcm-s32be"
            | "pcm-f32"
            | "pcm-f32le"
            | "pcm-f32be"
            | "pcm-f64"
            | "pcm-f64le"
            | "pcm-f64be"
            | "pcm-mulaw"
            | "pcm-alaw"
            | "ulaw"
            | "alaw"
    )
}

fn is_transcodable_audio_codec(codec: &str) -> bool {
    matches!(
        codec.to_ascii_lowercase().as_str(),
        "ac3" | "eac3" | "dts" | "truehd" | "pcm_s16le" | "pcm_s24le" | "pcm_s32le" | "pcm_f32le"
    )
}

/// Codecs the browser can decode *and* ffmpeg can mux into MP4 — safe to
/// `-c:a copy` in a stream copy. Vorbis and PCM variants are browser-decodable
/// but not standard MP4 audio, so they still need AAC transcode.
pub(crate) fn is_browser_safe_audio_codec_for_mp4(codec: &str) -> bool {
    matches!(
        codec.to_ascii_lowercase().as_str(),
        "aac" | "mp3" | "opus" | "flac"
    )
}
