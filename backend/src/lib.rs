use std::{collections::HashMap, net::SocketAddr, path::PathBuf, sync::Arc, time::Duration};

use axum::{
    Json, Router,
    extract::{Path, Query, State, ws::WebSocketUpgrade},
    http::{HeaderMap, HeaderValue, StatusCode, header::RANGE},
    middleware,
    response::{IntoResponse, Response},
    routing::{get, post},
};
use time::OffsetDateTime;
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

pub mod clock;
pub mod ffmpeg;
pub mod library;
pub(crate) mod library_probe;
pub(crate) mod library_walk;
pub(crate) mod origin;
pub mod persistence;
pub(crate) mod persistence_rows;
pub(crate) mod playback;
pub mod probes;
pub mod protocol;
pub(crate) mod rooms;
pub mod scan_gate;
pub mod stream_copies;
pub mod streaming;
pub mod thumbnails;

#[cfg(not(debug_assertions))]
mod frontend_assets;

use clock::format_timestamp;
use library::{LibraryConfig, LibraryService};
use origin::{build_cors_with_origin, origin_allowed, reject_cross_origin_requests};
use persistence::{Persistence, PersistenceError, StreamCopyRecord, StreamCopyRequestRecord};
use probes::{ProbeConfig, ProbeWorkerPool};
use protocol::{
    CreateRoomRequest, CreateStreamCopyRequest, HealthResponse, LibraryResponse,
    LibraryScanResponse, LibraryStatusResponse, MediaItem, PlaybackMode, Room, RoomSocketQuery,
    RoomsResponse, StreamCopyStatus, StreamCopySummary, SubtitleMode, SubtitleSourceKind,
    is_text_subtitle_codec,
};
use rooms::handle_room_socket;
pub(crate) use rooms::{RoomHub, RoomRecord, SharedRoom, SharedRooms};
use stream_copies::{StreamCopyConfig, StreamCopyJob, StreamCopyWorkerPool};
use streaming::{
    StreamMediaError, stream_embedded_subtitle_response, stream_file_response,
    stream_media_response, stream_subtitle_response, stream_thumbnail_response,
    stream_webvtt_file_response, unsatisfiable_range_response,
};
use thumbnails::{ThumbnailConfig, ThumbnailWorkerPool};

const ROOM_EVENT_BUFFER: usize = 64;
const MAX_DISPLAY_NAME_CHARS: usize = 120;
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

async fn list_rooms(State(state): State<AppState>) -> Json<RoomsResponse> {
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

async fn list_library(State(state): State<AppState>) -> Result<Json<LibraryResponse>, ApiError> {
    state.library.snapshot().await.map(Json).map_err(|error| {
        warn!(%error, "failed to load library snapshot");
        ApiError::internal("Failed to load library.")
    })
}

async fn library_status(
    State(state): State<AppState>,
) -> Result<Json<LibraryStatusResponse>, ApiError> {
    state.library.status().await.map(Json).map_err(|error| {
        warn!(%error, "failed to load library status");
        ApiError::internal("Failed to load library status.")
    })
}

async fn scan_library(
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

    // Stage 1 (the walk) is now finished — every discovered file has a row
    // in `media_items`. Hand background work off using internal pending-job
    // queries so the public library summary can stay lean.
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

async fn get_media(
    State(state): State<AppState>,
    Path(media_id): Path<Uuid>,
) -> Result<Json<MediaItem>, ApiError> {
    let media_item =
        load_media_item(&state, media_id, "get_media", "Failed to load media.").await?;

    Ok(Json(media_item))
}

async fn stream_media(
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

async fn stream_thumbnail(
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

async fn get_stream_copy(
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

async fn create_stream_copy(
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

async fn stream_prepared_subtitle(
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
        protocol::PreparationState::Preparing => {
            "A stream copy is still being prepared for this media."
        }
        protocol::PreparationState::Failed => {
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
    media_item: &protocol::MediaItem,
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

async fn stream_subtitle(
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

async fn stream_embedded_subtitle(
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

async fn create_room(
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
    // No clients yet, so the cleanup timer starts ticking immediately.
    // A connecting client within 2 minutes cancels it.
    hub.schedule_cleanup().await;
    state.rooms.write().await.insert(snapshot.id, hub);

    Ok((StatusCode::CREATED, Json(snapshot)))
}

async fn load_media_item(
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

async fn connect_room_socket(
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

fn sanitize_client_name(client_name: Option<String>) -> String {
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

#[derive(Debug)]
struct ApiError {
    status: StatusCode,
    message: String,
}

impl ApiError {
    fn bad_request(message: impl Into<String>) -> Self {
        Self {
            status: StatusCode::BAD_REQUEST,
            message: message.into(),
        }
    }

    fn not_found(message: impl Into<String>) -> Self {
        Self {
            status: StatusCode::NOT_FOUND,
            message: message.into(),
        }
    }

    fn internal(message: impl Into<String>) -> Self {
        Self {
            status: StatusCode::INTERNAL_SERVER_ERROR,
            message: message.into(),
        }
    }

    fn with_status(status: StatusCode, message: impl Into<String>) -> Self {
        Self {
            status,
            message: message.into(),
        }
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let body = Json(serde_json::json!({
            "error": self.message,
        }));

        (self.status, body).into_response()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        clock::AuthoritativePlaybackClock,
        protocol::{
            ClientSocketMessage, DriftCorrectionAction, PlaybackAction, PlaybackStatus, ServerEvent,
        },
        rooms::{
            MAX_CLIENT_ONE_WAY_MS, SocketDispatch, back_date_for_client_latency,
            normalize_command_position, normalize_reported_position,
        },
    };
    use axum::{
        body::{Body, to_bytes},
        http::Request,
    };
    use time::Duration as TimeDuration;
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
