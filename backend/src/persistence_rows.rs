use sqlx::Row;
use time::{OffsetDateTime, format_description::well_known::Rfc3339};
use uuid::Uuid;

use crate::{
    RoomRecord,
    clock::{AuthoritativePlaybackClock, PlaybackClockCheckpoint},
    persistence::{PersistenceError, StreamCopyRecord},
    protocol::{
        AudioStream, LibraryRoot, MediaItem, MediaSummary, PlaybackMode, PlaybackStatus,
        PreparationState, StreamCopyStatus, SubtitleMode, SubtitleSourceKind, SubtitleStream,
        SubtitleTrack,
    },
};

pub(crate) fn map_row_to_room_record(
    row: sqlx::sqlite::SqliteRow,
) -> Result<RoomRecord, PersistenceError> {
    let room_id = row.try_get::<String, _>("id")?;
    let created_at = row.try_get::<String, _>("created_at")?;
    let clock_updated_at = row.try_get::<String, _>("clock_updated_at")?;
    let playback_status = row.try_get::<String, _>("status")?;
    let raw_media_id = row.try_get::<Option<String>, _>("media_id")?;

    let media_id = match raw_media_id {
        Some(value) => Some(Uuid::parse_str(&value).map_err(|error| {
            PersistenceError::InvalidData(format!(
                "invalid stored room media id '{value}': {error}"
            ))
        })?),
        None => None,
    };

    Ok(RoomRecord {
        id: Uuid::parse_str(&room_id).map_err(|error| {
            PersistenceError::InvalidData(format!("invalid stored room id '{room_id}': {error}"))
        })?,
        name: row.try_get("name")?,
        media_id,
        media_title: row.try_get("media_title")?,
        created_at,
        clock: AuthoritativePlaybackClock::restore(PlaybackClockCheckpoint {
            status: parse_playback_status(&playback_status)?,
            anchor_position_seconds: row.try_get("anchor_position_seconds")?,
            updated_at: parse_timestamp(&clock_updated_at)?,
            playback_rate: row.try_get("playback_rate")?,
        }),
    })
}

pub(crate) fn map_row_to_library_root(
    row: sqlx::sqlite::SqliteRow,
) -> Result<LibraryRoot, PersistenceError> {
    Ok(LibraryRoot {
        path: row.try_get("path")?,
        last_scanned_at: row.try_get("last_scanned_at")?,
        last_scan_error: row.try_get("last_scan_error")?,
    })
}

pub(crate) fn map_row_to_media_summary(
    row: sqlx::sqlite::SqliteRow,
) -> Result<MediaSummary, PersistenceError> {
    let id = row.try_get::<String, _>("id")?;
    let size_bytes = row.try_get::<i64, _>("size_bytes")?;
    let subtitle_tracks_json = row.try_get::<String, _>("subtitle_tracks_json")?;
    let audio_streams_json = row.try_get::<String, _>("audio_streams_json")?;
    let subtitle_streams_json = row.try_get::<String, _>("subtitle_streams_json")?;
    let playback_mode_raw = row.try_get::<String, _>("playback_mode")?;
    let playback_mode = PlaybackMode::from_str_opt(&playback_mode_raw).ok_or_else(|| {
        PersistenceError::InvalidData(format!(
            "invalid stored playback_mode '{playback_mode_raw}'"
        ))
    })?;
    let media_uuid = Uuid::parse_str(&id).map_err(|error| {
        PersistenceError::InvalidData(format!("invalid stored media id '{id}': {error}"))
    })?;
    if size_bytes < 0 {
        return Err(PersistenceError::InvalidData(format!(
            "invalid stored media size '{size_bytes}'"
        )));
    }

    let size_bytes = size_bytes as u64;
    let modified_at = row.try_get::<String, _>("modified_at")?;
    let current_stream_copy = map_joined_stream_copy_record(&row)?.filter(|record| {
        record.source_size_bytes == size_bytes && record.source_modified_at == modified_at
    });
    let preparation_state = preparation_state_for(playback_mode, current_stream_copy.as_ref());
    let subtitle_track_count = deserialize_subtitle_tracks(&subtitle_tracks_json)?.len();
    let audio_stream_count = deserialize_audio_streams(&audio_streams_json)?.len();
    let subtitle_stream_count = deserialize_subtitle_streams(&subtitle_streams_json)?.len();

    Ok(MediaSummary {
        id: media_uuid,
        root_path: row.try_get("root_path")?,
        relative_path: row.try_get("relative_path")?,
        file_name: row.try_get("file_name")?,
        extension: row.try_get("extension")?,
        size_bytes,
        indexed_at: row.try_get("indexed_at")?,
        duration_seconds: row.try_get("duration_seconds")?,
        probe_error: row.try_get("probe_error")?,
        subtitle_track_count,
        audio_stream_count,
        subtitle_stream_count,
        thumbnail_generated_at: row.try_get("thumbnail_generated_at")?,
        thumbnail_error: row.try_get("thumbnail_error")?,
        preparation_state,
    })
}

