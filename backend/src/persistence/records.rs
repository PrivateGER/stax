use uuid::Uuid;

use crate::protocol::{
    AudioStream, LibraryRoot, MediaSummary, PlaybackMode, StreamCopyStatus, SubtitleMode,
    SubtitleSourceKind, SubtitleStream, SubtitleTrack,
};

#[derive(Debug, Clone)]
pub(crate) struct LibrarySnapshot {
    pub roots: Vec<LibraryRoot>,
    pub items: Vec<MediaSummary>,
}

#[derive(Debug, Clone)]
pub(crate) struct LibraryStatusSnapshot {
    pub revision: u64,
    pub has_pending_background_work: bool,
}

/// Minimal media-item snapshot the thumbnail worker needs to schedule a job.
/// Kept narrow on purpose so the worker pool doesn't pull the entire
/// `MediaItem` shape (with its serialized stream/subtitle blobs) into memory
/// for every queued file.
#[derive(Debug, Clone)]
pub struct PendingThumbnail {
    pub media_id: Uuid,
    pub root_path: String,
    pub relative_path: String,
    pub video_codec: Option<String>,
    pub duration_seconds: Option<f64>,
}

/// All probe-derived state the scanner needs to short-circuit re-probing
/// when a file hasn't changed on disk. The scanner compares
/// `(size_bytes, modified_at)` against fresh filesystem metadata; if both
/// match and `probe_error` is `None`, every other field can be reused
/// verbatim and ffprobe is skipped entirely.
#[derive(Debug, Clone)]
pub struct CachedMediaRecord {
    pub id: Uuid,
    pub size_bytes: u64,
    pub modified_at: String,
    pub duration_seconds: Option<f64>,
    pub container_name: Option<String>,
    pub video_codec: Option<String>,
    pub audio_codec: Option<String>,
    pub width: Option<u32>,
    pub height: Option<u32>,
    pub probed_at: Option<String>,
    pub probe_error: Option<String>,
    pub playback_mode: PlaybackMode,
    pub video_profile: Option<String>,
    pub video_level: Option<u32>,
    pub video_pix_fmt: Option<String>,
    pub video_bit_depth: Option<u8>,
    pub audio_streams: Vec<AudioStream>,
    pub subtitle_streams: Vec<SubtitleStream>,
}

/// One probed file's worth of metadata, ready to be written back to the
/// `media_items` row by the background probe pool. Mirrors the columns
/// populated during the probe phase of a scan.
#[derive(Debug, Clone)]
pub struct ProbeOutcome {
    pub probed_at: String,
    pub probe_error: Option<String>,
    pub duration_seconds: Option<f64>,
    pub container_name: Option<String>,
    pub video_codec: Option<String>,
    pub audio_codec: Option<String>,
    pub width: Option<u32>,
    pub height: Option<u32>,
    pub playback_mode: PlaybackMode,
    pub video_profile: Option<String>,
    pub video_level: Option<u32>,
    pub video_pix_fmt: Option<String>,
    pub video_bit_depth: Option<u8>,
    pub audio_streams: Vec<AudioStream>,
    pub subtitle_streams: Vec<SubtitleStream>,
}

/// One row's worth of fields the walker writes per file, plus an optional
/// probe carry-over for cache-hit rows. Cache-miss rows pass `cached_probe
/// = None` and the upsert writes NULL probe columns + reset thumbnail
/// state, queueing the row for the background probe pool.
#[derive(Debug, Clone)]
pub struct WalkRecord {
    pub id: Uuid,
    pub root_path: String,
    pub relative_path: String,
    pub file_name: String,
    pub extension: Option<String>,
    pub size_bytes: u64,
    pub modified_at: String,
    pub indexed_at: String,
    pub content_type: Option<String>,
    pub subtitle_tracks: Vec<SubtitleTrack>,
    /// `Some` iff `(size, mtime)` matches a previously-probed row that
    /// did not error - the cached probe fields are preserved verbatim.
    pub cached_probe: Option<CachedProbeFields>,
    /// Cached thumbnail outcome from the previous scan (only honored
    /// alongside `cached_probe`). `(generated_at, error)`.
    pub cached_thumbnail: Option<(Option<String>, Option<String>)>,
}

#[derive(Debug, Clone)]
pub struct CachedProbeFields {
    pub probed_at: Option<String>,
    pub probe_error: Option<String>,
    pub duration_seconds: Option<f64>,
    pub container_name: Option<String>,
    pub video_codec: Option<String>,
    pub audio_codec: Option<String>,
    pub width: Option<u32>,
    pub height: Option<u32>,
    pub playback_mode: PlaybackMode,
    pub video_profile: Option<String>,
    pub video_level: Option<u32>,
    pub video_pix_fmt: Option<String>,
    pub video_bit_depth: Option<u8>,
    pub audio_streams: Vec<AudioStream>,
    pub subtitle_streams: Vec<SubtitleStream>,
}

/// Minimal media-item snapshot the probe pool needs to schedule a job.
#[derive(Debug, Clone)]
pub struct PendingProbe {
    pub media_id: Uuid,
    pub root_path: String,
    pub relative_path: String,
    pub extension: Option<String>,
}

#[derive(Debug, Clone)]
pub struct StreamCopyRecord {
    pub media_id: Uuid,
    pub source_size_bytes: u64,
    pub source_modified_at: String,
    pub status: StreamCopyStatus,
    pub audio_stream_index: Option<u32>,
    pub subtitle_mode: SubtitleMode,
    pub subtitle_kind: Option<SubtitleSourceKind>,
    pub subtitle_index: Option<u32>,
    pub output_path: Option<String>,
    pub output_content_type: Option<String>,
    pub subtitle_path: Option<String>,
    pub error: Option<String>,
    pub created_at: String,
    pub updated_at: String,
}

#[derive(Debug, Clone)]
pub struct StreamCopyRequestRecord {
    pub media_id: Uuid,
    pub source_size_bytes: u64,
    pub source_modified_at: String,
    pub audio_stream_index: Option<u32>,
    pub subtitle_mode: SubtitleMode,
    pub subtitle_kind: Option<SubtitleSourceKind>,
    pub subtitle_index: Option<u32>,
    pub updated_at: String,
}

#[derive(Debug, Clone)]
pub struct PendingStreamCopy {
    pub media_id: Uuid,
}
