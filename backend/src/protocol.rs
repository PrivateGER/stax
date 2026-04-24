use serde::{Deserialize, Serialize};
use uuid::Uuid;

pub const DRIFT_TOLERANCE_SECONDS: f64 = 0.35;
pub const HARD_DRIFT_THRESHOLD_SECONDS: f64 = 1.5;

#[derive(Debug, Clone, Serialize)]
pub struct HealthResponse {
    pub status: &'static str,
    pub service: &'static str,
    pub version: &'static str,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Room {
    pub id: Uuid,
    pub name: String,
    pub media_id: Option<Uuid>,
    pub media_title: Option<String>,
    pub playback_state: PlaybackState,
    pub created_at: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PlaybackState {
    pub status: PlaybackStatus,
    pub position_seconds: f64,
    pub anchor_position_seconds: f64,
    pub clock_updated_at: String,
    pub emitted_at: String,
    pub playback_rate: f64,
    pub drift_tolerance_seconds: f64,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum PlaybackStatus {
    Playing,
    Paused,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum PlaybackAction {
    Play,
    Pause,
    Seek,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum DriftCorrectionAction {
    InSync,
    Nudge,
    Seek,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct Participant {
    pub id: Uuid,
    pub name: String,
    pub drift_seconds: Option<f64>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CreateRoomRequest {
    pub name: String,
    pub media_id: Option<Uuid>,
    pub media_title: Option<String>,
}

#[derive(Debug, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct RoomSocketQuery {
    pub client_name: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(
    tag = "type",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
pub enum ClientSocketMessage {
    Play {
        position_seconds: Option<f64>,
        client_one_way_ms: Option<u32>,
    },
    Pause {
        position_seconds: Option<f64>,
        client_one_way_ms: Option<u32>,
    },
    Seek {
        position_seconds: f64,
        client_one_way_ms: Option<u32>,
    },
    SelectMedia {
        media_id: Uuid,
    },
    ReportPosition {
        position_seconds: f64,
    },
    Ping {
        client_sent_at_ms: f64,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(
    tag = "type",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
pub enum ServerEvent {
    Snapshot {
        room: Room,
        connection_count: usize,
        participants: Vec<Participant>,
    },
    PlaybackUpdated {
        room: Room,
        actor: String,
        action: PlaybackAction,
    },
    MediaChanged {
        room: Room,
        actor: String,
    },
    PresenceChanged {
        room_id: Uuid,
        connection_count: usize,
        actor: String,
        joined: bool,
    },
    ParticipantsUpdated {
        room_id: Uuid,
        participants: Vec<Participant>,
    },
    DriftCorrection {
        room_id: Uuid,
        actor: String,
        reported_position_seconds: f64,
        expected_position_seconds: f64,
        delta_seconds: f64,
        tolerance_seconds: f64,
        suggested_action: DriftCorrectionAction,
        measured_at: String,
    },
    Error {
        message: String,
    },
    Pong {
        client_sent_at_ms: f64,
    },
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RoomsResponse {
    pub rooms: Vec<Room>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct LibraryRoot {
    pub path: String,
    pub last_scanned_at: Option<String>,
    pub last_scan_error: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct SubtitleTrack {
    pub file_name: String,
    pub relative_path: String,
    pub extension: String,
    pub label: String,
    pub language: Option<String>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum PlaybackMode {
    Direct,
    NeedsPreparation,
    Unsupported,
}

impl PlaybackMode {
    pub fn as_str(self) -> &'static str {
        match self {
            PlaybackMode::Direct => "direct",
            PlaybackMode::NeedsPreparation => "needsPreparation",
            PlaybackMode::Unsupported => "unsupported",
        }
    }

    pub fn from_str_opt(value: &str) -> Option<Self> {
        match value {
            "direct" => Some(PlaybackMode::Direct),
            "needsPreparation" => Some(PlaybackMode::NeedsPreparation),
            "unsupported" => Some(PlaybackMode::Unsupported),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum PreparationState {
    Direct,
    NeedsPreparation,
    Preparing,
    Prepared,
    Failed,
    Unsupported,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum StreamCopyStatus {
    Queued,
    Running,
    Ready,
    Failed,
}

impl StreamCopyStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            StreamCopyStatus::Queued => "queued",
            StreamCopyStatus::Running => "running",
            StreamCopyStatus::Ready => "ready",
            StreamCopyStatus::Failed => "failed",
        }
    }

    pub fn from_str_opt(value: &str) -> Option<Self> {
        match value {
            "queued" => Some(StreamCopyStatus::Queued),
            "running" => Some(StreamCopyStatus::Running),
            "ready" => Some(StreamCopyStatus::Ready),
            "failed" => Some(StreamCopyStatus::Failed),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum SubtitleMode {
    Off,
    Sidecar,
    Burned,
}

impl SubtitleMode {
    pub fn as_str(self) -> &'static str {
        match self {
            SubtitleMode::Off => "off",
            SubtitleMode::Sidecar => "sidecar",
            SubtitleMode::Burned => "burned",
        }
    }

    pub fn from_str_opt(value: &str) -> Option<Self> {
        match value {
            "off" => Some(SubtitleMode::Off),
            "sidecar" => Some(SubtitleMode::Sidecar),
            "burned" => Some(SubtitleMode::Burned),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum SubtitleSourceKind {
    Sidecar,
    Embedded,
}

impl SubtitleSourceKind {
    pub fn as_str(self) -> &'static str {
        match self {
            SubtitleSourceKind::Sidecar => "sidecar",
            SubtitleSourceKind::Embedded => "embedded",
        }
    }

    pub fn from_str_opt(value: &str) -> Option<Self> {
        match value {
            "sidecar" => Some(SubtitleSourceKind::Sidecar),
            "embedded" => Some(SubtitleSourceKind::Embedded),
            _ => None,
        }
    }
}

pub(crate) fn is_text_subtitle_codec(codec: Option<&str>) -> bool {
    matches!(
        codec.unwrap_or_default().to_ascii_lowercase().as_str(),
        "ass" | "ssa" | "subrip" | "srt" | "webvtt" | "mov_text" | "text"
    )
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct StreamCopySubtitleSelection {
    pub kind: SubtitleSourceKind,
    pub index: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct StreamCopySummary {
    pub status: StreamCopyStatus,
    pub audio_stream_index: Option<u32>,
    pub subtitle_mode: SubtitleMode,
    pub subtitle: Option<StreamCopySubtitleSelection>,
    pub subtitle_url: Option<String>,
    pub error: Option<String>,
    pub progress_ratio: Option<f32>,
    pub progress_speed: Option<f32>,
    pub updated_at: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct CreateStreamCopyRequest {
    pub audio_stream_index: Option<u32>,
    pub subtitle_mode: SubtitleMode,
    pub subtitle: Option<StreamCopySubtitleSelection>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct AudioStream {
    pub index: u32,
    pub codec: Option<String>,
    pub channels: Option<u32>,
    pub channel_layout: Option<String>,
    pub language: Option<String>,
    pub title: Option<String>,
    pub default: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct SubtitleStream {
    pub index: u32,
    pub codec: Option<String>,
    pub language: Option<String>,
    pub title: Option<String>,
    pub default: bool,
    pub forced: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct MediaItem {
    pub id: Uuid,
    pub root_path: String,
    pub relative_path: String,
    pub file_name: String,
    pub extension: Option<String>,
    pub size_bytes: u64,
    pub modified_at: String,
    pub indexed_at: String,
    pub content_type: Option<String>,
    pub duration_seconds: Option<f64>,
    pub container_name: Option<String>,
    pub video_codec: Option<String>,
    pub audio_codec: Option<String>,
    pub width: Option<u32>,
    pub height: Option<u32>,
    pub probed_at: Option<String>,
    pub probe_error: Option<String>,
    pub subtitle_tracks: Vec<SubtitleTrack>,
    pub thumbnail_generated_at: Option<String>,
    pub thumbnail_error: Option<String>,
    pub playback_mode: PlaybackMode,
    pub preparation_state: PreparationState,
    pub video_profile: Option<String>,
    pub video_level: Option<u32>,
    pub video_pix_fmt: Option<String>,
    pub video_bit_depth: Option<u8>,
    pub audio_streams: Vec<AudioStream>,
    pub subtitle_streams: Vec<SubtitleStream>,
    pub stream_copy: Option<StreamCopySummary>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct MediaSummary {
    pub id: Uuid,
    pub root_path: String,
    pub relative_path: String,
    pub file_name: String,
    pub extension: Option<String>,
    pub size_bytes: u64,
    pub indexed_at: String,
    pub duration_seconds: Option<f64>,
    pub probe_error: Option<String>,
    pub subtitle_track_count: usize,
    pub audio_stream_count: usize,
    pub subtitle_stream_count: usize,
    pub thumbnail_generated_at: Option<String>,
    pub thumbnail_error: Option<String>,
    pub preparation_state: PreparationState,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct LibraryResponse {
    pub revision: u64,
    pub has_pending_background_work: bool,
    pub roots: Vec<LibraryRoot>,
    pub items: Vec<MediaSummary>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct LibraryScanResponse {
    pub revision: u64,
    pub has_pending_background_work: bool,
    pub roots: Vec<LibraryRoot>,
    pub items: Vec<MediaSummary>,
    pub scanned_root_count: usize,
    pub indexed_item_count: usize,
    pub scanned_at: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct LibraryStatusResponse {
    pub revision: u64,
    pub has_pending_background_work: bool,
}

#[cfg(test)]
mod tests {
    use super::is_text_subtitle_codec;

    #[test]
    fn text_subtitle_codec_detection_is_case_insensitive() {
        assert!(is_text_subtitle_codec(Some("SubRip")));
        assert!(is_text_subtitle_codec(Some("MOV_TEXT")));
    }

    #[test]
    fn text_subtitle_codec_detection_rejects_bitmap_and_missing_codecs() {
        assert!(!is_text_subtitle_codec(Some("hdmv_pgs_subtitle")));
        assert!(!is_text_subtitle_codec(None));
    }
}
