use std::{collections::HashMap, net::SocketAddr, path::PathBuf, sync::Arc, time::Duration};

use axum::{
    Json, Router,
    http::HeaderValue,
    middleware,
    routing::{get, post},
};
use tokio::sync::{Mutex as TokioMutex, RwLock, mpsc};
use tower_http::{
    compression::{
        CompressionLayer,
        predicate::{DefaultPredicate, NotForContentType, Predicate},
    },
    trace::TraceLayer,
};
use tracing::warn;
use uuid::Uuid;

pub(crate) mod api_error;
pub mod clock;
pub mod ffmpeg;
pub mod library;
pub(crate) mod library_probe;
pub(crate) mod library_routes;
pub(crate) mod library_walk;
pub(crate) mod media_routes;
pub(crate) mod origin;
pub mod persistence;
pub(crate) mod persistence_rows;
pub(crate) mod playback;
pub mod probes;
pub mod protocol;
pub(crate) mod room_routes;
pub(crate) mod rooms;
pub mod scan_gate;
pub mod stream_copies;
pub(crate) mod stream_copy_progress;
pub mod streaming;
pub(crate) mod thumbnail_render;
pub mod thumbnails;

#[cfg(not(debug_assertions))]
mod frontend_assets;

use library::{LibraryConfig, LibraryService};
use library_routes::{library_status, list_library, scan_library};
use media_routes::{
    create_stream_copy, get_media, get_stream_copy, stream_embedded_subtitle, stream_media,
    stream_prepared_subtitle, stream_subtitle, stream_thumbnail,
};
use origin::{build_cors_with_origin, reject_cross_origin_requests};
use persistence::{Persistence, PersistenceError};
use probes::{ProbeConfig, ProbeWorkerPool};
use protocol::HealthResponse;
use room_routes::{connect_room_socket, create_room, list_rooms};
pub(crate) use rooms::{RoomHub, RoomRecord, SharedRoom, SharedRooms};
use stream_copies::{StreamCopyConfig, StreamCopyWorkerPool};
use thumbnails::{ThumbnailConfig, ThumbnailWorkerPool};

const ROOM_EVENT_BUFFER: usize = 64;
/// Default grace period before a room with no connected clients is
/// deleted. Tolerates brief reconnects (refresh, network blip) without
/// destroying a session. Integration tests can shrink this via
/// `load_state_with_library_and_grace`.
pub const DEFAULT_EMPTY_ROOM_GRACE: Duration = Duration::from_secs(120);
pub const DEFAULT_API_ADDR: &str = "127.0.0.1:3001";
pub const DEFAULT_DATABASE_PATH: &str = "stax.db";
pub const DEFAULT_LOG_FILTER: &str = "stax_backend=debug,tower_http=info";

#[derive(Clone)]
pub struct AppState {
    rooms: SharedRooms,
    library: LibraryService,
    persistence: Persistence,
    stream_copies: StreamCopyWorkerPool,
    thumbnails: ThumbnailWorkerPool,
    probes: ProbeWorkerPool,
    scan_lock: Arc<TokioMutex<()>>,
    cleanup_tx: mpsc::UnboundedSender<Uuid>,
    empty_room_grace: Duration,
    frontend_origin: Option<HeaderValue>,
}

#[derive(Clone, Debug)]
pub struct RuntimeConfig {
    pub database_path: PathBuf,
    pub library: LibraryConfig,
    pub stream_copy_workers: usize,
    pub thumbnail_workers: usize,
    pub frontend_origin: Option<String>,
}

impl Default for RuntimeConfig {
    fn default() -> Self {
        Self {
            database_path: PathBuf::from(DEFAULT_DATABASE_PATH),
            library: LibraryConfig::default(),
            stream_copy_workers: 1,
            thumbnail_workers: 2,
            frontend_origin: None,
        }
    }
}

#[derive(Clone, Debug)]
pub struct ServerConfig {
    pub api_addr: SocketAddr,
    pub runtime: RuntimeConfig,
    pub log_filter: String,
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            api_addr: DEFAULT_API_ADDR
                .parse()
                .expect("default API address should parse"),
            runtime: RuntimeConfig::default(),
            log_filter: DEFAULT_LOG_FILTER.to_string(),
        }
    }
}

