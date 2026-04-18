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
    Play { position_seconds: Option<f64> },
    Pause { position_seconds: Option<f64> },
    Seek { position_seconds: f64 },
    ReportPosition { position_seconds: f64 },
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
    },
    PlaybackUpdated {
        room: Room,
        actor: String,
        action: PlaybackAction,
    },
    PresenceChanged {
        room_id: Uuid,
        connection_count: usize,
        actor: String,
        joined: bool,
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
    HlsRemux,
    HlsAudioTranscode,
    HlsFullTranscode,
    Unsupported,
}

impl PlaybackMode {
    pub fn as_str(self) -> &'static str {
        match self {
            PlaybackMode::Direct => "direct",
            PlaybackMode::HlsRemux => "hlsRemux",
            PlaybackMode::HlsAudioTranscode => "hlsAudioTranscode",
            PlaybackMode::HlsFullTranscode => "hlsFullTranscode",
            PlaybackMode::Unsupported => "unsupported",
        }
    }

    pub fn from_str_opt(value: &str) -> Option<Self> {
        match value {
            "direct" => Some(PlaybackMode::Direct),
            "hlsRemux" => Some(PlaybackMode::HlsRemux),
            "hlsAudioTranscode" => Some(PlaybackMode::HlsAudioTranscode),
            "hlsFullTranscode" => Some(PlaybackMode::HlsFullTranscode),
            "unsupported" => Some(PlaybackMode::Unsupported),
            _ => None,
        }
    }

    pub fn is_hls(self) -> bool {
        matches!(
            self,
            PlaybackMode::HlsRemux
                | PlaybackMode::HlsAudioTranscode
                | PlaybackMode::HlsFullTranscode
        )
    }
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
    pub video_profile: Option<String>,
    pub video_level: Option<u32>,
    pub video_pix_fmt: Option<String>,
    pub video_bit_depth: Option<u8>,
    pub audio_streams: Vec<AudioStream>,
    pub subtitle_streams: Vec<SubtitleStream>,
    pub hls_master_url: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct LibraryResponse {
    pub roots: Vec<LibraryRoot>,
    pub items: Vec<MediaItem>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct LibraryScanResponse {
    pub roots: Vec<LibraryRoot>,
    pub items: Vec<MediaItem>,
    pub scanned_root_count: usize,
    pub indexed_item_count: usize,
    pub scanned_at: String,
}
