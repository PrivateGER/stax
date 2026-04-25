use axum::{
    Json,
    extract::{Path, State},
    http::{HeaderMap, StatusCode, header::RANGE},
    response::Response,
};
use time::OffsetDateTime;
use tracing::warn;
use uuid::Uuid;

use crate::{
    AppState,
    api::error::ApiError,
    clock::format_timestamp,
    persistence::{StreamCopyRecord, StreamCopyRequestRecord},
    protocol::{
        CreateStreamCopyRequest, MediaItem, PlaybackMode, StreamCopyStatus, StreamCopySummary,
        SubtitleMode, SubtitleSourceKind, is_text_subtitle_codec,
    },
    stream_copies::StreamCopyJob,
    streaming::{
        StreamMediaError, stream_embedded_subtitle_response, stream_file_response,
        stream_media_response, stream_subtitle_response, stream_thumbnail_response,
        stream_webvtt_file_response, unsatisfiable_range_response,
    },
};

pub(crate) async fn get_media(
    State(state): State<AppState>,
    Path(media_id): Path<Uuid>,
) -> Result<Json<MediaItem>, ApiError> {
    let media_item =
        load_media_item(&state, media_id, "get_media", "Failed to load media.").await?;

    Ok(Json(media_item))
}

pub(crate) async fn stream_media(
    State(state): State<AppState>,
    Path(media_id): Path<Uuid>,
    headers: HeaderMap,
) -> Result<Response, ApiError> {
    let range_header = match headers.get(RANGE) {
        Some(value) => match value.to_str() {
            Ok(value) => Some(value),
            Err(_) => {
                return Err(ApiError::bad_request("Range header must be valid ASCII."));
            }
        },
        None => None,
    };

    let media_item =
        load_media_item(&state, media_id, "stream_media", "Failed to load media.").await?;

    let stream_result = match media_item.playback_mode {
        PlaybackMode::Direct => stream_media_response(&media_item, range_header).await,
        PlaybackMode::NeedsPreparation => {
            let Some(record) = load_current_stream_copy_record(&state, &media_item).await? else {
                return Err(ApiError::with_status(
                    StatusCode::CONFLICT,
                    playback_unavailable_message(&media_item),
                ));
            };
            if record.status != StreamCopyStatus::Ready {
                return Err(ApiError::with_status(
                    StatusCode::CONFLICT,
                    playback_unavailable_message(&media_item),
                ));
            }
            let Some(output_path) = record.output_path.as_deref() else {
                return Err(ApiError::internal(
                    "Prepared stream copy is missing an output file.",
                ));
            };
            let content_type = record
                .output_content_type
                .as_deref()
                .unwrap_or("application/octet-stream");
            stream_file_response(
                std::path::Path::new(output_path),
                content_type,
                range_header,
            )
            .await
        }
        PlaybackMode::Unsupported => {
            return Err(ApiError::with_status(
                StatusCode::UNSUPPORTED_MEDIA_TYPE,
                "This media is not supported for browser playback.",
            ));
        }
    };

    match stream_result {
        Ok(response) => Ok(response),
        Err(StreamMediaError::NotFound) => Err(ApiError::not_found("Media file not found.")),
        Err(StreamMediaError::MalformedRange(message)) => Err(ApiError::bad_request(message)),
        Err(StreamMediaError::UnsatisfiableRange { file_len }) => {
            Ok(unsatisfiable_range_response(file_len))
        }
        Err(StreamMediaError::Io(error)) => {
            warn!(%error, media_id = %media_item.id, "failed to stream media");
            Err(ApiError::internal("Failed to stream media."))
        }
    }
}

pub(crate) async fn stream_thumbnail(
    State(state): State<AppState>,
    Path(media_id): Path<Uuid>,
) -> Result<Response, ApiError> {
    let media_item = load_media_item(
        &state,
        media_id,
        "stream_thumbnail",
        "Failed to load media.",
    )
    .await?;

    if media_item.thumbnail_generated_at.is_none() {
        return Err(ApiError::not_found("Thumbnail not available."));
    }

    let Some(thumbnail_path) = state.library.thumbnail_path(media_item.id) else {
        return Err(ApiError::not_found("Thumbnail not available."));
    };

    match stream_thumbnail_response(&thumbnail_path).await {
        Ok(response) => Ok(response),
        Err(StreamMediaError::NotFound) => Err(ApiError::not_found("Thumbnail not available.")),
        Err(StreamMediaError::Io(error)) => {
            warn!(%error, media_id = %media_item.id, "failed to stream thumbnail");
            Err(ApiError::internal("Failed to stream thumbnail."))
        }
        Err(StreamMediaError::MalformedRange(_) | StreamMediaError::UnsatisfiableRange { .. }) => {
            Err(ApiError::internal("Failed to stream thumbnail."))
        }
    }
}