pub(crate) fn map_row_to_media_item(
    row: sqlx::sqlite::SqliteRow,
) -> Result<MediaItem, PersistenceError> {
    let id = row.try_get::<String, _>("id")?;
    let size_bytes = row.try_get::<i64, _>("size_bytes")?;
    let subtitle_tracks_json = row.try_get::<String, _>("subtitle_tracks_json")?;
    let audio_streams_json = row.try_get::<String, _>("audio_streams_json")?;
    let subtitle_streams_json = row.try_get::<String, _>("subtitle_streams_json")?;
    let playback_mode_raw = row.try_get::<String, _>("playback_mode")?;
    let playback_mode = PlaybackMode::from_str_opt(&playback_mode_raw).ok_or_else(|| {
        PersistenceError::InvalidData(format!(
            "invalid stored playback_mode '{playback_mode_raw}'"
        ))
    })?;
    let media_uuid = Uuid::parse_str(&id).map_err(|error| {
        PersistenceError::InvalidData(format!("invalid stored media id '{id}': {error}"))
    })?;
    if size_bytes < 0 {
        return Err(PersistenceError::InvalidData(format!(
            "invalid stored media size '{size_bytes}'"
        )));
    }

    let size_bytes = size_bytes as u64;
    let modified_at = row.try_get::<String, _>("modified_at")?;
    let current_stream_copy = map_joined_stream_copy_record(&row)?.filter(|record| {
        record.source_size_bytes == size_bytes && record.source_modified_at == modified_at
    });
    let preparation_state = preparation_state_for(playback_mode, current_stream_copy.as_ref());
    let stream_copy = if playback_mode == PlaybackMode::NeedsPreparation {
        current_stream_copy
            .as_ref()
            .map(|record| super::persistence::stream_copy_summary_for(media_uuid, record))
    } else {
        None
    };

    Ok(MediaItem {
        id: media_uuid,
        root_path: row.try_get("root_path")?,
        relative_path: row.try_get("relative_path")?,
        file_name: row.try_get("file_name")?,
        extension: row.try_get("extension")?,
        size_bytes,
        modified_at,
        indexed_at: row.try_get("indexed_at")?,
        content_type: row.try_get("content_type")?,
        duration_seconds: row.try_get("duration_seconds")?,
        container_name: row.try_get("container_name")?,
        video_codec: row.try_get("video_codec")?,
        audio_codec: row.try_get("audio_codec")?,
        width: parse_optional_u32(&row, "width")?,
        height: parse_optional_u32(&row, "height")?,
        probed_at: row.try_get("probed_at")?,
        probe_error: row.try_get("probe_error")?,
        subtitle_tracks: deserialize_subtitle_tracks(&subtitle_tracks_json)?,
        thumbnail_generated_at: row.try_get("thumbnail_generated_at")?,
        thumbnail_error: row.try_get("thumbnail_error")?,
        playback_mode,
        preparation_state,
        video_profile: row.try_get("video_profile")?,
        video_level: parse_optional_u32(&row, "video_level")?,
        video_pix_fmt: row.try_get("video_pix_fmt")?,
        video_bit_depth: parse_optional_u8(&row, "video_bit_depth")?,
        audio_streams: deserialize_audio_streams(&audio_streams_json)?,
        subtitle_streams: deserialize_subtitle_streams(&subtitle_streams_json)?,
        stream_copy,
    })
}

fn preparation_state_for(
    playback_mode: PlaybackMode,
    current_stream_copy: Option<&StreamCopyRecord>,
) -> PreparationState {
    match playback_mode {
        PlaybackMode::Direct => PreparationState::Direct,
        PlaybackMode::Unsupported => PreparationState::Unsupported,
        PlaybackMode::NeedsPreparation => match current_stream_copy {
            Some(record) => match record.status {
                StreamCopyStatus::Queued | StreamCopyStatus::Running => PreparationState::Preparing,
                StreamCopyStatus::Ready => PreparationState::Prepared,
                StreamCopyStatus::Failed => PreparationState::Failed,
            },
            None => PreparationState::NeedsPreparation,
        },
    }
}