pub async fn load_state(persistence: Persistence) -> Result<AppState, PersistenceError> {
    load_state_with_library(persistence, LibraryConfig::default()).await
}

pub async fn load_state_with_library(
    persistence: Persistence,
    library_config: LibraryConfig,
) -> Result<AppState, PersistenceError> {
    load_state_with_library_and_grace(persistence, library_config, DEFAULT_EMPTY_ROOM_GRACE).await
}

pub async fn load_state_with_library_and_grace(
    persistence: Persistence,
    library_config: LibraryConfig,
    empty_room_grace: Duration,
) -> Result<AppState, PersistenceError> {
    load_state_with_runtime_and_grace(
        persistence,
        RuntimeConfig {
            library: library_config,
            ..RuntimeConfig::default()
        },
        empty_room_grace,
    )
    .await
}

pub async fn load_state_from_runtime(config: RuntimeConfig) -> Result<AppState, PersistenceError> {
    let persistence = Persistence::open_at(&config.database_path).await?;
    load_state_with_runtime(persistence, config).await
}

pub async fn load_state_with_runtime(
    persistence: Persistence,
    config: RuntimeConfig,
) -> Result<AppState, PersistenceError> {
    load_state_with_runtime_and_grace(persistence, config, DEFAULT_EMPTY_ROOM_GRACE).await
}

pub async fn load_state_with_runtime_and_grace(
    persistence: Persistence,
    config: RuntimeConfig,
    empty_room_grace: Duration,
) -> Result<AppState, PersistenceError> {
    let frontend_origin = parse_frontend_origin(config.frontend_origin.as_deref())?;
    let stream_copy_config = StreamCopyConfig {
        cache_dir: config
            .library
            .stream_copy_cache_dir()
            .map(std::path::Path::to_path_buf),
        ffmpeg_command: config
            .library
            .ffmpeg_command()
            .map(std::path::Path::to_path_buf),
        hw_accel: config.library.hw_accel().clone(),
        max_concurrent: config.stream_copy_workers.max(1),
    };
    let thumbnail_config = ThumbnailConfig {
        cache_dir: config
            .library
            .thumbnail_cache_dir()
            .map(std::path::Path::to_path_buf),
        ffmpeg_command: config
            .library
            .ffmpeg_command()
            .map(std::path::Path::to_path_buf),
        hw_accel: config.library.hw_accel().clone(),
        max_concurrent: config.thumbnail_workers.max(1),
    };
    // Shared across the background scan pools. The gate lets the project
    // pause new probe/thumbnail work around any future foreground work that
    // needs the same library mount.
    let scan_gate = crate::scan_gate::ScanGate::new();
    let thumbnails =
        ThumbnailWorkerPool::spawn(thumbnail_config, persistence.clone(), scan_gate.clone());

    let probe_config = ProbeConfig {
        probe_command: config
            .library
            .probe_command()
            .map(std::path::Path::to_path_buf),
        max_concurrent: config.library.probe_workers(),
    };
    let probes = ProbeWorkerPool::spawn(
        probe_config,
        persistence.clone(),
        thumbnails.clone(),
        scan_gate.clone(),
    );

    let library = LibraryService::new(persistence.clone(), config.library);
    library.sync_config().await?;
    let stream_copies =
        StreamCopyWorkerPool::spawn(stream_copy_config, persistence.clone(), library.clone());

    // Drain the persisted backlogs: any items that were pending when the
    // process last exited (or never finished) need to be re-enqueued so
    // workers can resume from where they left off. Probes drain first so
    // their successful outcomes can chain into thumbnail jobs as the
    // thumbnail pool starts processing its own backlog.
    let pending_probes = persistence.list_pending_probes().await?;
    if !pending_probes.is_empty() {
        tracing::info!(
            count = pending_probes.len(),
            "enqueueing pending probe jobs from previous session"
        );
        probes.enqueue_pending(pending_probes);
    }
    let pending = persistence.list_pending_thumbnails().await?;
    if !pending.is_empty() {
        tracing::info!(
            count = pending.len(),
            "enqueueing pending thumbnail jobs from previous session"
        );
        thumbnails.enqueue_pending(pending);
    }
    let pending_stream_copies = persistence.list_pending_stream_copies().await?;
    if !pending_stream_copies.is_empty() {
        tracing::info!(
            count = pending_stream_copies.len(),
            "enqueueing pending stream copy jobs from previous session"
        );
        stream_copies.enqueue_pending(pending_stream_copies);
    }

    let room_records = persistence.load_rooms().await?;

    let (cleanup_tx, cleanup_rx) = mpsc::unbounded_channel::<Uuid>();

    let rooms: HashMap<Uuid, SharedRoom> = room_records
        .into_iter()
        .map(|room| {
            (
                room.id,
                Arc::new(RoomHub::new(
                    room,
                    persistence.clone(),
                    library.clone(),
                    cleanup_tx.clone(),
                    empty_room_grace,
                )),
            )
        })
        .collect();
    let rooms: SharedRooms = Arc::new(RwLock::new(rooms));

    spawn_room_cleanup_task(rooms.clone(), persistence.clone(), cleanup_rx);
    // Any room that was already empty at boot (process died while clients
    // were elsewhere) should also fall under the cleanup timer.
    {
        let rooms_guard = rooms.read().await;
        for hub in rooms_guard.values() {
            hub.schedule_cleanup().await;
        }
    }

    Ok(AppState {
        library,
        rooms,
        persistence,
        stream_copies,
        thumbnails,
        probes,
        scan_lock: Arc::new(TokioMutex::new(())),
        cleanup_tx,
        empty_room_grace,
        frontend_origin,
    })
}