pub(crate) async fn get_stream_copy(
    State(state): State<AppState>,
    Path(media_id): Path<Uuid>,
) -> Result<Json<StreamCopySummary>, ApiError> {
    let media_item =
        load_media_item(&state, media_id, "get_stream_copy", "Failed to load media.").await?;

    let Some(record) = load_current_stream_copy_record(&state, &media_item).await? else {
        return Err(ApiError::not_found(
            "No current stream copy exists for this media.",
        ));
    };

    let summary = state
        .stream_copies
        .summary_for(media_item.id, media_item.duration_seconds, &record)
        .await;

    Ok(Json(summary))
}

pub(crate) async fn create_stream_copy(
    State(state): State<AppState>,
    Path(media_id): Path<Uuid>,
    Json(request): Json<CreateStreamCopyRequest>,
) -> Result<Json<StreamCopySummary>, ApiError> {
    let media_item = load_media_item(
        &state,
        media_id,
        "create_stream_copy",
        "Failed to load media.",
    )
    .await?;

    if media_item.playback_mode == PlaybackMode::Direct {
        return Err(ApiError::with_status(
            StatusCode::CONFLICT,
            "This media is already browser-playable and does not need a stream copy.",
        ));
    }
    if media_item.playback_mode == PlaybackMode::Unsupported {
        return Err(ApiError::with_status(
            StatusCode::UNSUPPORTED_MEDIA_TYPE,
            "This media is not supported for browser playback.",
        ));
    }

    validate_stream_copy_request(&media_item, &request)?;

    if let Some(existing) = load_current_stream_copy_record(&state, &media_item).await? {
        let same_request = stream_copy_request_matches(&existing, &request);
        if same_request
            && matches!(
                existing.status,
                StreamCopyStatus::Queued | StreamCopyStatus::Running | StreamCopyStatus::Ready
            )
        {
            let summary = state
                .stream_copies
                .summary_for(media_item.id, media_item.duration_seconds, &existing)
                .await;
            return Ok(Json(summary));
        }
        if !same_request
            && matches!(
                existing.status,
                StreamCopyStatus::Queued | StreamCopyStatus::Running
            )
        {
            return Err(ApiError::with_status(
                StatusCode::CONFLICT,
                "A different stream copy request is already in progress for this media.",
            ));
        }
    }

    let now = format_timestamp(OffsetDateTime::now_utc());
    let subtitle_kind = request.subtitle.as_ref().map(|subtitle| subtitle.kind);
    let subtitle_index = request.subtitle.as_ref().map(|subtitle| subtitle.index);
    state
        .persistence
        .upsert_stream_copy_request(&StreamCopyRequestRecord {
            media_id,
            source_size_bytes: media_item.size_bytes,
            source_modified_at: media_item.modified_at.clone(),
            audio_stream_index: request.audio_stream_index,
            subtitle_mode: request.subtitle_mode,
            subtitle_kind,
            subtitle_index,
            updated_at: now,
        })
        .await
        .map_err(|error| {
            warn!(%error, %media_id, "failed to persist stream copy request");
            ApiError::internal("Failed to create stream copy.")
        })?;
    if !state.stream_copies.enqueue(StreamCopyJob { media_id }) {
        let failed_at = format_timestamp(OffsetDateTime::now_utc());
        if let Err(error) = state
            .persistence
            .mark_stream_copy_failed(media_id, "Stream copy worker queue is full.", &failed_at)
            .await
        {
            warn!(%error, %media_id, "failed to mark stream copy queue admission failure");
        }
        return Err(ApiError::with_status(
            StatusCode::SERVICE_UNAVAILABLE,
            "Stream copy worker queue is full.",
        ));
    }

    let refreshed = state
        .persistence
        .find_stream_copy(media_id)
        .await
        .map_err(|error| {
            warn!(%error, %media_id, "failed to load queued stream copy record");
            ApiError::internal("Failed to load stream copy state.")
        })?;
    let Some(record) = refreshed else {
        return Err(ApiError::internal(
            "Failed to load the queued stream copy state.",
        ));
    };

    let summary = state
        .stream_copies
        .summary_for(media_item.id, media_item.duration_seconds, &record)
        .await;

    Ok(Json(summary))
}

