use axum::{Json, extract::State, http::StatusCode};
use tracing::warn;

use crate::{
    AppState,
    api_error::ApiError,
    protocol::{LibraryResponse, LibraryScanResponse, LibraryStatusResponse},
};

pub(crate) async fn list_library(
    State(state): State<AppState>,
) -> Result<Json<LibraryResponse>, ApiError> {
    state.library.snapshot().await.map(Json).map_err(|error| {
        warn!(%error, "failed to load library snapshot");
        ApiError::internal("Failed to load library.")
    })
}

pub(crate) async fn library_status(
    State(state): State<AppState>,
) -> Result<Json<LibraryStatusResponse>, ApiError> {
    state.library.status().await.map(Json).map_err(|error| {
        warn!(%error, "failed to load library status");
        ApiError::internal("Failed to load library status.")
    })
}

pub(crate) async fn scan_library(
    State(state): State<AppState>,
) -> Result<Json<LibraryScanResponse>, ApiError> {
    let Ok(_scan_guard) = state.scan_lock.try_lock() else {
        return Err(ApiError::with_status(
            StatusCode::CONFLICT,
            "A library scan is already running.",
        ));
    };

    let response = state.library.scan().await.map_err(|error| {
        warn!(%error, "failed to scan library");
        ApiError::internal("Failed to scan library.")
    })?;

    let pending_probes = state
        .persistence
        .list_pending_probes()
        .await
        .map_err(|error| {
            warn!(%error, "failed to load pending probes after scan");
            ApiError::internal("Failed to enqueue library scan work.")
        })?;
    let probes_enqueued = pending_probes.len();
    state.probes.enqueue_pending(pending_probes);

    let pending_thumbnails =
        state
            .persistence
            .list_pending_thumbnails()
            .await
            .map_err(|error| {
                warn!(%error, "failed to load pending thumbnails after scan");
                ApiError::internal("Failed to enqueue library scan work.")
            })?;
    let thumbnails_enqueued = pending_thumbnails.len();
    state.thumbnails.enqueue_pending(pending_thumbnails);
    tracing::info!(
        scanned = response.items.len(),
        probes_enqueued,
        thumbnails_enqueued,
        "library scan completed; background jobs enqueued"
    );

    Ok(Json(response))
}