fn parse_frontend_origin(origin: Option<&str>) -> Result<Option<HeaderValue>, PersistenceError> {
    origin
        .map(|value| {
            HeaderValue::from_str(value).map_err(|error| {
                PersistenceError::InvalidData(format!(
                    "frontend origin must be a valid HTTP header value: {error}"
                ))
            })
        })
        .transpose()
}

fn spawn_room_cleanup_task(
    rooms: SharedRooms,
    persistence: Persistence,
    mut cleanup_rx: mpsc::UnboundedReceiver<Uuid>,
) {
    tokio::spawn(async move {
        while let Some(room_id) = cleanup_rx.recv().await {
            let mut guard = rooms.write().await;
            let Some(hub) = guard.get(&room_id) else {
                continue;
            };
            if hub.connection_count() != 0 {
                // Race: a client joined after the timer fired. Leave the
                // room alone; a later leave-to-zero will reschedule.
                continue;
            }
            guard.remove(&room_id);
            drop(guard);
            if let Err(error) = persistence.delete_room(room_id).await {
                warn!(%error, %room_id, "failed to delete empty room");
            } else {
                tracing::info!(%room_id, "deleted empty room after grace period");
            }
        }
    });
}

pub async fn seeded_state() -> AppState {
    let persistence = Persistence::open_in_memory()
        .await
        .expect("in-memory persistence should initialize");

    load_state(persistence)
        .await
        .expect("in-memory app state should initialize")
}

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

pub fn init_tracing(log_filter: &str) {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::new(log_filter))
        .with_target(false)
        .compact()
        .init();
}

pub async fn shutdown_signal() {
    let ctrl_c = async {
        tokio::signal::ctrl_c()
            .await
            .expect("failed to install Ctrl+C signal handler");
    };

    #[cfg(unix)]
    let terminate = async {
        use tokio::signal::unix::{SignalKind, signal};

        signal(SignalKind::terminate())
            .expect("failed to install SIGTERM signal handler")
            .recv()
            .await;
    };

    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        _ = ctrl_c => {}
        _ = terminate => {}
    }
}