pub(crate) async fn stream_prepared_subtitle(
    State(state): State<AppState>,
    Path(media_id): Path<Uuid>,
) -> Result<Response, ApiError> {
    let media_item = load_media_item(
        &state,
        media_id,
        "stream_prepared_subtitle",
        "Failed to load media.",
    )
    .await?;
    let Some(record) = load_current_stream_copy_record(&state, &media_item).await? else {
        return Err(ApiError::not_found("Prepared subtitle not found."));
    };
    if record.status != StreamCopyStatus::Ready {
        return Err(ApiError::with_status(
            StatusCode::CONFLICT,
            playback_unavailable_message(&media_item),
        ));
    }
    let Some(subtitle_path) = record.subtitle_path.as_deref() else {
        return Err(ApiError::not_found("Prepared subtitle not found."));
    };

    match stream_webvtt_file_response(std::path::Path::new(subtitle_path)).await {
        Ok(response) => Ok(response),
        Err(StreamMediaError::NotFound) => Err(ApiError::not_found("Prepared subtitle not found.")),
        Err(StreamMediaError::Io(error)) => {
            warn!(%error, media_id = %media_item.id, "failed to stream prepared subtitle");
            Err(ApiError::internal("Failed to stream prepared subtitle."))
        }
        Err(StreamMediaError::MalformedRange(_) | StreamMediaError::UnsatisfiableRange { .. }) => {
            Err(ApiError::internal("Failed to stream prepared subtitle."))
        }
    }
}

pub(crate) async fn stream_subtitle(
    State(state): State<AppState>,
    Path((media_id, track_index)): Path<(Uuid, usize)>,
) -> Result<Response, ApiError> {
    let media_item =
        load_media_item(&state, media_id, "stream_subtitle", "Failed to load media.").await?;

    let Some(subtitle_track) = media_item.subtitle_tracks.get(track_index) else {
        return Err(ApiError::not_found("Subtitle track not found."));
    };

    match stream_subtitle_response(&media_item, subtitle_track).await {
        Ok(response) => Ok(response),
        Err(StreamMediaError::NotFound) => Err(ApiError::not_found("Subtitle file not found.")),
        Err(StreamMediaError::Io(error)) => {
            warn!(
                %error,
                media_id = %media_item.id,
                subtitle = %subtitle_track.relative_path,
                "failed to stream subtitle"
            );
            Err(ApiError::internal("Failed to stream subtitle."))
        }
        Err(StreamMediaError::MalformedRange(_) | StreamMediaError::UnsatisfiableRange { .. }) => {
            Err(ApiError::internal("Failed to stream subtitle."))
        }
    }
}

pub(crate) async fn stream_embedded_subtitle(
    State(state): State<AppState>,
    Path((media_id, stream_index)): Path<(Uuid, u32)>,
) -> Result<Response, ApiError> {
    let media_item = load_media_item(
        &state,
        media_id,
        "stream_embedded_subtitle",
        "Failed to load media.",
    )
    .await?;

    let Some(subtitle_stream) = media_item
        .subtitle_streams
        .iter()
        .find(|stream| stream.index == stream_index)
    else {
        return Err(ApiError::not_found("Subtitle stream not found."));
    };

    if !is_text_subtitle_codec(subtitle_stream.codec.as_deref()) {
        return Err(ApiError::bad_request(
            "Selected embedded subtitle stream cannot be converted to WebVTT.",
        ));
    }

    let Some(ffmpeg_command) = state.library.ffmpeg_command() else {
        return Err(ApiError::internal(
            "Embedded subtitle extraction requires ffmpeg to be configured.",
        ));
    };

    match stream_embedded_subtitle_response(ffmpeg_command, &media_item, subtitle_stream).await {
        Ok(response) => Ok(response),
        Err(StreamMediaError::NotFound) => Err(ApiError::not_found("Subtitle stream not found.")),
        Err(StreamMediaError::Io(error)) => {
            warn!(
                %error,
                media_id = %media_item.id,
                stream_index,
                "failed to stream embedded subtitle"
            );
            Err(ApiError::internal("Failed to stream subtitle."))
        }
        Err(StreamMediaError::MalformedRange(_) | StreamMediaError::UnsatisfiableRange { .. }) => {
            Err(ApiError::internal("Failed to stream subtitle."))
        }
    }
}

async fn load_current_stream_copy_record(
    state: &AppState,
    media_item: &MediaItem,
) -> Result<Option<StreamCopyRecord>, ApiError> {
    let record = state
        .persistence
        .find_stream_copy(media_item.id)
        .await
        .map_err(|error| {
            warn!(%error, media_id = %media_item.id, "failed to load stream copy record");
            ApiError::internal("Failed to load stream copy state.")
        })?;

    Ok(record.filter(|record| stream_copy_matches_media_source(record, media_item)))
}

