use std::sync::Arc;

use axum::{
    Json,
    extract::{Path, Query, State, ws::WebSocketUpgrade},
    http::{HeaderMap, StatusCode},
    response::Response,
};
use time::OffsetDateTime;
use tracing::warn;
use uuid::Uuid;

use crate::{
    AppState,
    api_error::ApiError,
    media_routes::load_media_item,
    origin::origin_allowed,
    protocol::{CreateRoomRequest, Room, RoomSocketQuery, RoomsResponse},
    rooms::{RoomHub, RoomRecord, SharedRoom, handle_room_socket},
};

pub(crate) const MAX_DISPLAY_NAME_CHARS: usize = 120;

pub(crate) async fn list_rooms(State(state): State<AppState>) -> Json<RoomsResponse> {
    let room_hubs = {
        let rooms = state.rooms.read().await;
        rooms.values().cloned().collect::<Vec<_>>()
    };

    let mut snapshots = Vec::with_capacity(room_hubs.len());

    for room_hub in room_hubs {
        snapshots.push(room_hub.snapshot().await);
    }

    snapshots.sort_unstable_by(|left, right| left.name.cmp(&right.name));

    Json(RoomsResponse { rooms: snapshots })
}

pub(crate) async fn create_room(
    State(state): State<AppState>,
    Json(payload): Json<CreateRoomRequest>,
) -> Result<(StatusCode, Json<Room>), ApiError> {
    let trimmed_name = payload.name.trim();

    if trimmed_name.is_empty() {
        return Err(ApiError::bad_request("Room name is required"));
    }
    if trimmed_name.chars().count() > MAX_DISPLAY_NAME_CHARS {
        return Err(ApiError::bad_request("Room name is too long."));
    }

    let provided_media_title = payload
        .media_title
        .map(|value| {
            let trimmed = value.trim();
            if trimmed.chars().count() > MAX_DISPLAY_NAME_CHARS {
                return Err(ApiError::bad_request("Media title is too long."));
            }
            Ok(trimmed.to_string())
        })
        .transpose()?
        .filter(|value| !value.is_empty());

    let (media_id, media_title) = match payload.media_id {
        Some(media_id) => {
            let media_item = load_media_item(
                &state,
                media_id,
                "create_room",
                "Failed to resolve media for room.",
            )
            .await
            .map_err(|error| match error.status {
                StatusCode::NOT_FOUND => {
                    ApiError::bad_request("Media is not in the library index.")
                }
                _ => error,
            })?;

            let derived_title = provided_media_title
                .clone()
                .unwrap_or_else(|| media_item.file_name.clone());

            (Some(media_item.id), Some(derived_title))
        }
        None => (None, provided_media_title),
    };

    let room = RoomRecord::new(
        trimmed_name.into(),
        media_id,
        media_title,
        OffsetDateTime::now_utc(),
    );
    let snapshot = room.snapshot(OffsetDateTime::now_utc());

    state.persistence.save_room(&room).await.map_err(|error| {
        warn!(%error, room_id = %room.id, "failed to persist created room");
        ApiError::internal("Failed to persist room.")
    })?;

    let hub = Arc::new(RoomHub::new(
        room,
        state.persistence.clone(),
        state.library.clone(),
        state.cleanup_tx.clone(),
        state.empty_room_grace,
    ));
    hub.schedule_cleanup().await;
    state.rooms.write().await.insert(snapshot.id, hub);

    Ok((StatusCode::CREATED, Json(snapshot)))
}

pub(crate) async fn connect_room_socket(
    State(state): State<AppState>,
    Path(room_id): Path<Uuid>,
    Query(query): Query<RoomSocketQuery>,
    headers: HeaderMap,
    websocket: WebSocketUpgrade,
) -> Result<Response, ApiError> {
    if !origin_allowed(&headers, state.frontend_origin.as_ref()) {
        return Err(ApiError::with_status(
            StatusCode::FORBIDDEN,
            "WebSocket origin is not allowed.",
        ));
    }

    let room = find_room(&state, room_id)
        .await
        .ok_or_else(|| ApiError::not_found("Room not found"))?;

    let client_name = sanitize_client_name(query.client_name);

    Ok(websocket.on_upgrade(move |socket| async move {
        handle_room_socket(socket, room_id, room, client_name).await;
    }))
}

async fn find_room(state: &AppState, room_id: Uuid) -> Option<SharedRoom> {
    let rooms = state.rooms.read().await;
    rooms.get(&room_id).cloned()
}

pub(crate) fn sanitize_client_name(client_name: Option<String>) -> String {
    client_name
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
        .map(|value| truncate_chars(value, MAX_DISPLAY_NAME_CHARS))
        .unwrap_or_else(|| {
            let fallback = Uuid::new_v4();
            format!("viewer-{}", &fallback.to_string()[..8])
        })
}

fn truncate_chars(value: String, max_chars: usize) -> String {
    if value.chars().count() <= max_chars {
        return value;
    }

    value.chars().take(max_chars).collect()
}
