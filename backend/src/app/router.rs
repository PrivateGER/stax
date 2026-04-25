use axum::{
    Json, Router, middleware,
    routing::{get, post},
};
use tower_http::{
    compression::{
        CompressionLayer,
        predicate::{DefaultPredicate, NotForContentType, Predicate},
    },
    trace::TraceLayer,
};

use crate::{
    api::{
        library::{library_status, list_library, scan_library},
        media::{
            create_stream_copy, get_media, get_stream_copy, stream_embedded_subtitle, stream_media,
            stream_prepared_subtitle, stream_subtitle, stream_thumbnail,
        },
        rooms::{connect_room_socket, create_room, list_rooms},
    },
    origin::{build_cors_with_origin, reject_cross_origin_requests},
    protocol::HealthResponse,
};

use super::AppState;

#[cfg(not(debug_assertions))]
use crate::frontend_assets;

pub fn build_app(state: AppState) -> Router {
    let frontend_origin = state.frontend_origin.clone();
    let compression = CompressionLayer::new().compress_when(
        DefaultPredicate::new()
            .and(NotForContentType::const_new("audio/"))
            .and(NotForContentType::const_new("video/"))
            .and(NotForContentType::const_new("application/octet-stream")),
    );
    let app = Router::new()
        .route("/api/health", get(health))
        .route("/api/library", get(list_library))
        .route("/api/library/status", get(library_status))
        .route("/api/library/scan", post(scan_library))
        .route("/api/media/{media_id}", get(get_media))
        .route("/api/media/{media_id}/stream", get(stream_media))
        .route(
            "/api/media/{media_id}/stream-copy",
            get(get_stream_copy).post(create_stream_copy),
        )
        .route(
            "/api/media/{media_id}/stream-copy/subtitle",
            get(stream_prepared_subtitle),
        )
        .route(
            "/api/media/{media_id}/subtitles/{track_index}",
            get(stream_subtitle),
        )
        .route(
            "/api/media/{media_id}/subtitles/embedded/{stream_index}",
            get(stream_embedded_subtitle),
        )
        .route("/api/media/{media_id}/thumbnail", get(stream_thumbnail))
        .route("/api/rooms", get(list_rooms).post(create_room))
        .route("/api/rooms/{room_id}/ws", get(connect_room_socket))
        .with_state(state);

    add_frontend_fallback(app)
        .layer(compression)
        .layer(build_cors_with_origin(frontend_origin.clone()))
        .layer(middleware::from_fn_with_state(
            frontend_origin,
            reject_cross_origin_requests,
        ))
        .layer(TraceLayer::new_for_http())
}

#[cfg(not(debug_assertions))]
fn add_frontend_fallback(app: Router) -> Router {
    app.fallback(frontend_assets::serve)
}

#[cfg(debug_assertions)]
fn add_frontend_fallback(app: Router) -> Router {
    app
}

async fn health() -> Json<HealthResponse> {
    Json(HealthResponse {
        status: "ok",
        service: "stax-backend",
        version: env!("CARGO_PKG_VERSION"),
    })
}