fn stream_copy_matches_media_source(record: &StreamCopyRecord, media_item: &MediaItem) -> bool {
    record.source_size_bytes == media_item.size_bytes
        && record.source_modified_at == media_item.modified_at
}

fn playback_unavailable_message(media_item: &MediaItem) -> &'static str {
    match media_item.preparation_state {
        crate::protocol::PreparationState::Preparing => {
            "A stream copy is still being prepared for this media."
        }
        crate::protocol::PreparationState::Failed => {
            "The last stream copy attempt failed. Create a new stream copy to play this media."
        }
        _ => "This media needs a stream copy before it can be played in the browser.",
    }
}

fn stream_copy_request_matches(
    record: &StreamCopyRecord,
    request: &CreateStreamCopyRequest,
) -> bool {
    record.audio_stream_index == request.audio_stream_index
        && record.subtitle_mode == request.subtitle_mode
        && record.subtitle_kind == request.subtitle.as_ref().map(|subtitle| subtitle.kind)
        && record.subtitle_index == request.subtitle.as_ref().map(|subtitle| subtitle.index)
}

fn validate_stream_copy_request(
    media_item: &MediaItem,
    request: &CreateStreamCopyRequest,
) -> Result<(), ApiError> {
    if let Some(audio_stream_index) = request.audio_stream_index
        && !media_item
            .audio_streams
            .iter()
            .any(|stream| stream.index == audio_stream_index)
    {
        return Err(ApiError::bad_request(
            "Selected audio stream does not exist for this media.",
        ));
    }

    match request.subtitle_mode {
        SubtitleMode::Off => {
            if request.subtitle.is_some() {
                return Err(ApiError::bad_request(
                    "Do not provide a subtitle source when subtitle mode is 'off'.",
                ));
            }
        }
        SubtitleMode::Sidecar | SubtitleMode::Burned => {
            let Some(selection) = request.subtitle.as_ref() else {
                return Err(ApiError::bad_request(
                    "A subtitle source is required for the selected subtitle mode.",
                ));
            };
            match selection.kind {
                SubtitleSourceKind::Sidecar => {
                    let Some(track) = media_item.subtitle_tracks.get(selection.index as usize)
                    else {
                        return Err(ApiError::bad_request(
                            "Selected sidecar subtitle track does not exist.",
                        ));
                    };
                    let extension = track.extension.to_ascii_lowercase();
                    if request.subtitle_mode == SubtitleMode::Sidecar
                        && !matches!(extension.as_str(), "vtt" | "srt")
                    {
                        return Err(ApiError::bad_request(
                            "Only VTT and SRT sidecar subtitles can be prepared as sidecar WebVTT.",
                        ));
                    }
                    if request.subtitle_mode == SubtitleMode::Burned
                        && !matches!(extension.as_str(), "vtt" | "srt" | "ass" | "ssa")
                    {
                        return Err(ApiError::bad_request(
                            "Only text-based sidecar subtitles can be burned into a stream copy.",
                        ));
                    }
                }
                SubtitleSourceKind::Embedded => {
                    let Some(stream) = media_item
                        .subtitle_streams
                        .iter()
                        .find(|stream| stream.index == selection.index)
                    else {
                        return Err(ApiError::bad_request(
                            "Selected embedded subtitle stream does not exist.",
                        ));
                    };
                    if !is_text_subtitle_codec(stream.codec.as_deref()) {
                        return Err(ApiError::bad_request(match request.subtitle_mode {
                            SubtitleMode::Sidecar => {
                                "Selected embedded subtitle stream cannot be converted to sidecar WebVTT."
                            }
                            SubtitleMode::Burned => {
                                "Selected embedded subtitle stream cannot be burned into a stream copy."
                            }
                            SubtitleMode::Off => unreachable!(),
                        }));
                    }
                }
            }
        }
    }

    Ok(())
}

pub(crate) async fn load_media_item(
    state: &AppState,
    media_id: Uuid,
    action: &'static str,
    client_error: &'static str,
) -> Result<MediaItem, ApiError> {
    state
        .library
        .media_item(media_id)
        .await
        .map_err(|error| {
            warn!(%error, %media_id, action, "failed to load media item");
            ApiError::internal(client_error)
        })?
        .ok_or_else(|| ApiError::not_found("Media not found."))
}