async fn health() -> Json<HealthResponse> {
    Json(HealthResponse {
        status: "ok",
        service: "stax-backend",
        version: env!("CARGO_PKG_VERSION"),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        clock::{AuthoritativePlaybackClock, format_timestamp},
        protocol::{
            ClientSocketMessage, DriftCorrectionAction, PlaybackAction, PlaybackStatus,
            RoomsResponse, ServerEvent,
        },
        room_routes::{MAX_DISPLAY_NAME_CHARS, sanitize_client_name},
        rooms::{
            MAX_CLIENT_ONE_WAY_MS, SocketDispatch, back_date_for_client_latency,
            normalize_command_position, normalize_reported_position,
        },
    };
    use axum::{
        body::{Body, to_bytes},
        http::{Request, StatusCode},
    };
    use time::Duration as TimeDuration;
    use time::OffsetDateTime;
    use tower::ServiceExt;
    use uuid::Uuid;

    async fn test_app() -> Router {
        build_app(seeded_state().await)
    }

    async fn test_room_hub(room: RoomRecord) -> (Arc<RoomHub>, mpsc::UnboundedReceiver<Uuid>) {
        let persistence = Persistence::open_in_memory().await.unwrap();
        let library = LibraryService::new(persistence.clone(), LibraryConfig::default());
        let (cleanup_tx, cleanup_rx) = mpsc::unbounded_channel();
        let hub = Arc::new(RoomHub::new(
            room,
            persistence,
            library,
            cleanup_tx,
            DEFAULT_EMPTY_ROOM_GRACE,
        ));
        (hub, cleanup_rx)
    }

    #[tokio::test]
    async fn health_endpoint_returns_ok() {
        let response = test_app()
            .await
            .oneshot(
                Request::builder()
                    .uri("/api/health")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn create_room_rejects_empty_name() {
        let response = test_app()
            .await
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/rooms")
                    .header("content-type", "application/json")
                    .body(Body::from(r#"{"name":"   "}"#))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn create_room_rejects_oversized_name() {
        let oversized = "a".repeat(MAX_DISPLAY_NAME_CHARS + 1);
        let response = test_app()
            .await
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/rooms")
                    .header("content-type", "application/json")
                    .body(Body::from(format!(r#"{{"name":"{oversized}"}}"#)))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn create_room_rejects_oversized_media_title() {
        let oversized = "a".repeat(MAX_DISPLAY_NAME_CHARS + 1);
        let response = test_app()
            .await
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/rooms")
                    .header("content-type", "application/json")
                    .body(Body::from(format!(
                        r#"{{"name":"Movie Lab","mediaTitle":"{oversized}"}}"#
                    )))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn rejects_cross_origin_http_requests_by_default() {
        let response = test_app()
            .await
            .oneshot(
                Request::builder()
                    .uri("/api/health")
                    .header("host", "stax.local:3001")
                    .header("origin", "https://evil.example")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::FORBIDDEN);
    }

    #[tokio::test]
    async fn accepts_same_host_origin_by_default() {
        let response = test_app()
            .await
            .oneshot(
                Request::builder()
                    .uri("/api/health")
                    .header("host", "stax.local:3001")
                    .header("origin", "http://stax.local:3001")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn create_room_is_reflected_in_room_listing() {
        let app = test_app().await;

        let create_response = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/rooms")
                    .header("content-type", "application/json")
                    .body(Body::from(r#"{"name":"Movie Lab","mediaTitle":"Solaris"}"#))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(create_response.status(), StatusCode::CREATED);

        let list_response = app
            .oneshot(
                Request::builder()
                    .uri("/api/rooms")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        let body = to_bytes(list_response.into_body(), usize::MAX)
            .await
            .unwrap();
        let payload: RoomsResponse = serde_json::from_slice(&body).unwrap();

        assert_eq!(payload.rooms.len(), 1);
        assert!(payload.rooms.iter().any(|room| room.name == "Movie Lab"));
    }

    #[tokio::test]
    async fn report_position_returns_direct_correction_event() {
        let (room, _cleanup_rx) = test_room_hub(RoomRecord::new(
            "Test Room".into(),
            None,
            None,
            OffsetDateTime::now_utc(),
        ))
        .await;

        let connection_id = Uuid::new_v4();
        room.add_participant(connection_id, "operator".into()).await;
        let event = room
            .apply_socket_message(
                ClientSocketMessage::ReportPosition {
                    position_seconds: 2.2,
                },
                "operator",
                connection_id,
            )
            .await
            .unwrap();

        match event {
            SocketDispatch::Direct(ServerEvent::DriftCorrection {
                actor,
                suggested_action,
                ..
            }) => {
                assert_eq!(actor, "operator");
                assert_eq!(suggested_action, DriftCorrectionAction::Seek);
            }
            _ => panic!("expected direct drift correction event"),
        }
    }

    #[tokio::test]
    async fn pause_without_position_uses_elapsed_playback_time() {
        let (room, _cleanup_rx) = test_room_hub(RoomRecord::new(
            "Clock Room".into(),
            None,
            None,
            OffsetDateTime::now_utc(),
        ))
        .await;

        let connection_id = Uuid::new_v4();
        room.apply_socket_message(
            ClientSocketMessage::Play {
                position_seconds: Some(5.0),
                client_one_way_ms: None,
            },
            "operator",
            connection_id,
        )
        .await
        .unwrap();

        tokio::time::sleep(std::time::Duration::from_millis(250)).await;

        let event = room
            .apply_socket_message(
                ClientSocketMessage::Pause {
                    position_seconds: None,
                    client_one_way_ms: None,
                },
                "operator",
                connection_id,
            )
            .await
            .unwrap();

        match event {
            SocketDispatch::Broadcast(ServerEvent::PlaybackUpdated { room, action, .. }) => {
                assert_eq!(action, PlaybackAction::Pause);
                assert_eq!(room.playback_state.status, PlaybackStatus::Paused);
                assert!(room.playback_state.position_seconds >= 5.2);
            }
            _ => panic!("expected playback update"),
        }
    }

    #[test]
    fn sanitize_blank_client_name_creates_fallback() {
        let client_name = sanitize_client_name(Some("   ".into()));

        assert!(client_name.starts_with("viewer-"));
        assert_eq!(client_name.len(), "viewer-".len() + 8);
    }

    #[test]
    fn sanitize_client_name_truncates_oversized_names() {
        let client_name = sanitize_client_name(Some("a".repeat(MAX_DISPLAY_NAME_CHARS + 1)));

        assert_eq!(client_name.len(), MAX_DISPLAY_NAME_CHARS);
    }

    #[test]
    fn parse_frontend_origin_rejects_invalid_header_values() {
        let error =
            parse_frontend_origin(Some("https://frontend.example\r\nx-test: nope")).unwrap_err();

        match error {
            PersistenceError::InvalidData(message) => {
                assert!(message.contains("frontend origin must be a valid HTTP header value"));
            }
            other => panic!("expected invalid data error, got {other:?}"),
        }
    }

    #[test]
    fn normalize_reported_position_preserves_precision() {
        assert_eq!(normalize_reported_position(1.23456).unwrap(), 1.235);
    }

    #[test]
    fn back_date_for_client_latency_passes_through_none_and_zero() {
        let now = OffsetDateTime::UNIX_EPOCH;

        assert_eq!(back_date_for_client_latency(now, None), now);
        assert_eq!(back_date_for_client_latency(now, Some(0)), now);
    }

    #[test]
    fn back_date_for_client_latency_shifts_by_reported_amount() {
        let now = OffsetDateTime::UNIX_EPOCH;
        let shifted = back_date_for_client_latency(now, Some(250));

        assert_eq!(now - shifted, TimeDuration::milliseconds(250));
    }

    #[test]
    fn back_date_for_client_latency_clamps_values_above_ceiling() {
        let now = OffsetDateTime::UNIX_EPOCH;
        let shifted = back_date_for_client_latency(now, Some(10_000));

        assert_eq!(
            now - shifted,
            TimeDuration::milliseconds(MAX_CLIENT_ONE_WAY_MS as i64)
        );
    }

    #[test]
    fn normalize_command_position_rejects_negative_values() {
        assert_eq!(
            normalize_command_position(-1.0).unwrap_err(),
            "Playback position cannot be negative."
        );
    }

    #[test]
    fn room_snapshot_contains_anchor_and_clock_metadata() {
        let now = OffsetDateTime::UNIX_EPOCH;
        let room = RoomRecord {
            id: Uuid::new_v4(),
            name: "Snapshot Room".into(),
            media_id: None,
            media_title: None,
            created_at: format_timestamp(now),
            clock: AuthoritativePlaybackClock::new_paused(now),
        };

        let snapshot = room.snapshot(now);

        assert_eq!(snapshot.playback_state.position_seconds, 0.0);
        assert_eq!(snapshot.playback_state.anchor_position_seconds, 0.0);
        assert_eq!(
            snapshot.playback_state.clock_updated_at,
            snapshot.created_at
        );
    }
}