fn map_joined_stream_copy_record(
    row: &sqlx::sqlite::SqliteRow,
) -> Result<Option<StreamCopyRecord>, PersistenceError> {
    let Some(status_raw) = row.try_get::<Option<String>, _>("stream_copy_status")? else {
        return Ok(None);
    };
    let Some(source_size_bytes) = row.try_get::<Option<i64>, _>("stream_copy_source_size_bytes")?
    else {
        return Err(PersistenceError::InvalidData(
            "stream copy row missing source size".to_string(),
        ));
    };
    if source_size_bytes < 0 {
        return Err(PersistenceError::InvalidData(format!(
            "invalid stored stream copy source size '{source_size_bytes}'"
        )));
    }
    let media_id = row.try_get::<String, _>("id")?;
    let media_id = Uuid::parse_str(&media_id).map_err(|error| {
        PersistenceError::InvalidData(format!("invalid stored media id '{media_id}': {error}"))
    })?;
    let status = StreamCopyStatus::from_str_opt(&status_raw).ok_or_else(|| {
        PersistenceError::InvalidData(format!("invalid stream copy status '{status_raw}'"))
    })?;
    let subtitle_mode_raw = row.try_get::<Option<String>, _>("stream_copy_subtitle_mode")?;
    let subtitle_mode = subtitle_mode_raw
        .as_deref()
        .and_then(SubtitleMode::from_str_opt)
        .ok_or_else(|| {
            PersistenceError::InvalidData("stream copy row missing subtitle mode".to_string())
        })?;
    let subtitle_kind = match row.try_get::<Option<String>, _>("stream_copy_subtitle_kind")? {
        Some(raw) => Some(SubtitleSourceKind::from_str_opt(&raw).ok_or_else(|| {
            PersistenceError::InvalidData(format!("invalid stream copy subtitle kind '{raw}'"))
        })?),
        None => None,
    };

    Ok(Some(StreamCopyRecord {
        media_id,
        source_size_bytes: source_size_bytes as u64,
        source_modified_at: row
            .try_get::<Option<String>, _>("stream_copy_source_modified_at")?
            .ok_or_else(|| {
                PersistenceError::InvalidData(
                    "stream copy row missing source modified_at".to_string(),
                )
            })?,
        status,
        audio_stream_index: parse_optional_u32(row, "stream_copy_audio_stream_index")?,
        subtitle_mode,
        subtitle_kind,
        subtitle_index: parse_optional_u32(row, "stream_copy_subtitle_index")?,
        output_path: row.try_get("stream_copy_output_path")?,
        output_content_type: row.try_get("stream_copy_output_content_type")?,
        subtitle_path: row.try_get("stream_copy_subtitle_path")?,
        error: row.try_get("stream_copy_error")?,
        created_at: row
            .try_get::<Option<String>, _>("stream_copy_created_at")?
            .ok_or_else(|| {
                PersistenceError::InvalidData("stream copy row missing created_at".to_string())
            })?,
        updated_at: row
            .try_get::<Option<String>, _>("stream_copy_updated_at")?
            .ok_or_else(|| {
                PersistenceError::InvalidData("stream copy row missing updated_at".to_string())
            })?,
    }))
}

pub(crate) fn map_row_to_stream_copy_record(
    row: sqlx::sqlite::SqliteRow,
) -> Result<StreamCopyRecord, PersistenceError> {
    let media_id = row.try_get::<String, _>("media_id")?;
    let media_id = Uuid::parse_str(&media_id).map_err(|error| {
        PersistenceError::InvalidData(format!(
            "invalid stored stream copy media id '{media_id}': {error}"
        ))
    })?;
    let source_size_bytes = row.try_get::<i64, _>("source_size_bytes")?;
    if source_size_bytes < 0 {
        return Err(PersistenceError::InvalidData(format!(
            "invalid stored stream copy source size '{source_size_bytes}'"
        )));
    }
    let status_raw = row.try_get::<String, _>("status")?;
    let status = StreamCopyStatus::from_str_opt(&status_raw).ok_or_else(|| {
        PersistenceError::InvalidData(format!("invalid stream copy status '{status_raw}'"))
    })?;
    let subtitle_mode_raw = row.try_get::<String, _>("subtitle_mode")?;
    let subtitle_mode = SubtitleMode::from_str_opt(&subtitle_mode_raw).ok_or_else(|| {
        PersistenceError::InvalidData(format!(
            "invalid stream copy subtitle mode '{subtitle_mode_raw}'"
        ))
    })?;
    let subtitle_kind = match row.try_get::<Option<String>, _>("subtitle_kind")? {
        Some(raw) => Some(SubtitleSourceKind::from_str_opt(&raw).ok_or_else(|| {
            PersistenceError::InvalidData(format!("invalid stream copy subtitle kind '{raw}'"))
        })?),
        None => None,
    };

    Ok(StreamCopyRecord {
        media_id,
        source_size_bytes: source_size_bytes as u64,
        source_modified_at: row.try_get("source_modified_at")?,
        status,
        audio_stream_index: parse_optional_u32(&row, "audio_stream_index")?,
        subtitle_mode,
        subtitle_kind,
        subtitle_index: parse_optional_u32(&row, "subtitle_index")?,
        output_path: row.try_get("output_path")?,
        output_content_type: row.try_get("output_content_type")?,
        subtitle_path: row.try_get("subtitle_path")?,
        error: row.try_get("error")?,
        created_at: row.try_get("created_at")?,
        updated_at: row.try_get("updated_at")?,
    })
}

pub(crate) fn serialize_audio_streams(streams: &[AudioStream]) -> Result<String, PersistenceError> {
    serde_json::to_string(streams).map_err(|error| {
        PersistenceError::InvalidData(format!("failed to serialize audio streams: {error}"))
    })
}

pub(crate) fn deserialize_audio_streams(value: &str) -> Result<Vec<AudioStream>, PersistenceError> {
    serde_json::from_str(value).map_err(|error| {
        PersistenceError::InvalidData(format!("invalid stored audio stream JSON: {error}"))
    })
}

pub(crate) fn serialize_subtitle_streams(
    streams: &[SubtitleStream],
) -> Result<String, PersistenceError> {
    serde_json::to_string(streams).map_err(|error| {
        PersistenceError::InvalidData(format!("failed to serialize subtitle streams: {error}"))
    })
}

pub(crate) fn deserialize_subtitle_streams(
    value: &str,
) -> Result<Vec<SubtitleStream>, PersistenceError> {
    serde_json::from_str(value).map_err(|error| {
        PersistenceError::InvalidData(format!("invalid stored subtitle stream JSON: {error}"))
    })
}

pub(crate) fn parse_optional_u8(
    row: &sqlx::sqlite::SqliteRow,
    column: &str,
) -> Result<Option<u8>, PersistenceError> {
    let value = row.try_get::<Option<i64>, _>(column)?;

    match value {
        Some(value) if value < 0 => Err(PersistenceError::InvalidData(format!(
            "invalid stored {column} '{value}'"
        ))),
        Some(value) => u8::try_from(value).map(Some).map_err(|error| {
            PersistenceError::InvalidData(format!("invalid stored {column} '{value}': {error}"))
        }),
        None => Ok(None),
    }
}

pub(crate) fn serialize_subtitle_tracks(
    tracks: &[SubtitleTrack],
) -> Result<String, PersistenceError> {
    serde_json::to_string(tracks).map_err(|error| {
        PersistenceError::InvalidData(format!("failed to serialize subtitle tracks: {error}"))
    })
}

pub(crate) fn deserialize_subtitle_tracks(
    value: &str,
) -> Result<Vec<SubtitleTrack>, PersistenceError> {
    serde_json::from_str(value).map_err(|error| {
        PersistenceError::InvalidData(format!("invalid stored subtitle track JSON: {error}"))
    })
}

pub(crate) fn parse_optional_u32(
    row: &sqlx::sqlite::SqliteRow,
    column: &str,
) -> Result<Option<u32>, PersistenceError> {
    let value = row.try_get::<Option<i64>, _>(column)?;

    match value {
        Some(value) if value < 0 => Err(PersistenceError::InvalidData(format!(
            "invalid stored {column} '{value}'"
        ))),
        Some(value) => u32::try_from(value).map(Some).map_err(|error| {
            PersistenceError::InvalidData(format!("invalid stored {column} '{value}': {error}"))
        }),
        None => Ok(None),
    }
}

pub(crate) fn playback_status_to_str(status: PlaybackStatus) -> &'static str {
    match status {
        PlaybackStatus::Playing => "playing",
        PlaybackStatus::Paused => "paused",
    }
}

fn parse_playback_status(value: &str) -> Result<PlaybackStatus, PersistenceError> {
    match value {
        "playing" => Ok(PlaybackStatus::Playing),
        "paused" => Ok(PlaybackStatus::Paused),
        other => Err(PersistenceError::InvalidData(format!(
            "invalid stored playback status '{other}'"
        ))),
    }
}

fn parse_timestamp(value: &str) -> Result<OffsetDateTime, PersistenceError> {
    OffsetDateTime::parse(value, &Rfc3339).map_err(|error| {
        PersistenceError::InvalidData(format!(
            "invalid stored RFC3339 timestamp '{value}': {error}"
        ))
    })
}
