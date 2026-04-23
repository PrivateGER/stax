use std::{
    collections::HashSet,
    fs,
    path::{Path, PathBuf},
    time::Duration,
};

#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;

use axum::Router;
use futures_util::{SinkExt, StreamExt, future::join_all};
use reqwest::Client;
use reqwest::header::{ACCEPT_ENCODING, CONTENT_ENCODING, CONTENT_RANGE, RANGE};
use syncplay_backend::{
    build_app,
    library::LibraryConfig,
    load_state, load_state_with_library, load_state_with_library_and_grace,
    persistence::Persistence,
    protocol::{
        CreateStreamCopyRequest, DriftCorrectionAction, LibraryResponse, LibraryScanResponse,
        LibraryStatusResponse, PlaybackStatus, Room, RoomsResponse, ServerEvent, StreamCopyStatus,
        StreamCopySummary,
    },
    seeded_state,
};
use tempfile::TempDir;
use tokio::{net::TcpListener, task::JoinHandle, time::timeout};
use tokio_tungstenite::{connect_async, tungstenite::Message};

struct TestServer {
    base_url: String,
    ws_base_url: String,
    client: Client,
    task: JoinHandle<()>,
}

impl TestServer {
    async fn spawn() -> Self {
        Self::spawn_with_app(build_app(seeded_state().await)).await
    }

    async fn spawn_persistent(database_path: &Path) -> Self {
        let persistence = Persistence::open_at(database_path).await.unwrap();
        let app = build_app(load_state(persistence).await.unwrap());

        Self::spawn_with_app(app).await
    }

    async fn spawn_with_library_roots(root_paths: Vec<PathBuf>) -> Self {
        Self::spawn_with_library_config(LibraryConfig::from_paths(root_paths).without_probe()).await
    }

    async fn spawn_persistent_with_library(database_path: &Path, root_paths: Vec<PathBuf>) -> Self {
        Self::spawn_persistent_with_library_config(
            database_path,
            LibraryConfig::from_paths(root_paths).without_probe(),
        )
        .await
    }

    async fn spawn_with_library_config(config: LibraryConfig) -> Self {
        let persistence = Persistence::open_in_memory().await.unwrap();
        let app = build_app(load_state_with_library(persistence, config).await.unwrap());

        Self::spawn_with_app(app).await
    }

    async fn spawn_with_grace(grace: Duration) -> Self {
        let persistence = Persistence::open_in_memory().await.unwrap();
        let app = build_app(
            load_state_with_library_and_grace(persistence, LibraryConfig::default(), grace)
                .await
                .unwrap(),
        );

        Self::spawn_with_app(app).await
    }

    async fn spawn_persistent_with_library_config(
        database_path: &Path,
        config: LibraryConfig,
    ) -> Self {
        let persistence = Persistence::open_at(database_path).await.unwrap();
        let app = build_app(load_state_with_library(persistence, config).await.unwrap());

        Self::spawn_with_app(app).await
    }

    async fn spawn_with_app(app: Router) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let task = tokio::spawn(serve(listener, app));

        Self {
            base_url: format!("http://{address}"),
            ws_base_url: format!("ws://{address}"),
            client: Client::new(),
            task,
        }
    }

    async fn rooms(&self) -> RoomsResponse {
        self.client
            .get(format!("{}/api/rooms", self.base_url))
            .send()
            .await
            .unwrap()
            .error_for_status()
            .unwrap()
            .json()
            .await
            .unwrap()
    }

    async fn seeded_room(&self) -> Room {
        self.client
            .post(format!("{}/api/rooms", self.base_url))
            .json(&serde_json::json!({
                "name": "Test Room"
            }))
            .send()
            .await
            .unwrap()
            .error_for_status()
            .unwrap()
            .json()
            .await
            .unwrap()
    }

    async fn library(&self) -> LibraryResponse {
        self.client
            .get(format!("{}/api/library", self.base_url))
            .send()
            .await
            .unwrap()
            .error_for_status()
            .unwrap()
            .json()
            .await
            .unwrap()
    }

    async fn scan_library(&self) -> LibraryScanResponse {
        self.client
            .post(format!("{}/api/library/scan", self.base_url))
            .send()
            .await
            .unwrap()
            .error_for_status()
            .unwrap()
            .json()
            .await
            .unwrap()
    }

    async fn library_status(&self) -> LibraryStatusResponse {
        self.client
            .get(format!("{}/api/library/status", self.base_url))
            .send()
            .await
            .unwrap()
            .error_for_status()
            .unwrap()
            .json()
            .await
            .unwrap()
    }

    /// Poll `/api/library` until the predicate succeeds for the first item
    /// or the timeout elapses. Thumbnail generation is now backgrounded, so
    /// tests that previously read scan output synchronously have to wait
    /// for the worker pool to catch up.
    async fn poll_first_item_until<F>(
        &self,
        timeout_duration: Duration,
        mut predicate: F,
    ) -> syncplay_backend::protocol::MediaItem
    where
        F: FnMut(&syncplay_backend::protocol::MediaItem) -> bool,
    {
        let deadline = std::time::Instant::now() + timeout_duration;
        loop {
            let library = self.library().await;
            if let Some(item) = library.items.into_iter().next()
                && predicate(&item)
            {
                return item;
            }

            if std::time::Instant::now() >= deadline {
                panic!("predicate did not become true within {timeout_duration:?}");
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    }

    /// Wait until the background probe pool has resolved every item in the
    /// library — either with metadata (`probed_at IS NOT NULL`) or a
    /// recorded failure (`probe_error IS NOT NULL`). The walk now returns
    /// before probes finish, so tests that assert on probe-derived fields
    /// (codec, duration, dimensions, playback_mode) have to wait first.
    async fn wait_for_probes_complete(&self) -> LibraryResponse {
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        loop {
            let library = self.library().await;
            let pending = library
                .items
                .iter()
                .filter(|item| item.probed_at.is_none() && item.probe_error.is_none())
                .count();
            if pending == 0 {
                return library;
            }
            if std::time::Instant::now() >= deadline {
                panic!("{} item(s) still have no probe outcome after 5s", pending);
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    }

    /// Same as `wait_for_probes_complete` but also waits for the thumbnail
    /// pool to settle. Use when a test asserts on `thumbnail_generated_at`
    /// or `thumbnail_error`.
    async fn wait_for_library_complete(&self) -> LibraryResponse {
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        loop {
            let library = self.library().await;
            let probe_pending = library
                .items
                .iter()
                .filter(|item| item.probed_at.is_none() && item.probe_error.is_none())
                .count();
            let thumb_pending = library
                .items
                .iter()
                .filter(|item| {
                    item.thumbnail_generated_at.is_none()
                        && item.thumbnail_error.is_none()
                        && item.probe_error.is_none()
                })
                .count();
            if probe_pending == 0 && thumb_pending == 0 {
                return library;
            }
            if std::time::Instant::now() >= deadline {
                panic!(
                    "library never settled: {probe_pending} probe pending, {thumb_pending} thumbnail pending"
                );
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    }

    async fn wait_for_library_status<F>(
        &self,
        timeout_duration: Duration,
        mut predicate: F,
    ) -> LibraryStatusResponse
    where
        F: FnMut(&LibraryStatusResponse) -> bool,
    {
        let deadline = std::time::Instant::now() + timeout_duration;
        loop {
            let status = self.library_status().await;
            if predicate(&status) {
                return status;
            }

            if std::time::Instant::now() >= deadline {
                panic!("library status did not match predicate within {timeout_duration:?}");
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    }

    async fn create_stream_copy(
        &self,
        media_id: &str,
        request: &CreateStreamCopyRequest,
    ) -> reqwest::Response {
        self.client
            .post(format!(
                "{}/api/media/{media_id}/stream-copy",
                self.base_url
            ))
            .json(request)
            .send()
            .await
            .unwrap()
    }

    async fn stream_copy_summary(&self, media_id: &str) -> StreamCopySummary {
        self.client
            .get(format!(
                "{}/api/media/{media_id}/stream-copy",
                self.base_url
            ))
            .send()
            .await
            .unwrap()
            .error_for_status()
            .unwrap()
            .json()
            .await
            .unwrap()
    }

    async fn wait_for_stream_copy_summary<F>(
        &self,
        media_id: &str,
        timeout_duration: Duration,
        mut predicate: F,
    ) -> StreamCopySummary
    where
        F: FnMut(&StreamCopySummary) -> bool,
    {
        let deadline = std::time::Instant::now() + timeout_duration;
        loop {
            let summary = self.stream_copy_summary(media_id).await;
            if predicate(&summary) {
                return summary;
            }

            if std::time::Instant::now() >= deadline {
                panic!("stream copy summary did not match predicate within {timeout_duration:?}");
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    }

    async fn wait_for_preparation_state(
        &self,
        media_id: &str,
        expected: &str,
    ) -> syncplay_backend::protocol::MediaItem {
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        loop {
            let library = self.library().await;
            if let Some(item) = library
                .items
                .into_iter()
                .find(|item| item.id.to_string() == media_id)
            {
                let state = serde_json::to_value(item.preparation_state)
                    .unwrap()
                    .as_str()
                    .unwrap()
                    .to_string();
                if state == expected {
                    return item;
                }
            }

            if std::time::Instant::now() >= deadline {
                panic!("preparation state did not become {expected} within 5s");
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    }
}

impl Drop for TestServer {
    fn drop(&mut self) {
        self.task.abort();
    }
}

async fn serve(listener: TcpListener, app: Router) {
    axum::serve(listener, app).await.unwrap();
}

async fn connect_room_socket(
    server: &TestServer,
    room_id: &str,
    client_name: &str,
) -> tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>> {
    let query = format!("clientName={client_name}");
    connect_room_socket_with_query(server, room_id, &query).await
}

async fn connect_room_socket_with_query(
    server: &TestServer,
    room_id: &str,
    query: &str,
) -> tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>> {
    let url = format!("{}/api/rooms/{room_id}/ws?{query}", server.ws_base_url);
    let (socket, response) = connect_async(url).await.unwrap();

    assert_eq!(response.status(), StatusCode::SWITCHING_PROTOCOLS);

    socket
}

async fn next_event(
    socket: &mut tokio_tungstenite::WebSocketStream<
        tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
    >,
) -> ServerEvent {
    loop {
        let event = next_event_any(socket).await;
        // ParticipantsUpdated is a high-frequency auxiliary event (fires on
        // join/leave and every drift report). Most tests step through events
        // looking for the domain change they triggered, so silently skip it
        // by default. Tests that need to observe it use next_event_any.
        if matches!(event, ServerEvent::ParticipantsUpdated { .. }) {
            continue;
        }
        return event;
    }
}

async fn next_event_any(
    socket: &mut tokio_tungstenite::WebSocketStream<
        tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
    >,
) -> ServerEvent {
    let message = timeout(Duration::from_secs(2), socket.next())
        .await
        .expect("timed out waiting for websocket event")
        .expect("websocket stream closed")
        .expect("websocket transport error");

    match message {
        Message::Text(text) => serde_json::from_str(text.as_str()).unwrap(),
        other => panic!("expected text websocket message, got {other:?}"),
    }
}

async fn assert_no_event(
    socket: &mut tokio_tungstenite::WebSocketStream<
        tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
    >,
) {
    let deadline = tokio::time::Instant::now() + Duration::from_millis(250);
    loop {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            return;
        }
        match timeout(remaining, socket.next()).await {
            Err(_) => return,
            Ok(Some(Ok(Message::Text(text)))) => {
                let event: ServerEvent = serde_json::from_str(text.as_str()).unwrap();
                if matches!(event, ServerEvent::ParticipantsUpdated { .. }) {
                    continue;
                }
                panic!("expected no websocket event, but one arrived: {event:?}");
            }
            Ok(Some(Ok(_))) => continue,
            Ok(Some(Err(error))) => panic!("websocket transport error: {error:?}"),
            Ok(None) => panic!("websocket stream closed unexpectedly"),
        }
    }
}

fn compression_test_client() -> Client {
    Client::builder()
        .no_gzip()
        .no_zstd()
        .no_brotli()
        .no_deflate()
        .build()
        .unwrap()
}

use reqwest::StatusCode;

#[cfg(unix)]
fn write_probe_script(temp_dir: &TempDir, name: &str, contents: &str) -> PathBuf {
    let path = temp_dir.path().join(name);
    fs::write(&path, contents).unwrap();
    let mut permissions = fs::metadata(&path).unwrap().permissions();
    permissions.set_mode(0o755);
    fs::set_permissions(&path, permissions).unwrap();
    path
}

#[tokio::test]
async fn create_room_with_known_media_id_anchors_room_to_that_item() {
    let temp_dir = TempDir::new().unwrap();
    let root = temp_dir.path().join("library");
    fs::create_dir_all(&root).unwrap();
    fs::write(root.join("feature.mp4"), b"feature").unwrap();
    let server = TestServer::spawn_with_library_roots(vec![root]).await;

    let scan = server.scan_library().await;
    let media_item = scan.items.into_iter().next().expect("indexed media");

    let response = server
        .client
        .post(format!("{}/api/rooms", server.base_url))
        .json(&serde_json::json!({
            "name": "Movie Night",
            "mediaId": media_item.id.to_string(),
        }))
        .send()
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::CREATED);

    let room: Room = response.json().await.unwrap();
    assert_eq!(room.media_id, Some(media_item.id));
    assert_eq!(
        room.media_title.as_deref(),
        Some(media_item.file_name.as_str())
    );

    // A restart should preserve the room-to-media link.
    let restored = server
        .rooms()
        .await
        .rooms
        .into_iter()
        .find(|entry| entry.id == room.id)
        .expect("created room should be listed");

    assert_eq!(restored.media_id, Some(media_item.id));
}

#[tokio::test]
async fn create_room_rejects_unknown_media_id() {
    let server = TestServer::spawn().await;
    let unknown_id = uuid::Uuid::new_v4();

    let response = server
        .client
        .post(format!("{}/api/rooms", server.base_url))
        .json(&serde_json::json!({
            "name": "Movie Night",
            "mediaId": unknown_id.to_string(),
        }))
        .send()
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn create_room_trims_name_and_drops_blank_media_title() {
    let server = TestServer::spawn().await;

    let response = server
        .client
        .post(format!("{}/api/rooms", server.base_url))
        .json(&serde_json::json!({
            "name": "  Movie Lab  ",
            "mediaTitle": "   "
        }))
        .send()
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::CREATED);

    let room: Room = response.json().await.unwrap();
    assert_eq!(room.name, "Movie Lab");
    assert_eq!(room.media_title, None);
    assert!(!room.created_at.is_empty());
    assert_eq!(room.playback_state.anchor_position_seconds, 0.0);
}

#[tokio::test]
async fn library_endpoints_return_empty_snapshot_without_roots() {
    let server = TestServer::spawn().await;

    let library = server.library().await;

    assert!(library.roots.is_empty());
    assert!(library.items.is_empty());

    let scan = server.scan_library().await;

    assert_eq!(scan.scanned_root_count, 0);
    assert_eq!(scan.indexed_item_count, 0);
    assert!(scan.roots.is_empty());
    assert!(scan.items.is_empty());
}

#[tokio::test]
async fn room_listing_is_sorted_alphabetically() {
    let server = TestServer::spawn().await;

    for room_name in ["Zulu Night", "Alpha Night"] {
        let response = server
            .client
            .post(format!("{}/api/rooms", server.base_url))
            .json(&serde_json::json!({
                "name": room_name
            }))
            .send()
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::CREATED);
    }

    let names = server
        .rooms()
        .await
        .rooms
        .into_iter()
        .map(|room| room.name)
        .collect::<Vec<_>>();

    assert_eq!(names, vec!["Alpha Night", "Zulu Night"]);
}

#[tokio::test]
async fn library_scan_indexes_supported_files_and_ignores_other_entries() {
    let temp_dir = TempDir::new().unwrap();
    let root = temp_dir.path().join("library");
    fs::create_dir_all(root.join("nested")).unwrap();
    fs::write(root.join("movie.mp4"), b"movie").unwrap();
    fs::write(root.join("nested").join("episode.mkv"), b"episode").unwrap();
    fs::write(root.join("notes.txt"), b"ignore me").unwrap();
    let canonical_root = fs::canonicalize(&root).unwrap();
    let server = TestServer::spawn_with_library_roots(vec![root]).await;

    let initial_snapshot = server.library().await;
    assert_eq!(initial_snapshot.roots.len(), 1);
    assert_eq!(
        initial_snapshot.roots[0].path,
        canonical_root.to_string_lossy()
    );
    assert_eq!(initial_snapshot.roots[0].last_scanned_at, None);
    assert_eq!(initial_snapshot.roots[0].last_scan_error, None);
    assert!(initial_snapshot.items.is_empty());

    let scan = server.scan_library().await;
    let relative_paths = scan
        .items
        .iter()
        .map(|item| item.relative_path.as_str())
        .collect::<Vec<_>>();

    assert_eq!(scan.scanned_root_count, 1);
    assert_eq!(scan.indexed_item_count, 2);
    assert_eq!(relative_paths, vec!["movie.mp4", "nested/episode.mkv"]);
    assert!(
        scan.items
            .iter()
            .all(|item| item.root_path == canonical_root.to_string_lossy())
    );
    assert!(scan.roots[0].last_scanned_at.is_some());
    assert_eq!(scan.roots[0].last_scan_error, None);
}

#[tokio::test]
async fn library_endpoint_supports_gzip_and_zstd_compression() {
    let temp_dir = TempDir::new().unwrap();
    let root = temp_dir.path().join("library");
    fs::create_dir_all(root.join("nested")).unwrap();
    fs::write(
        root.join("movie-with-a-very-long-name-to-inflate-json.mp4"),
        b"movie",
    )
    .unwrap();
    fs::write(root.join("nested").join("episode-01.mkv"), b"episode").unwrap();
    let server = TestServer::spawn_with_library_roots(vec![root]).await;
    let client = compression_test_client();

    server.scan_library().await;

    for encoding in ["gzip", "zstd"] {
        let response = client
            .get(format!("{}/api/library", server.base_url))
            .header(ACCEPT_ENCODING, encoding)
            .send()
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(response.headers().get(CONTENT_ENCODING).unwrap(), encoding);
        assert!(!response.bytes().await.unwrap().is_empty());
    }
}

#[tokio::test]
async fn library_rescan_updates_changed_files_and_removes_deleted_entries() {
    let temp_dir = TempDir::new().unwrap();
    let root = temp_dir.path().join("library");
    fs::create_dir_all(&root).unwrap();
    let feature = root.join("feature.mp4");
    let bonus = root.join("bonus.webm");
    fs::write(&feature, b"short").unwrap();
    fs::write(&bonus, b"bonus-cut").unwrap();
    let server = TestServer::spawn_with_library_roots(vec![root]).await;

    let first_scan = server.scan_library().await;
    let initial_feature = first_scan
        .items
        .iter()
        .find(|item| item.relative_path == "feature.mp4")
        .unwrap()
        .clone();

    fs::write(&feature, b"feature-length-master").unwrap();
    fs::remove_file(&bonus).unwrap();

    let second_scan = server.scan_library().await;
    let feature_item = second_scan
        .items
        .iter()
        .find(|item| item.relative_path == "feature.mp4")
        .unwrap();

    assert_eq!(second_scan.indexed_item_count, 1);
    assert!(
        second_scan
            .items
            .iter()
            .all(|item| item.relative_path != "bonus.webm")
    );
    assert!(feature_item.size_bytes > initial_feature.size_bytes);
    assert_eq!(feature_item.file_name, "feature.mp4");
}

#[tokio::test]
async fn library_scan_records_missing_root_error_and_clears_items() {
    let temp_dir = TempDir::new().unwrap();
    let root = temp_dir.path().join("library");
    fs::create_dir_all(&root).unwrap();
    fs::write(root.join("movie.mp4"), b"movie").unwrap();
    let server = TestServer::spawn_with_library_roots(vec![root.clone()]).await;

    let first_scan = server.scan_library().await;
    assert_eq!(first_scan.indexed_item_count, 1);

    fs::remove_dir_all(&root).unwrap();

    let second_scan = server.scan_library().await;

    assert_eq!(second_scan.indexed_item_count, 0);
    assert_eq!(second_scan.roots.len(), 1);
    assert!(second_scan.roots[0].last_scan_error.is_some());
    assert!(second_scan.items.is_empty());
}

#[tokio::test]
async fn library_scan_keeps_healthy_roots_when_another_root_fails() {
    let temp_dir = TempDir::new().unwrap();
    let healthy_root = temp_dir.path().join("healthy");
    let missing_root = temp_dir.path().join("missing");
    fs::create_dir_all(&healthy_root).unwrap();
    fs::write(healthy_root.join("movie.mp4"), b"movie").unwrap();
    let healthy_root = fs::canonicalize(&healthy_root).unwrap();
    let server =
        TestServer::spawn_with_library_roots(vec![healthy_root.clone(), missing_root.clone()])
            .await;

    let scan = server.scan_library().await;

    assert_eq!(scan.scanned_root_count, 2);
    assert_eq!(scan.indexed_item_count, 1);

    let healthy_state = scan
        .roots
        .iter()
        .find(|root| root.path == healthy_root.to_string_lossy())
        .unwrap();
    let failed_state = scan
        .roots
        .iter()
        .find(|root| root.path == missing_root.to_string_lossy())
        .unwrap();

    assert!(healthy_state.last_scanned_at.is_some());
    assert_eq!(healthy_state.last_scan_error, None);
    assert!(failed_state.last_scan_error.is_some());
    assert_eq!(scan.items.len(), 1);
    assert_eq!(scan.items[0].root_path, healthy_root.to_string_lossy());
}

#[cfg(unix)]
#[tokio::test]
async fn library_scan_populates_probe_metadata_from_configured_command() {
    let temp_dir = TempDir::new().unwrap();
    let root = temp_dir.path().join("library");
    fs::create_dir_all(&root).unwrap();
    fs::write(root.join("movie.mp4"), b"movie").unwrap();
    let probe_script = write_probe_script(
        &temp_dir,
        "probe-success.sh",
        r#"#!/usr/bin/env bash
set -euo pipefail
cat <<'JSON'
{
  "format": { "format_name": "mov,mp4,m4a,3gp,3g2,mj2", "duration": "95.375" },
  "streams": [
    { "codec_type": "video", "codec_name": "h264", "width": 1920, "height": 1080 },
    { "codec_type": "audio", "codec_name": "aac" }
  ]
}
JSON
"#,
    );
    let server = TestServer::spawn_with_library_config(
        LibraryConfig::from_paths(vec![root]).with_probe_command(probe_script),
    )
    .await;

    let scan = server.scan_library().await;
    assert_eq!(scan.indexed_item_count, 1);

    // Probes now run in the background after the walk returns; refetch
    // once the pool has caught up so we see the populated metadata.
    let library = server.wait_for_probes_complete().await;
    let item = &library.items[0];

    assert_eq!(item.duration_seconds, Some(95.375));
    assert_eq!(
        item.container_name.as_deref(),
        Some("mov,mp4,m4a,3gp,3g2,mj2")
    );
    assert_eq!(item.video_codec.as_deref(), Some("h264"));
    assert_eq!(item.audio_codec.as_deref(), Some("aac"));
    assert_eq!(item.width, Some(1920));
    assert_eq!(item.height, Some(1080));
    assert!(item.probed_at.is_some());
    assert_eq!(item.probe_error, None);
}

#[cfg(unix)]
#[tokio::test]
async fn library_scan_keeps_item_when_probe_command_fails() {
    let temp_dir = TempDir::new().unwrap();
    let root = temp_dir.path().join("library");
    fs::create_dir_all(&root).unwrap();
    fs::write(root.join("movie.mp4"), b"movie").unwrap();
    let probe_script = write_probe_script(
        &temp_dir,
        "probe-fail.sh",
        r#"#!/usr/bin/env bash
set -euo pipefail
echo "synthetic ffprobe failure" >&2
exit 2
"#,
    );
    let server = TestServer::spawn_with_library_config(
        LibraryConfig::from_paths(vec![root]).with_probe_command(probe_script),
    )
    .await;

    let scan = server.scan_library().await;
    assert_eq!(scan.indexed_item_count, 1);

    let library = server.wait_for_probes_complete().await;
    let item = &library.items[0];

    assert!(item.probed_at.is_some());
    assert_eq!(item.duration_seconds, None);
    assert!(
        item.probe_error
            .as_deref()
            .unwrap()
            .contains("synthetic ffprobe failure")
    );
}

#[cfg(unix)]
#[tokio::test]
async fn library_status_reports_revision_and_pending_background_work() {
    let temp_dir = TempDir::new().unwrap();
    let root = temp_dir.path().join("library");
    fs::create_dir_all(&root).unwrap();
    fs::write(root.join("movie.mp4"), b"movie").unwrap();

    let probe_script = write_probe_script(
        &temp_dir,
        "probe-slow.sh",
        r#"#!/usr/bin/env bash
set -euo pipefail
sleep 0.2
cat <<'JSON'
{
  "format": { "format_name": "mov,mp4,m4a,3gp,3g2,mj2", "duration": "120.000" },
  "streams": [
    { "codec_type": "video", "codec_name": "h264", "width": 1920, "height": 1080 },
    { "codec_type": "audio", "codec_name": "aac" }
  ]
}
JSON
"#,
    );
    let ffmpeg_stub = write_ffmpeg_stub(
        &temp_dir,
        "ffmpeg-ok.sh",
        r#"#!/usr/bin/env bash
set -euo pipefail
output="${!#}"
printf 'STUBJPEG' > "$output"
"#,
    );
    let cache_dir = temp_dir.path().join("thumbnails");

    let config = LibraryConfig::from_paths(vec![root])
        .with_probe_command(probe_script)
        .with_ffmpeg_command(ffmpeg_stub)
        .with_thumbnail_cache_dir(&cache_dir);
    let server = TestServer::spawn_with_library_config(config).await;

    let initial_status = server.library_status().await;
    assert!(!initial_status.has_pending_background_work);

    let scan = server.scan_library().await;
    assert!(scan.revision > initial_status.revision);
    assert!(scan.has_pending_background_work);

    let in_progress = server.library_status().await;
    assert!(in_progress.revision >= scan.revision);
    assert!(in_progress.has_pending_background_work);

    let settled_status = server
        .wait_for_library_status(Duration::from_secs(5), |status| {
            !status.has_pending_background_work && status.revision > scan.revision
        })
        .await;
    let settled_library = server.library().await;

    assert_eq!(settled_library.revision, settled_status.revision);
    assert!(!settled_library.has_pending_background_work);
}

/// Builds a probe script that records every invocation by appending a
/// line to `counter_path`. Tests can read the line count to assert
/// exactly how many `ffprobe` calls happened during a scan.
#[cfg(unix)]
fn write_counting_probe_script(
    temp_dir: &TempDir,
    name: &str,
    counter_path: &std::path::Path,
) -> PathBuf {
    let counter_str = counter_path.to_string_lossy();
    let script = format!(
        r#"#!/usr/bin/env bash
set -euo pipefail
echo "$@" >> {counter_str}
cat <<'JSON'
{{
  "format": {{ "format_name": "mov,mp4,m4a,3gp,3g2,mj2", "duration": "10.0" }},
  "streams": [
    {{ "codec_type": "video", "codec_name": "h264", "width": 640, "height": 480 }},
    {{ "codec_type": "audio", "codec_name": "aac" }}
  ]
}}
JSON
"#
    );
    write_probe_script(temp_dir, name, &script)
}

#[cfg(unix)]
fn count_probe_invocations(counter_path: &std::path::Path) -> usize {
    fs::read_to_string(counter_path)
        .map(|contents| contents.lines().filter(|line| !line.is_empty()).count())
        .unwrap_or(0)
}

#[cfg(unix)]
#[tokio::test]
async fn library_rescan_skips_ffprobe_for_unchanged_files() {
    let temp_dir = TempDir::new().unwrap();
    let root = temp_dir.path().join("library");
    fs::create_dir_all(&root).unwrap();
    fs::write(root.join("a.mp4"), b"alpha").unwrap();
    fs::write(root.join("b.mp4"), b"bravo").unwrap();
    fs::write(root.join("c.mp4"), b"charlie").unwrap();

    let counter = temp_dir.path().join("probe-calls.log");
    let probe_script = write_counting_probe_script(&temp_dir, "probe-counted.sh", &counter);

    let server = TestServer::spawn_with_library_config(
        LibraryConfig::from_paths(vec![root]).with_probe_command(probe_script),
    )
    .await;

    server.scan_library().await;
    server.wait_for_probes_complete().await;
    let after_first = count_probe_invocations(&counter);
    assert_eq!(after_first, 3, "first scan should probe every file");

    server.scan_library().await;
    server.wait_for_probes_complete().await;
    let after_second = count_probe_invocations(&counter);
    assert_eq!(
        after_second,
        3,
        "second scan with no changes must reuse cached probe metadata (saw {} new probes)",
        after_second - after_first
    );
}

#[cfg(unix)]
#[tokio::test]
async fn library_rescan_reprobes_only_changed_files() {
    let temp_dir = TempDir::new().unwrap();
    let root = temp_dir.path().join("library");
    fs::create_dir_all(&root).unwrap();
    fs::write(root.join("a.mp4"), b"alpha").unwrap();
    fs::write(root.join("b.mp4"), b"bravo").unwrap();
    fs::write(root.join("c.mp4"), b"charlie").unwrap();

    let counter = temp_dir.path().join("probe-calls.log");
    let probe_script = write_counting_probe_script(&temp_dir, "probe-counted.sh", &counter);

    let server = TestServer::spawn_with_library_config(
        LibraryConfig::from_paths(vec![root.clone()]).with_probe_command(probe_script),
    )
    .await;

    server.scan_library().await;
    server.wait_for_probes_complete().await;
    let baseline = count_probe_invocations(&counter);
    assert_eq!(baseline, 3);

    // Sleep just enough that the modified-time advances at the
    // filesystem's resolution (most modern filesystems give us
    // sub-second mtimes, but a 1.1s sleep is portable to the few that
    // floor to seconds).
    std::thread::sleep(std::time::Duration::from_millis(1100));
    fs::write(root.join("b.mp4"), b"bravo-rewritten-with-new-bytes").unwrap();

    server.scan_library().await;
    server.wait_for_probes_complete().await;
    let after_change = count_probe_invocations(&counter);
    assert_eq!(
        after_change - baseline,
        1,
        "only the modified file should be re-probed (saw {} new probes)",
        after_change - baseline
    );
}

#[tokio::test]
async fn library_scan_discovers_sidecar_subtitles() {
    let temp_dir = TempDir::new().unwrap();
    let root = temp_dir.path().join("library");
    fs::create_dir_all(&root).unwrap();
    fs::write(root.join("movie.mp4"), b"movie").unwrap();
    fs::write(
        root.join("movie.en.srt"),
        b"1\n00:00:01,000 --> 00:00:03,000\nHello there\n",
    )
    .unwrap();
    fs::write(
        root.join("movie.forced.vtt"),
        b"WEBVTT\n\n00:00:01.000 --> 00:00:03.000\nHello there\n",
    )
    .unwrap();
    fs::write(root.join("movie-night.en.srt"), b"ignore").unwrap();
    let server = TestServer::spawn_with_library_roots(vec![root]).await;

    let scan = server.scan_library().await;
    let item = &scan.items[0];

    assert_eq!(item.subtitle_tracks.len(), 2);
    assert_eq!(item.subtitle_tracks[0].relative_path, "movie.en.srt");
    assert_eq!(item.subtitle_tracks[0].label, "EN");
    assert_eq!(item.subtitle_tracks[0].language.as_deref(), Some("en"));
    assert_eq!(item.subtitle_tracks[1].relative_path, "movie.forced.vtt");
    assert_eq!(item.subtitle_tracks[1].label, "Forced");
}

#[tokio::test]
async fn concurrent_room_creation_returns_unique_rooms_and_preserves_all_entries() {
    let server = TestServer::spawn().await;
    let room_names = (0..8)
        .map(|index| format!("Concurrent Room {index}"))
        .collect::<Vec<_>>();

    let created_rooms = join_all(room_names.iter().map(|room_name| {
        let client = server.client.clone();
        let url = format!("{}/api/rooms", server.base_url);

        async move {
            let response = client
                .post(url)
                .json(&serde_json::json!({
                    "name": room_name
                }))
                .send()
                .await
                .unwrap();

            assert_eq!(response.status(), StatusCode::CREATED);

            response.json::<Room>().await.unwrap()
        }
    }))
    .await;

    let created_ids = created_rooms
        .iter()
        .map(|room| room.id)
        .collect::<HashSet<_>>();
    assert_eq!(created_ids.len(), created_rooms.len());

    let listed_rooms = server.rooms().await.rooms;

    assert_eq!(listed_rooms.len(), room_names.len());

    for room_name in room_names {
        assert!(listed_rooms.iter().any(|room| room.name == room_name));
    }
}

#[tokio::test]
async fn startup_does_not_seed_preview_room() {
    let temp_dir = TempDir::new().unwrap();
    let database_path = temp_dir.path().join("syncplay.db");

    let server = TestServer::spawn_persistent(&database_path).await;
    let rooms = server.rooms().await.rooms;

    assert!(rooms.is_empty());
}

#[tokio::test]
async fn library_index_survives_restart() {
    let temp_dir = TempDir::new().unwrap();
    let database_path = temp_dir.path().join("syncplay.db");
    let root = temp_dir.path().join("library");
    fs::create_dir_all(&root).unwrap();
    fs::write(root.join("movie.mp4"), b"movie").unwrap();

    let first_server =
        TestServer::spawn_persistent_with_library(&database_path, vec![root.clone()]).await;
    let first_scan = first_server.scan_library().await;

    assert_eq!(first_scan.indexed_item_count, 1);

    drop(first_server);

    let second_server =
        TestServer::spawn_persistent_with_library(&database_path, vec![root.clone()]).await;
    let restored_library = second_server.library().await;

    assert_eq!(restored_library.roots.len(), 1);
    assert_eq!(restored_library.items.len(), 1);
    assert_eq!(restored_library.items[0].relative_path, "movie.mp4");
    assert!(restored_library.roots[0].last_scanned_at.is_some());
}

#[cfg(unix)]
#[tokio::test]
async fn library_probe_metadata_survives_restart() {
    let temp_dir = TempDir::new().unwrap();
    let database_path = temp_dir.path().join("syncplay.db");
    let root = temp_dir.path().join("library");
    fs::create_dir_all(&root).unwrap();
    fs::write(root.join("movie.mp4"), b"movie").unwrap();
    let probe_script = write_probe_script(
        &temp_dir,
        "probe-restart.sh",
        r#"#!/usr/bin/env bash
set -euo pipefail
cat <<'JSON'
{
  "format": { "format_name": "matroska,webm", "duration": "42.000" },
  "streams": [
    { "codec_type": "video", "codec_name": "vp9", "width": 1280, "height": 720 },
    { "codec_type": "audio", "codec_name": "opus" }
  ]
}
JSON
"#,
    );
    let first_server = TestServer::spawn_persistent_with_library_config(
        &database_path,
        LibraryConfig::from_paths(vec![root.clone()]).with_probe_command(probe_script.clone()),
    )
    .await;

    first_server.scan_library().await;
    let probed = first_server.wait_for_probes_complete().await;
    assert_eq!(probed.items[0].video_codec.as_deref(), Some("vp9"));

    drop(first_server);

    let second_server = TestServer::spawn_persistent_with_library_config(
        &database_path,
        LibraryConfig::from_paths(vec![root]).with_probe_command(probe_script),
    )
    .await;
    let restored_library = second_server.library().await;
    let item = &restored_library.items[0];

    assert_eq!(item.duration_seconds, Some(42.0));
    assert_eq!(item.container_name.as_deref(), Some("matroska,webm"));
    assert_eq!(item.video_codec.as_deref(), Some("vp9"));
    assert_eq!(item.audio_codec.as_deref(), Some("opus"));
    assert_eq!(item.width, Some(1280));
    assert_eq!(item.height, Some(720));
    assert!(item.probed_at.is_some());
    assert_eq!(item.probe_error, None);
}

#[tokio::test]
async fn library_subtitles_survive_restart() {
    let temp_dir = TempDir::new().unwrap();
    let database_path = temp_dir.path().join("syncplay.db");
    let root = temp_dir.path().join("library");
    fs::create_dir_all(&root).unwrap();
    fs::write(root.join("movie.mp4"), b"movie").unwrap();
    fs::write(
        root.join("movie.en.srt"),
        b"1\n00:00:01,000 --> 00:00:03,000\nHello there\n",
    )
    .unwrap();

    let first_server =
        TestServer::spawn_persistent_with_library(&database_path, vec![root.clone()]).await;
    let first_scan = first_server.scan_library().await;
    assert_eq!(first_scan.items[0].subtitle_tracks.len(), 1);

    drop(first_server);

    let second_server = TestServer::spawn_persistent_with_library(&database_path, vec![root]).await;
    let restored_library = second_server.library().await;

    assert_eq!(restored_library.items[0].subtitle_tracks.len(), 1);
    assert_eq!(
        restored_library.items[0].subtitle_tracks[0].relative_path,
        "movie.en.srt"
    );
}

#[tokio::test]
async fn removing_a_library_root_from_config_cascades_its_indexed_items_on_restart() {
    let temp_dir = TempDir::new().unwrap();
    let database_path = temp_dir.path().join("syncplay.db");
    let movies_root = temp_dir.path().join("movies");
    let music_root = temp_dir.path().join("music");
    fs::create_dir_all(&movies_root).unwrap();
    fs::create_dir_all(&music_root).unwrap();
    fs::write(movies_root.join("arrival.mp4"), b"movie").unwrap();
    fs::write(music_root.join("theme.mp3"), b"audio").unwrap();

    let first_server = TestServer::spawn_persistent_with_library(
        &database_path,
        vec![movies_root.clone(), music_root.clone()],
    )
    .await;
    let first_scan = first_server.scan_library().await;
    assert_eq!(first_scan.roots.len(), 2);
    assert_eq!(first_scan.items.len(), 2);

    drop(first_server);

    let second_server =
        TestServer::spawn_persistent_with_library(&database_path, vec![movies_root.clone()]).await;
    let restored_library = second_server.library().await;

    assert_eq!(restored_library.roots.len(), 1);
    assert_eq!(restored_library.items.len(), 1);
    assert_eq!(
        restored_library.roots[0].path,
        fs::canonicalize(&movies_root).unwrap().to_string_lossy()
    );
    assert_eq!(restored_library.items[0].relative_path, "arrival.mp4");
    assert_eq!(
        restored_library.items[0].root_path,
        fs::canonicalize(&movies_root).unwrap().to_string_lossy()
    );
}

#[tokio::test]
async fn media_stream_returns_full_file_when_no_range_is_requested() {
    let temp_dir = TempDir::new().unwrap();
    let root = temp_dir.path().join("library");
    fs::create_dir_all(&root).unwrap();
    let expected_bytes = b"stream-me".to_vec();
    fs::write(root.join("movie.mp4"), &expected_bytes).unwrap();
    let server = TestServer::spawn_with_library_roots(vec![root]).await;
    let scan = server.scan_library().await;
    let media_id = scan.items[0].id;

    let response = server
        .client
        .get(format!("{}/api/media/{media_id}/stream", server.base_url))
        .send()
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(response.headers().get("accept-ranges").unwrap(), "bytes");
    assert_eq!(response.headers().get("content-type").unwrap(), "video/mp4");
    assert_eq!(
        response.bytes().await.unwrap().as_ref(),
        expected_bytes.as_slice()
    );
}

#[tokio::test]
async fn media_stream_skips_response_compression_for_video() {
    let temp_dir = TempDir::new().unwrap();
    let root = temp_dir.path().join("library");
    fs::create_dir_all(&root).unwrap();
    let expected_bytes = b"stream-me".to_vec();
    fs::write(root.join("movie.mp4"), &expected_bytes).unwrap();
    let server = TestServer::spawn_with_library_roots(vec![root]).await;
    let client = compression_test_client();
    let scan = server.scan_library().await;
    let media_id = scan.items[0].id;

    for encoding in ["gzip", "zstd"] {
        let response = client
            .get(format!("{}/api/media/{media_id}/stream", server.base_url))
            .header(ACCEPT_ENCODING, encoding)
            .send()
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        assert!(response.headers().get(CONTENT_ENCODING).is_none());
        assert_eq!(
            response.bytes().await.unwrap().as_ref(),
            expected_bytes.as_slice()
        );
    }
}

#[tokio::test]
async fn media_stream_supports_explicit_byte_ranges() {
    let temp_dir = TempDir::new().unwrap();
    let root = temp_dir.path().join("library");
    fs::create_dir_all(&root).unwrap();
    fs::write(root.join("movie.mp4"), b"abcdefghij").unwrap();
    let server = TestServer::spawn_with_library_roots(vec![root]).await;
    let scan = server.scan_library().await;
    let media_id = scan.items[0].id;

    let response = server
        .client
        .get(format!("{}/api/media/{media_id}/stream", server.base_url))
        .header(RANGE, "bytes=2-5")
        .send()
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::PARTIAL_CONTENT);
    assert_eq!(
        response.headers().get(CONTENT_RANGE).unwrap(),
        "bytes 2-5/10"
    );
    assert_eq!(response.bytes().await.unwrap().as_ref(), b"cdef");
}

#[tokio::test]
async fn media_stream_supports_suffix_byte_ranges() {
    let temp_dir = TempDir::new().unwrap();
    let root = temp_dir.path().join("library");
    fs::create_dir_all(&root).unwrap();
    fs::write(root.join("movie.mp4"), b"abcdefghij").unwrap();
    let server = TestServer::spawn_with_library_roots(vec![root]).await;
    let scan = server.scan_library().await;
    let media_id = scan.items[0].id;

    let response = server
        .client
        .get(format!("{}/api/media/{media_id}/stream", server.base_url))
        .header(RANGE, "bytes=-3")
        .send()
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::PARTIAL_CONTENT);
    assert_eq!(
        response.headers().get(CONTENT_RANGE).unwrap(),
        "bytes 7-9/10"
    );
    assert_eq!(response.bytes().await.unwrap().as_ref(), b"hij");
}

#[tokio::test]
async fn media_stream_rejects_unsatisfiable_ranges() {
    let temp_dir = TempDir::new().unwrap();
    let root = temp_dir.path().join("library");
    fs::create_dir_all(&root).unwrap();
    fs::write(root.join("movie.mp4"), b"abcdefghij").unwrap();
    let server = TestServer::spawn_with_library_roots(vec![root]).await;
    let scan = server.scan_library().await;
    let media_id = scan.items[0].id;

    let response = server
        .client
        .get(format!("{}/api/media/{media_id}/stream", server.base_url))
        .header(RANGE, "bytes=50-60")
        .send()
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::RANGE_NOT_SATISFIABLE);
    assert_eq!(response.headers().get(CONTENT_RANGE).unwrap(), "bytes */10");
}

#[tokio::test]
async fn media_stream_returns_not_found_when_indexed_file_disappears() {
    let temp_dir = TempDir::new().unwrap();
    let root = temp_dir.path().join("library");
    fs::create_dir_all(&root).unwrap();
    let media_path = root.join("movie.mp4");
    fs::write(&media_path, b"abcdefghij").unwrap();
    let server = TestServer::spawn_with_library_roots(vec![root]).await;
    let scan = server.scan_library().await;
    let media_id = scan.items[0].id;

    fs::remove_file(media_path).unwrap();

    let response = server
        .client
        .get(format!("{}/api/media/{media_id}/stream", server.base_url))
        .send()
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn subtitle_stream_converts_srt_sidecars_to_webvtt() {
    let temp_dir = TempDir::new().unwrap();
    let root = temp_dir.path().join("library");
    fs::create_dir_all(&root).unwrap();
    fs::write(root.join("movie.mp4"), b"abcdefghij").unwrap();
    fs::write(
        root.join("movie.en.srt"),
        b"1\n00:00:01,000 --> 00:00:03,000\nHello, world\n",
    )
    .unwrap();
    let server = TestServer::spawn_with_library_roots(vec![root]).await;
    let scan = server.scan_library().await;
    let media_id = scan.items[0].id;

    let response = server
        .client
        .get(format!(
            "{}/api/media/{media_id}/subtitles/0",
            server.base_url
        ))
        .send()
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response.headers().get("content-type").unwrap(),
        "text/vtt; charset=utf-8"
    );
    assert_eq!(
        response.text().await.unwrap(),
        "WEBVTT\n\n1\n00:00:01.000 --> 00:00:03.000\nHello, world\n"
    );
}

#[tokio::test]
async fn subtitle_stream_returns_not_found_when_sidecar_disappears() {
    let temp_dir = TempDir::new().unwrap();
    let root = temp_dir.path().join("library");
    fs::create_dir_all(&root).unwrap();
    fs::write(root.join("movie.mp4"), b"abcdefghij").unwrap();
    let subtitle_path = root.join("movie.en.vtt");
    fs::write(
        &subtitle_path,
        b"WEBVTT\n\n00:00:01.000 --> 00:00:03.000\nHello\n",
    )
    .unwrap();
    let server = TestServer::spawn_with_library_roots(vec![root]).await;
    let scan = server.scan_library().await;
    let media_id = scan.items[0].id;

    fs::remove_file(subtitle_path).unwrap();

    let response = server
        .client
        .get(format!(
            "{}/api/media/{media_id}/subtitles/0",
            server.base_url
        ))
        .send()
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::NOT_FOUND);
}

#[cfg(unix)]
#[tokio::test]
async fn embedded_subtitle_stream_converts_text_tracks_to_webvtt() {
    let temp_dir = TempDir::new().unwrap();
    let root = temp_dir.path().join("library");
    fs::create_dir_all(&root).unwrap();
    fs::write(root.join("movie.mkv"), b"synthetic").unwrap();

    let probe_script = write_probe_script(
        &temp_dir,
        "probe-text-subtitle.sh",
        EMBEDDED_TEXT_PROBE_JSON,
    );
    let ffmpeg_stub = write_ffmpeg_stub(&temp_dir, "ffmpeg-webvtt.sh", FFMPEG_WEBVTT_OK_STUB);
    let cache_dir = temp_dir.path().join("stream-copies");
    let server = TestServer::spawn_with_library_config(stream_copy_config(
        root,
        probe_script,
        Some(ffmpeg_stub),
        cache_dir,
    ))
    .await;

    server.scan_library().await;
    let item = server
        .wait_for_probes_complete()
        .await
        .items
        .into_iter()
        .next()
        .unwrap();

    let response = server
        .client
        .get(format!(
            "{}/api/media/{}/subtitles/embedded/2",
            server.base_url, item.id
        ))
        .send()
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response.headers().get("content-type").unwrap(),
        "text/vtt; charset=utf-8"
    );
    let body = response.text().await.unwrap();
    assert!(body.starts_with("WEBVTT"));
    assert!(body.contains("Hello from embedded subtitle"));
}

#[tokio::test]
async fn websocket_returns_not_found_for_unknown_room() {
    let server = TestServer::spawn().await;
    let missing_room_id = "4a94c646-1a0b-4a1e-b518-c3b3bb5e5fd3";
    let url = format!(
        "{}/api/rooms/{missing_room_id}/ws?clientName=Ghost",
        server.ws_base_url
    );

    let error = connect_async(url).await.unwrap_err();

    match error {
        tokio_tungstenite::tungstenite::Error::Http(response) => {
            assert_eq!(response.status(), StatusCode::NOT_FOUND);
        }
        other => panic!("expected http error, got {other:?}"),
    }
}

#[tokio::test]
async fn websocket_malformed_payload_returns_parse_error_and_recovers() {
    let server = TestServer::spawn().await;
    let room = server.seeded_room().await;
    let mut socket = connect_room_socket(&server, &room.id.to_string(), "Operator").await;

    assert!(matches!(
        next_event(&mut socket).await,
        ServerEvent::Snapshot { .. }
    ));
    assert!(matches!(
        next_event(&mut socket).await,
        ServerEvent::PresenceChanged { joined: true, .. }
    ));

    socket
        .send(Message::Text("definitely-not-json".into()))
        .await
        .unwrap();

    match next_event(&mut socket).await {
        ServerEvent::Error { message } => {
            assert_eq!(message, "Could not parse the playback command.");
        }
        other => panic!("expected parse error event, got {other:?}"),
    }

    socket
        .send(Message::Text(
            serde_json::json!({
                "type": "play",
                "positionSeconds": 4.0
            })
            .to_string()
            .into(),
        ))
        .await
        .unwrap();

    match next_event(&mut socket).await {
        ServerEvent::PlaybackUpdated { room, .. } => {
            assert_eq!(room.playback_state.position_seconds, 4.0);
        }
        other => panic!("expected playback update, got {other:?}"),
    }
}

#[tokio::test]
async fn persisted_room_state_survives_restart() {
    let temp_dir = TempDir::new().unwrap();
    let database_path = temp_dir.path().join("syncplay.db");

    let first_server = TestServer::spawn_persistent(&database_path).await;
    let create_response = first_server
        .client
        .post(format!("{}/api/rooms", first_server.base_url))
        .json(&serde_json::json!({
            "name": "Persistent Room",
            "mediaTitle": "Arrival"
        }))
        .send()
        .await
        .unwrap();

    assert_eq!(create_response.status(), StatusCode::CREATED);
    let created_room: Room = create_response.json().await.unwrap();

    let mut socket =
        connect_room_socket(&first_server, &created_room.id.to_string(), "Operator").await;

    let _ = next_event(&mut socket).await;
    let _ = next_event(&mut socket).await;

    socket
        .send(Message::Text(
            serde_json::json!({
                "type": "seek",
                "positionSeconds": 6.4
            })
            .to_string()
            .into(),
        ))
        .await
        .unwrap();

    match next_event(&mut socket).await {
        ServerEvent::PlaybackUpdated { room, .. } => {
            assert_eq!(room.playback_state.position_seconds, 6.4);
        }
        other => panic!("expected persisted playback update, got {other:?}"),
    }

    drop(socket);
    drop(first_server);

    let second_server = TestServer::spawn_persistent(&database_path).await;
    let restored_room = second_server
        .rooms()
        .await
        .rooms
        .into_iter()
        .find(|room| room.id == created_room.id)
        .expect("room should be restored after restart");

    assert_eq!(restored_room.name, "Persistent Room");
    assert_eq!(restored_room.media_title.as_deref(), Some("Arrival"));
    assert_eq!(restored_room.playback_state.position_seconds, 6.4);
    assert_eq!(restored_room.playback_state.anchor_position_seconds, 6.4);
}

#[tokio::test]
async fn websocket_invalid_seek_keeps_connection_usable() {
    let server = TestServer::spawn().await;
    let room = server.seeded_room().await;
    let mut socket = connect_room_socket(&server, &room.id.to_string(), "Operator").await;

    assert!(matches!(
        next_event(&mut socket).await,
        ServerEvent::Snapshot { .. }
    ));
    assert!(matches!(
        next_event(&mut socket).await,
        ServerEvent::PresenceChanged { joined: true, .. }
    ));

    socket
        .send(Message::Text(
            serde_json::json!({
                "type": "seek",
                "positionSeconds": -2.0
            })
            .to_string()
            .into(),
        ))
        .await
        .unwrap();

    match next_event(&mut socket).await {
        ServerEvent::Error { message } => {
            assert_eq!(message, "Playback position cannot be negative.");
        }
        other => panic!("expected error event, got {other:?}"),
    }

    socket
        .send(Message::Text(
            serde_json::json!({
                "type": "seek",
                "positionSeconds": 9.2
            })
            .to_string()
            .into(),
        ))
        .await
        .unwrap();

    match next_event(&mut socket).await {
        ServerEvent::PlaybackUpdated { room, .. } => {
            assert_eq!(room.playback_state.position_seconds, 9.2);
        }
        other => panic!("expected playback update, got {other:?}"),
    }
}

#[tokio::test]
async fn websocket_blank_client_name_uses_fallback_viewer_id() {
    let server = TestServer::spawn().await;
    let room = server.seeded_room().await;
    let mut socket =
        connect_room_socket_with_query(&server, &room.id.to_string(), "clientName=").await;

    assert!(matches!(
        next_event(&mut socket).await,
        ServerEvent::Snapshot { .. }
    ));

    match next_event(&mut socket).await {
        ServerEvent::PresenceChanged {
            actor,
            joined,
            connection_count,
            ..
        } => {
            assert!(actor.starts_with("viewer-"));
            assert!(joined);
            assert_eq!(connection_count, 1);
        }
        other => panic!("expected fallback presence event, got {other:?}"),
    }
}

#[tokio::test]
async fn pause_without_position_uses_authoritative_clock_progress() {
    let server = TestServer::spawn().await;
    let room = server.seeded_room().await;
    let mut socket = connect_room_socket(&server, &room.id.to_string(), "Operator").await;

    let _ = next_event(&mut socket).await;
    let _ = next_event(&mut socket).await;

    socket
        .send(Message::Text(
            serde_json::json!({
                "type": "play",
                "positionSeconds": 5.0
            })
            .to_string()
            .into(),
        ))
        .await
        .unwrap();

    let _ = next_event(&mut socket).await;

    tokio::time::sleep(Duration::from_millis(260)).await;

    socket
        .send(Message::Text(
            serde_json::json!({ "type": "pause" }).to_string().into(),
        ))
        .await
        .unwrap();

    match next_event(&mut socket).await {
        ServerEvent::PlaybackUpdated { room, .. } => {
            assert_eq!(
                room.playback_state.status,
                syncplay_backend::protocol::PlaybackStatus::Paused
            );
            assert!(room.playback_state.position_seconds >= 5.2);
            assert_eq!(
                room.playback_state.position_seconds,
                room.playback_state.anchor_position_seconds
            );
        }
        other => panic!("expected playback update, got {other:?}"),
    }
}

#[tokio::test]
async fn second_client_join_and_leave_updates_presence_counts() {
    let server = TestServer::spawn().await;
    let room = server.seeded_room().await;
    let mut first = connect_room_socket(&server, &room.id.to_string(), "Alpha").await;

    assert!(matches!(
        next_event(&mut first).await,
        ServerEvent::Snapshot { .. }
    ));
    assert!(matches!(
        next_event(&mut first).await,
        ServerEvent::PresenceChanged {
            joined: true,
            connection_count: 1,
            ..
        }
    ));

    let mut second = connect_room_socket(&server, &room.id.to_string(), "Bravo").await;

    match next_event(&mut second).await {
        ServerEvent::Snapshot {
            connection_count, ..
        } => {
            assert_eq!(connection_count, 2);
        }
        other => panic!("expected second snapshot, got {other:?}"),
    }

    match next_event(&mut first).await {
        ServerEvent::PresenceChanged {
            actor,
            joined,
            connection_count,
            ..
        } => {
            assert_eq!(actor, "Bravo");
            assert!(joined);
            assert_eq!(connection_count, 2);
        }
        other => panic!("expected join broadcast on first client, got {other:?}"),
    }

    match next_event(&mut second).await {
        ServerEvent::PresenceChanged {
            actor,
            joined,
            connection_count,
            ..
        } => {
            assert_eq!(actor, "Bravo");
            assert!(joined);
            assert_eq!(connection_count, 2);
        }
        other => panic!("expected self join presence event, got {other:?}"),
    }

    second.send(Message::Close(None)).await.unwrap();

    match next_event(&mut first).await {
        ServerEvent::PresenceChanged {
            actor,
            joined,
            connection_count,
            ..
        } => {
            assert_eq!(actor, "Bravo");
            assert!(!joined);
            assert_eq!(connection_count, 1);
        }
        other => panic!("expected leave broadcast on first client, got {other:?}"),
    }
}

#[tokio::test]
async fn reconnect_churn_keeps_presence_counts_stable_for_primary_client() {
    let server = TestServer::spawn().await;
    let room = server.seeded_room().await;
    let mut primary = connect_room_socket(&server, &room.id.to_string(), "Alpha").await;

    assert!(matches!(
        next_event(&mut primary).await,
        ServerEvent::Snapshot { .. }
    ));
    assert!(matches!(
        next_event(&mut primary).await,
        ServerEvent::PresenceChanged {
            joined: true,
            connection_count: 1,
            ..
        }
    ));

    for actor in ["Bravo-1", "Bravo-2", "Bravo-3"] {
        let mut transient = connect_room_socket(&server, &room.id.to_string(), actor).await;

        match next_event(&mut transient).await {
            ServerEvent::Snapshot {
                connection_count, ..
            } => assert_eq!(connection_count, 2),
            other => panic!("expected transient snapshot, got {other:?}"),
        }

        match next_event(&mut primary).await {
            ServerEvent::PresenceChanged {
                actor: reported_actor,
                joined,
                connection_count,
                ..
            } => {
                assert_eq!(reported_actor, actor);
                assert!(joined);
                assert_eq!(connection_count, 2);
            }
            other => panic!("expected primary join event, got {other:?}"),
        }

        match next_event(&mut transient).await {
            ServerEvent::PresenceChanged {
                actor: reported_actor,
                joined,
                connection_count,
                ..
            } => {
                assert_eq!(reported_actor, actor);
                assert!(joined);
                assert_eq!(connection_count, 2);
            }
            other => panic!("expected transient self join event, got {other:?}"),
        }

        transient.send(Message::Close(None)).await.unwrap();

        match next_event(&mut primary).await {
            ServerEvent::PresenceChanged {
                actor: reported_actor,
                joined,
                connection_count,
                ..
            } => {
                assert_eq!(reported_actor, actor);
                assert!(!joined);
                assert_eq!(connection_count, 1);
            }
            other => panic!("expected primary leave event, got {other:?}"),
        }
    }
}

#[tokio::test]
async fn playback_updates_broadcast_in_order_to_other_clients() {
    let server = TestServer::spawn().await;
    let room = server.seeded_room().await;
    let mut controller = connect_room_socket(&server, &room.id.to_string(), "Alpha").await;

    assert!(matches!(
        next_event(&mut controller).await,
        ServerEvent::Snapshot { .. }
    ));
    assert!(matches!(
        next_event(&mut controller).await,
        ServerEvent::PresenceChanged {
            joined: true,
            connection_count: 1,
            ..
        }
    ));

    let mut observer = connect_room_socket(&server, &room.id.to_string(), "Bravo").await;

    assert!(matches!(
        next_event(&mut observer).await,
        ServerEvent::Snapshot {
            connection_count: 2,
            ..
        }
    ));
    assert!(matches!(
        next_event(&mut controller).await,
        ServerEvent::PresenceChanged {
            actor,
            joined: true,
            connection_count: 2,
            ..
        } if actor == "Bravo"
    ));
    assert!(matches!(
        next_event(&mut observer).await,
        ServerEvent::PresenceChanged {
            actor,
            joined: true,
            connection_count: 2,
            ..
        } if actor == "Bravo"
    ));

    controller
        .send(Message::Text(
            serde_json::json!({
                "type": "play",
                "positionSeconds": 3.0
            })
            .to_string()
            .into(),
        ))
        .await
        .unwrap();

    controller
        .send(Message::Text(
            serde_json::json!({
                "type": "seek",
                "positionSeconds": 7.5
            })
            .to_string()
            .into(),
        ))
        .await
        .unwrap();

    match next_event(&mut observer).await {
        ServerEvent::PlaybackUpdated {
            actor,
            action,
            room,
        } => {
            assert_eq!(actor, "Alpha");
            assert_eq!(action, syncplay_backend::protocol::PlaybackAction::Play);
            assert_eq!(room.playback_state.position_seconds, 3.0);
        }
        other => panic!("expected play update on observer, got {other:?}"),
    }

    match next_event(&mut observer).await {
        ServerEvent::PlaybackUpdated {
            actor,
            action,
            room,
        } => {
            assert_eq!(actor, "Alpha");
            assert_eq!(action, syncplay_backend::protocol::PlaybackAction::Seek);
            assert_eq!(room.playback_state.position_seconds, 7.5);
        }
        other => panic!("expected seek update on observer, got {other:?}"),
    }
}

#[tokio::test]
async fn drift_report_suggests_seek_when_client_is_far_off() {
    let server = TestServer::spawn().await;
    let room = server.seeded_room().await;
    let mut socket = connect_room_socket(&server, &room.id.to_string(), "Operator").await;

    let _ = next_event(&mut socket).await;
    let _ = next_event(&mut socket).await;

    socket
        .send(Message::Text(
            serde_json::json!({
                "type": "play",
                "positionSeconds": 10.0
            })
            .to_string()
            .into(),
        ))
        .await
        .unwrap();

    let _ = next_event(&mut socket).await;

    socket
        .send(Message::Text(
            serde_json::json!({
                "type": "reportPosition",
                "positionSeconds": 12.6
            })
            .to_string()
            .into(),
        ))
        .await
        .unwrap();

    match next_event(&mut socket).await {
        ServerEvent::DriftCorrection {
            suggested_action,
            delta_seconds,
            expected_position_seconds,
            ..
        } => {
            assert_eq!(suggested_action, DriftCorrectionAction::Seek);
            assert!(delta_seconds > 2.0);
            assert!(expected_position_seconds >= 10.0);
        }
        other => panic!("expected drift correction event, got {other:?}"),
    }
}

#[tokio::test]
async fn drift_report_classifies_in_sync_and_nudge_thresholds() {
    let server = TestServer::spawn().await;
    let room = server.seeded_room().await;
    let mut socket = connect_room_socket(&server, &room.id.to_string(), "Operator").await;

    let _ = next_event(&mut socket).await;
    let _ = next_event(&mut socket).await;

    socket
        .send(Message::Text(
            serde_json::json!({
                "type": "play",
                "positionSeconds": 10.0
            })
            .to_string()
            .into(),
        ))
        .await
        .unwrap();

    let _ = next_event(&mut socket).await;

    socket
        .send(Message::Text(
            serde_json::json!({
                "type": "reportPosition",
                "positionSeconds": 10.2
            })
            .to_string()
            .into(),
        ))
        .await
        .unwrap();

    match next_event(&mut socket).await {
        ServerEvent::DriftCorrection {
            suggested_action,
            delta_seconds,
            ..
        } => {
            assert_eq!(suggested_action, DriftCorrectionAction::InSync);
            assert!(delta_seconds.abs() <= 0.35);
        }
        other => panic!("expected in-sync drift correction event, got {other:?}"),
    }

    socket
        .send(Message::Text(
            serde_json::json!({
                "type": "reportPosition",
                "positionSeconds": 10.9
            })
            .to_string()
            .into(),
        ))
        .await
        .unwrap();

    match next_event(&mut socket).await {
        ServerEvent::DriftCorrection {
            suggested_action,
            delta_seconds,
            ..
        } => {
            assert_eq!(suggested_action, DriftCorrectionAction::Nudge);
            assert!(delta_seconds.abs() > 0.35);
            assert!(delta_seconds.abs() <= 1.5);
        }
        other => panic!("expected nudge drift correction event, got {other:?}"),
    }
}

#[tokio::test]
async fn drift_reports_are_not_broadcast_to_other_clients() {
    let server = TestServer::spawn().await;
    let room = server.seeded_room().await;
    let mut reporter = connect_room_socket(&server, &room.id.to_string(), "Alpha").await;

    assert!(matches!(
        next_event(&mut reporter).await,
        ServerEvent::Snapshot { .. }
    ));
    assert!(matches!(
        next_event(&mut reporter).await,
        ServerEvent::PresenceChanged {
            joined: true,
            connection_count: 1,
            ..
        }
    ));

    let mut observer = connect_room_socket(&server, &room.id.to_string(), "Bravo").await;

    let _ = next_event(&mut observer).await;
    let _ = next_event(&mut reporter).await;
    let _ = next_event(&mut observer).await;

    reporter
        .send(Message::Text(
            serde_json::json!({
                "type": "play",
                "positionSeconds": 10.0
            })
            .to_string()
            .into(),
        ))
        .await
        .unwrap();

    let _ = next_event(&mut reporter).await;
    let _ = next_event(&mut observer).await;

    reporter
        .send(Message::Text(
            serde_json::json!({
                "type": "reportPosition",
                "positionSeconds": 12.8
            })
            .to_string()
            .into(),
        ))
        .await
        .unwrap();

    match next_event(&mut reporter).await {
        ServerEvent::DriftCorrection {
            suggested_action, ..
        } => {
            assert_eq!(suggested_action, DriftCorrectionAction::Seek);
        }
        other => panic!("expected direct drift correction on reporter, got {other:?}"),
    }

    assert_no_event(&mut observer).await;
}

#[tokio::test]
async fn ping_returns_pong_with_echoed_timestamp() {
    let server = TestServer::spawn().await;
    let room = server.seeded_room().await;
    let mut socket = connect_room_socket(&server, &room.id.to_string(), "Operator").await;

    let _ = next_event(&mut socket).await;
    let _ = next_event(&mut socket).await;

    socket
        .send(Message::Text(
            serde_json::json!({
                "type": "ping",
                "clientSentAtMs": 1_700_000_000_123_i64
            })
            .to_string()
            .into(),
        ))
        .await
        .unwrap();

    match next_event(&mut socket).await {
        ServerEvent::Pong { client_sent_at_ms } => {
            assert_eq!(client_sent_at_ms, 1_700_000_000_123);
        }
        other => panic!("expected pong, got {other:?}"),
    }
}

#[tokio::test]
async fn ping_is_not_broadcast_to_other_clients() {
    let server = TestServer::spawn().await;
    let room = server.seeded_room().await;
    let mut reporter = connect_room_socket(&server, &room.id.to_string(), "Alpha").await;

    assert!(matches!(
        next_event(&mut reporter).await,
        ServerEvent::Snapshot { .. }
    ));
    assert!(matches!(
        next_event(&mut reporter).await,
        ServerEvent::PresenceChanged {
            joined: true,
            connection_count: 1,
            ..
        }
    ));

    let mut observer = connect_room_socket(&server, &room.id.to_string(), "Bravo").await;

    let _ = next_event(&mut observer).await;
    let _ = next_event(&mut reporter).await;
    let _ = next_event(&mut observer).await;

    reporter
        .send(Message::Text(
            serde_json::json!({ "type": "ping", "clientSentAtMs": 42 })
                .to_string()
                .into(),
        ))
        .await
        .unwrap();

    match next_event(&mut reporter).await {
        ServerEvent::Pong { client_sent_at_ms } => assert_eq!(client_sent_at_ms, 42),
        other => panic!("expected pong on reporter, got {other:?}"),
    }

    assert_no_event(&mut observer).await;
}

#[tokio::test]
async fn play_with_client_one_way_ms_back_dates_the_clock_anchor() {
    let server = TestServer::spawn().await;
    let room = server.seeded_room().await;
    let mut socket = connect_room_socket(&server, &room.id.to_string(), "Operator").await;

    let _ = next_event(&mut socket).await;
    let _ = next_event(&mut socket).await;

    socket
        .send(Message::Text(
            serde_json::json!({
                "type": "play",
                "positionSeconds": 10.0,
                "clientOneWayMs": 300
            })
            .to_string()
            .into(),
        ))
        .await
        .unwrap();

    match next_event(&mut socket).await {
        ServerEvent::PlaybackUpdated { room, .. } => {
            assert_eq!(room.playback_state.anchor_position_seconds, 10.0);
            // Anchor is placed 300 ms in the past, so effective position at
            // emit time has advanced past the reported 10.0.
            assert!(
                room.playback_state.position_seconds >= 10.3,
                "expected effective position >= 10.3 s, got {}",
                room.playback_state.position_seconds
            );
            assert!(
                room.playback_state.position_seconds <= 10.5,
                "expected effective position to stay near the back-date, got {}",
                room.playback_state.position_seconds
            );
        }
        other => panic!("expected playback update, got {other:?}"),
    }
}

#[tokio::test]
async fn client_one_way_ms_above_ceiling_is_clamped() {
    let server = TestServer::spawn().await;
    let room = server.seeded_room().await;
    let mut socket = connect_room_socket(&server, &room.id.to_string(), "Operator").await;

    let _ = next_event(&mut socket).await;
    let _ = next_event(&mut socket).await;

    socket
        .send(Message::Text(
            serde_json::json!({
                "type": "play",
                "positionSeconds": 10.0,
                "clientOneWayMs": 10_000
            })
            .to_string()
            .into(),
        ))
        .await
        .unwrap();

    match next_event(&mut socket).await {
        ServerEvent::PlaybackUpdated { room, .. } => {
            assert_eq!(room.playback_state.anchor_position_seconds, 10.0);
            // Server caps the back-date at 2 s; position should land close to
            // 12.0 but never run wildly ahead.
            assert!(
                (12.0..=12.3).contains(&room.playback_state.position_seconds),
                "expected clamped back-date to yield ~12.0 s, got {}",
                room.playback_state.position_seconds
            );
        }
        other => panic!("expected playback update, got {other:?}"),
    }
}

#[cfg(unix)]
fn write_ffmpeg_stub(temp_dir: &TempDir, name: &str, contents: &str) -> PathBuf {
    write_probe_script(temp_dir, name, contents)
}

#[cfg(unix)]
const SUCCESSFUL_PROBE_JSON: &str = r#"#!/usr/bin/env bash
set -euo pipefail
cat <<'JSON'
{
  "format": { "format_name": "mov,mp4,m4a,3gp,3g2,mj2", "duration": "120.000" },
  "streams": [
    { "codec_type": "video", "codec_name": "h264", "width": 1920, "height": 1080 },
    { "codec_type": "audio", "codec_name": "aac" }
  ]
}
JSON
"#;

#[cfg(unix)]
#[tokio::test]
async fn library_scan_generates_thumbnail_for_video_media() {
    let temp_dir = TempDir::new().unwrap();
    let root = temp_dir.path().join("library");
    fs::create_dir_all(&root).unwrap();
    fs::write(root.join("movie.mp4"), b"movie").unwrap();

    let probe_script = write_probe_script(&temp_dir, "probe-ok.sh", SUCCESSFUL_PROBE_JSON);
    let ffmpeg_stub = write_ffmpeg_stub(
        &temp_dir,
        "ffmpeg-ok.sh",
        r#"#!/usr/bin/env bash
set -euo pipefail
# Parse ffmpeg-style arguments to find the output path (last positional arg).
output="${!#}"
printf 'STUBJPEG' > "$output"
"#,
    );
    let cache_dir = temp_dir.path().join("thumbnails");

    let config = LibraryConfig::from_paths(vec![root])
        .with_probe_command(probe_script)
        .with_ffmpeg_command(ffmpeg_stub)
        .with_thumbnail_cache_dir(&cache_dir);
    let server = TestServer::spawn_with_library_config(config).await;

    let _ = server.scan_library().await;
    let item = server
        .poll_first_item_until(Duration::from_secs(5), |item| {
            item.thumbnail_generated_at.is_some()
        })
        .await;

    assert!(item.thumbnail_generated_at.is_some());
    assert_eq!(item.thumbnail_error, None);

    let thumbnail_path = cache_dir.join(format!("{}.jpg", item.id));
    assert!(
        thumbnail_path.exists(),
        "thumbnail should exist at {}",
        thumbnail_path.display()
    );

    let response = server
        .client
        .get(format!(
            "{}/api/media/{}/thumbnail",
            server.base_url, item.id
        ))
        .send()
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response.headers().get("content-type").unwrap(),
        "image/jpeg"
    );
    let body = response.bytes().await.unwrap();
    assert_eq!(body.as_ref(), b"STUBJPEG");
}

#[cfg(unix)]
#[tokio::test]
async fn library_scan_records_thumbnail_error_when_ffmpeg_fails() {
    let temp_dir = TempDir::new().unwrap();
    let root = temp_dir.path().join("library");
    fs::create_dir_all(&root).unwrap();
    fs::write(root.join("movie.mp4"), b"movie").unwrap();

    let probe_script = write_probe_script(&temp_dir, "probe-ok.sh", SUCCESSFUL_PROBE_JSON);
    let ffmpeg_stub = write_ffmpeg_stub(
        &temp_dir,
        "ffmpeg-fail.sh",
        r#"#!/usr/bin/env bash
set -euo pipefail
echo "synthetic ffmpeg failure" >&2
exit 1
"#,
    );
    let cache_dir = temp_dir.path().join("thumbnails");

    let config = LibraryConfig::from_paths(vec![root])
        .with_probe_command(probe_script)
        .with_ffmpeg_command(ffmpeg_stub)
        .with_thumbnail_cache_dir(&cache_dir);
    let server = TestServer::spawn_with_library_config(config).await;

    let _ = server.scan_library().await;
    let item = server
        .poll_first_item_until(Duration::from_secs(5), |item| {
            item.thumbnail_error.is_some()
        })
        .await;

    assert_eq!(item.thumbnail_generated_at, None);
    assert!(
        item.thumbnail_error
            .as_deref()
            .unwrap()
            .contains("synthetic ffmpeg failure"),
        "unexpected thumbnail error: {:?}",
        item.thumbnail_error
    );

    let response = server
        .client
        .get(format!(
            "{}/api/media/{}/thumbnail",
            server.base_url, item.id
        ))
        .send()
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::NOT_FOUND);
}

#[cfg(unix)]
#[tokio::test]
async fn library_scan_skips_thumbnail_for_audio_media() {
    let temp_dir = TempDir::new().unwrap();
    let root = temp_dir.path().join("library");
    fs::create_dir_all(&root).unwrap();
    fs::write(root.join("song.mp3"), b"audio").unwrap();

    let probe_script = write_probe_script(
        &temp_dir,
        "probe-audio.sh",
        r#"#!/usr/bin/env bash
set -euo pipefail
cat <<'JSON'
{
  "format": { "format_name": "mp3", "duration": "240.000" },
  "streams": [
    { "codec_type": "audio", "codec_name": "mp3" }
  ]
}
JSON
"#,
    );
    let ffmpeg_stub = write_ffmpeg_stub(
        &temp_dir,
        "ffmpeg-should-not-run.sh",
        r#"#!/usr/bin/env bash
echo "ffmpeg should not run for audio-only media" >&2
exit 99
"#,
    );
    let cache_dir = temp_dir.path().join("thumbnails");

    let config = LibraryConfig::from_paths(vec![root])
        .with_probe_command(probe_script)
        .with_ffmpeg_command(ffmpeg_stub)
        .with_thumbnail_cache_dir(&cache_dir);
    let server = TestServer::spawn_with_library_config(config).await;

    let scan = server.scan_library().await;
    let item = &scan.items[0];

    assert_eq!(item.thumbnail_generated_at, None);
    assert_eq!(item.thumbnail_error, None);
    assert!(!cache_dir.exists() || cache_dir.read_dir().unwrap().next().is_none());
}

#[cfg(unix)]
#[tokio::test]
async fn thumbnail_endpoint_reuses_stable_media_id_across_rescan() {
    let temp_dir = TempDir::new().unwrap();
    let root = temp_dir.path().join("library");
    fs::create_dir_all(&root).unwrap();
    fs::write(root.join("movie.mp4"), b"movie").unwrap();

    let probe_script = write_probe_script(&temp_dir, "probe-ok.sh", SUCCESSFUL_PROBE_JSON);
    let ffmpeg_stub = write_ffmpeg_stub(
        &temp_dir,
        "ffmpeg-ok.sh",
        r#"#!/usr/bin/env bash
set -euo pipefail
output="${!#}"
printf 'STUBJPEG' > "$output"
"#,
    );
    let cache_dir = temp_dir.path().join("thumbnails");

    let config = LibraryConfig::from_paths(vec![root.clone()])
        .with_probe_command(probe_script.clone())
        .with_ffmpeg_command(ffmpeg_stub.clone())
        .with_thumbnail_cache_dir(&cache_dir);
    let server = TestServer::spawn_with_library_config(config).await;

    server.scan_library().await;
    let first = server
        .wait_for_library_complete()
        .await
        .items
        .into_iter()
        .next()
        .unwrap();

    // Touch the source so the second scan must regenerate the thumbnail.
    fs::write(root.join("movie.mp4"), b"movie-updated").unwrap();

    server.scan_library().await;
    let second = server
        .wait_for_library_complete()
        .await
        .items
        .into_iter()
        .next()
        .unwrap();

    assert_eq!(
        first.id, second.id,
        "media id must remain stable across rescans"
    );
    assert!(second.thumbnail_generated_at.is_some());
}

#[cfg(unix)]
const NEEDS_PREPARATION_PROBE_JSON: &str = r#"#!/usr/bin/env bash
set -euo pipefail
cat <<'JSON'
{
  "format": { "format_name": "matroska,webm", "duration": "120.000" },
  "streams": [
    {
      "index": 0,
      "codec_type": "video",
      "codec_name": "h264",
      "profile": "High",
      "level": 40,
      "pix_fmt": "yuv420p",
      "bits_per_raw_sample": "8",
      "width": 1920,
      "height": 1080
    },
    {
      "index": 1,
      "codec_type": "audio",
      "codec_name": "ac3",
      "channels": 2,
      "channel_layout": "stereo",
      "disposition": { "default": 1 }
    }
  ]
}
JSON
"#;

#[cfg(unix)]
const EMBEDDED_PGS_PROBE_JSON: &str = r#"#!/usr/bin/env bash
set -euo pipefail
cat <<'JSON'
{
  "format": { "format_name": "matroska,webm", "duration": "120.000" },
  "streams": [
    {
      "index": 0,
      "codec_type": "video",
      "codec_name": "h264",
      "profile": "High",
      "level": 40,
      "pix_fmt": "yuv420p",
      "bits_per_raw_sample": "8",
      "width": 1920,
      "height": 1080
    },
    {
      "index": 1,
      "codec_type": "audio",
      "codec_name": "ac3",
      "channels": 2,
      "channel_layout": "stereo",
      "disposition": { "default": 1 }
    },
    {
      "index": 2,
      "codec_type": "subtitle",
      "codec_name": "hdmv_pgs_subtitle"
    }
  ]
}
JSON
"#;

#[cfg(unix)]
const EMBEDDED_TEXT_PROBE_JSON: &str = r#"#!/usr/bin/env bash
set -euo pipefail
cat <<'JSON'
{
  "format": { "format_name": "matroska,webm", "duration": "120.000" },
  "streams": [
    {
      "index": 0,
      "codec_type": "video",
      "codec_name": "h264",
      "profile": "High",
      "level": 40,
      "pix_fmt": "yuv420p",
      "bits_per_raw_sample": "8",
      "width": 1920,
      "height": 1080
    },
    {
      "index": 1,
      "codec_type": "audio",
      "codec_name": "ac3",
      "channels": 2,
      "channel_layout": "stereo",
      "disposition": { "default": 1 }
    },
    {
      "index": 2,
      "codec_type": "subtitle",
      "codec_name": "subrip",
      "tags": { "language": "eng", "title": "English" }
    }
  ]
}
JSON
"#;

#[cfg(unix)]
const FFMPEG_STREAM_COPY_OK_STUB: &str = r#"#!/usr/bin/env bash
set -euo pipefail
output="${!#}"
printf 'STREAMCOPY' > "$output"
"#;

#[cfg(unix)]
const FFMPEG_STREAM_COPY_SLOW_STUB: &str = r#"#!/usr/bin/env bash
set -euo pipefail
sleep 2
output="${!#}"
printf 'STREAMCOPY' > "$output"
"#;

#[cfg(unix)]
const FFMPEG_STREAM_COPY_PROGRESS_STUB: &str = r#"#!/usr/bin/env bash
set -euo pipefail
cat <<'EOF'
out_time_ms=60000000
speed=1.50x
progress=continue
EOF
sleep 2
cat <<'EOF'
out_time_ms=120000000
speed=1.00x
progress=end
EOF
output="${!#}"
printf 'STREAMCOPY' > "$output"
"#;

#[cfg(unix)]
const FFMPEG_WEBVTT_OK_STUB: &str = r#"#!/usr/bin/env bash
set -euo pipefail
output="${!#}"
body=$'WEBVTT\n\n00:00:01.000 --> 00:00:03.000\nHello from embedded subtitle\n'
if [ "$output" = "-" ]; then
  printf '%s' "$body"
else
  printf '%s' "$body" > "$output"
fi
"#;

#[cfg(unix)]
fn stream_copy_config(
    root: PathBuf,
    probe_script: PathBuf,
    ffmpeg_stub: Option<PathBuf>,
    cache_dir: PathBuf,
) -> LibraryConfig {
    let config = LibraryConfig::from_paths(vec![root])
        .with_probe_command(probe_script)
        .with_stream_copy_cache_dir(cache_dir);

    match ffmpeg_stub {
        Some(ffmpeg_stub) => config.with_ffmpeg_command(ffmpeg_stub),
        None => config,
    }
}

#[cfg(unix)]
#[tokio::test]
async fn stream_endpoint_returns_409_when_stream_copy_is_missing() {
    let temp_dir = TempDir::new().unwrap();
    let root = temp_dir.path().join("library");
    fs::create_dir_all(&root).unwrap();
    fs::write(root.join("movie.mkv"), b"synthetic").unwrap();

    let probe_script = write_probe_script(
        &temp_dir,
        "probe-needs-preparation.sh",
        NEEDS_PREPARATION_PROBE_JSON,
    );
    let cache_dir = temp_dir.path().join("stream-copies");
    let server = TestServer::spawn_with_library_config(stream_copy_config(
        root,
        probe_script,
        None,
        cache_dir,
    ))
    .await;

    server.scan_library().await;
    let item = server
        .wait_for_probes_complete()
        .await
        .items
        .into_iter()
        .next()
        .unwrap();
    assert_eq!(
        item.playback_mode,
        syncplay_backend::protocol::PlaybackMode::NeedsPreparation
    );
    assert_eq!(
        item.preparation_state,
        syncplay_backend::protocol::PreparationState::NeedsPreparation
    );

    let response = server
        .client
        .get(format!("{}/api/media/{}/stream", server.base_url, item.id))
        .send()
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::CONFLICT);
    assert!(response.text().await.unwrap().contains("stream copy"));
}

#[cfg(unix)]
#[tokio::test]
async fn create_stream_copy_returns_409_for_direct_playable_media() {
    let temp_dir = TempDir::new().unwrap();
    let root = temp_dir.path().join("library");
    fs::create_dir_all(&root).unwrap();
    fs::write(root.join("movie.mp4"), b"synthetic").unwrap();

    let probe_script = write_probe_script(&temp_dir, "probe-direct.sh", SUCCESSFUL_PROBE_JSON);
    let ffmpeg_stub = write_ffmpeg_stub(&temp_dir, "ffmpeg-copy.sh", FFMPEG_STREAM_COPY_OK_STUB);
    let cache_dir = temp_dir.path().join("stream-copies");
    let server = TestServer::spawn_with_library_config(stream_copy_config(
        root,
        probe_script,
        Some(ffmpeg_stub),
        cache_dir,
    ))
    .await;

    server.scan_library().await;
    let item = server
        .wait_for_probes_complete()
        .await
        .items
        .into_iter()
        .next()
        .unwrap();

    let response = server
        .create_stream_copy(
            &item.id.to_string(),
            &CreateStreamCopyRequest {
                audio_stream_index: item.audio_streams.first().map(|stream| stream.index),
                subtitle_mode: syncplay_backend::protocol::SubtitleMode::Off,
                subtitle: None,
            },
        )
        .await;

    assert_eq!(response.status(), StatusCode::CONFLICT);
}

#[cfg(unix)]
#[tokio::test]
async fn create_stream_copy_exposes_preparing_state_while_job_runs() {
    let temp_dir = TempDir::new().unwrap();
    let root = temp_dir.path().join("library");
    fs::create_dir_all(&root).unwrap();
    fs::write(root.join("movie.mkv"), b"synthetic").unwrap();

    let probe_script = write_probe_script(
        &temp_dir,
        "probe-needs-preparation.sh",
        NEEDS_PREPARATION_PROBE_JSON,
    );
    let ffmpeg_stub = write_ffmpeg_stub(&temp_dir, "ffmpeg-slow.sh", FFMPEG_STREAM_COPY_SLOW_STUB);
    let cache_dir = temp_dir.path().join("stream-copies");
    let server = TestServer::spawn_with_library_config(stream_copy_config(
        root,
        probe_script,
        Some(ffmpeg_stub),
        cache_dir,
    ))
    .await;

    server.scan_library().await;
    let item = server
        .wait_for_probes_complete()
        .await
        .items
        .into_iter()
        .next()
        .unwrap();

    let response = server
        .create_stream_copy(
            &item.id.to_string(),
            &CreateStreamCopyRequest {
                audio_stream_index: item.audio_streams.first().map(|stream| stream.index),
                subtitle_mode: syncplay_backend::protocol::SubtitleMode::Off,
                subtitle: None,
            },
        )
        .await;
    assert_eq!(response.status(), StatusCode::OK);

    let preparing = server
        .wait_for_preparation_state(&item.id.to_string(), "preparing")
        .await;
    assert_eq!(
        preparing.preparation_state,
        syncplay_backend::protocol::PreparationState::Preparing
    );
}

#[cfg(unix)]
#[tokio::test]
async fn get_stream_copy_exposes_live_progress_while_job_runs() {
    let temp_dir = TempDir::new().unwrap();
    let root = temp_dir.path().join("library");
    fs::create_dir_all(&root).unwrap();
    fs::write(root.join("movie.mkv"), b"synthetic").unwrap();

    let probe_script = write_probe_script(
        &temp_dir,
        "probe-needs-preparation.sh",
        NEEDS_PREPARATION_PROBE_JSON,
    );
    let ffmpeg_stub = write_ffmpeg_stub(
        &temp_dir,
        "ffmpeg-progress.sh",
        FFMPEG_STREAM_COPY_PROGRESS_STUB,
    );
    let cache_dir = temp_dir.path().join("stream-copies");
    let server = TestServer::spawn_with_library_config(stream_copy_config(
        root,
        probe_script,
        Some(ffmpeg_stub),
        cache_dir,
    ))
    .await;

    server.scan_library().await;
    let item = server
        .wait_for_probes_complete()
        .await
        .items
        .into_iter()
        .next()
        .unwrap();

    let response = server
        .create_stream_copy(
            &item.id.to_string(),
            &CreateStreamCopyRequest {
                audio_stream_index: item.audio_streams.first().map(|stream| stream.index),
                subtitle_mode: syncplay_backend::protocol::SubtitleMode::Off,
                subtitle: None,
            },
        )
        .await;
    assert_eq!(response.status(), StatusCode::OK);

    let summary = server
        .wait_for_stream_copy_summary(&item.id.to_string(), Duration::from_secs(5), |summary| {
            summary.status == StreamCopyStatus::Running && summary.progress_ratio.is_some()
        })
        .await;

    assert_eq!(summary.status, StreamCopyStatus::Running);
    let ratio = summary.progress_ratio.expect("running progress ratio");
    assert!(
        (ratio - 0.5).abs() < 0.01,
        "expected progress ratio close to 0.5, got {ratio}"
    );
    let speed = summary.progress_speed.expect("running progress speed");
    assert!(
        (speed - 1.5).abs() < 0.01,
        "expected progress speed close to 1.5, got {speed}"
    );
}

#[cfg(unix)]
#[tokio::test]
async fn prepared_stream_copy_serves_prepared_media_and_vtt_subtitle() {
    let temp_dir = TempDir::new().unwrap();
    let root = temp_dir.path().join("library");
    fs::create_dir_all(&root).unwrap();
    fs::write(root.join("movie.mkv"), b"synthetic").unwrap();
    fs::write(
        root.join("movie.en.srt"),
        "1\n00:00:01,000 --> 00:00:02,000\nHello from SRT\n",
    )
    .unwrap();

    let probe_script = write_probe_script(
        &temp_dir,
        "probe-needs-preparation.sh",
        NEEDS_PREPARATION_PROBE_JSON,
    );
    let ffmpeg_stub = write_ffmpeg_stub(&temp_dir, "ffmpeg-copy.sh", FFMPEG_STREAM_COPY_OK_STUB);
    let cache_dir = temp_dir.path().join("stream-copies");
    let server = TestServer::spawn_with_library_config(stream_copy_config(
        root,
        probe_script,
        Some(ffmpeg_stub),
        cache_dir,
    ))
    .await;

    server.scan_library().await;
    let item = server
        .wait_for_probes_complete()
        .await
        .items
        .into_iter()
        .next()
        .unwrap();

    let response = server
        .create_stream_copy(
            &item.id.to_string(),
            &CreateStreamCopyRequest {
                audio_stream_index: item.audio_streams.first().map(|stream| stream.index),
                subtitle_mode: syncplay_backend::protocol::SubtitleMode::Sidecar,
                subtitle: Some(syncplay_backend::protocol::StreamCopySubtitleSelection {
                    kind: syncplay_backend::protocol::SubtitleSourceKind::Sidecar,
                    index: 0,
                }),
            },
        )
        .await;
    assert_eq!(response.status(), StatusCode::OK);

    let prepared = server
        .wait_for_preparation_state(&item.id.to_string(), "prepared")
        .await;
    assert_eq!(
        prepared.preparation_state,
        syncplay_backend::protocol::PreparationState::Prepared
    );

    let media_response = server
        .client
        .get(format!("{}/api/media/{}/stream", server.base_url, item.id))
        .send()
        .await
        .unwrap();
    assert_eq!(media_response.status(), StatusCode::OK);
    assert_eq!(
        media_response.bytes().await.unwrap().as_ref(),
        b"STREAMCOPY"
    );

    let subtitle_response = server
        .client
        .get(format!(
            "{}/api/media/{}/stream-copy/subtitle",
            server.base_url, item.id
        ))
        .send()
        .await
        .unwrap();
    assert_eq!(subtitle_response.status(), StatusCode::OK);
    let subtitle_body = subtitle_response.text().await.unwrap();
    assert!(subtitle_body.starts_with("WEBVTT"));
    assert!(subtitle_body.contains("00:00:01.000 --> 00:00:02.000"));
    assert!(subtitle_body.contains("Hello from SRT"));
}

#[cfg(unix)]
#[tokio::test]
async fn create_stream_copy_rejects_non_text_embedded_subtitle_for_sidecar_mode() {
    let temp_dir = TempDir::new().unwrap();
    let root = temp_dir.path().join("library");
    fs::create_dir_all(&root).unwrap();
    fs::write(root.join("movie.mkv"), b"synthetic").unwrap();

    let probe_script = write_probe_script(&temp_dir, "probe-pgs.sh", EMBEDDED_PGS_PROBE_JSON);
    let ffmpeg_stub = write_ffmpeg_stub(&temp_dir, "ffmpeg-copy.sh", FFMPEG_STREAM_COPY_OK_STUB);
    let cache_dir = temp_dir.path().join("stream-copies");
    let server = TestServer::spawn_with_library_config(stream_copy_config(
        root,
        probe_script,
        Some(ffmpeg_stub),
        cache_dir,
    ))
    .await;

    server.scan_library().await;
    let item = server
        .wait_for_probes_complete()
        .await
        .items
        .into_iter()
        .next()
        .unwrap();

    let response = server
        .create_stream_copy(
            &item.id.to_string(),
            &CreateStreamCopyRequest {
                audio_stream_index: item.audio_streams.first().map(|stream| stream.index),
                subtitle_mode: syncplay_backend::protocol::SubtitleMode::Sidecar,
                subtitle: Some(syncplay_backend::protocol::StreamCopySubtitleSelection {
                    kind: syncplay_backend::protocol::SubtitleSourceKind::Embedded,
                    index: 2,
                }),
            },
        )
        .await;

    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    assert!(
        response
            .text()
            .await
            .unwrap()
            .contains("cannot be converted")
    );
}

#[tokio::test]
async fn select_media_swaps_room_media_and_resets_clock() {
    let temp_dir = TempDir::new().unwrap();
    let root = temp_dir.path().join("library");
    fs::create_dir_all(&root).unwrap();
    fs::write(root.join("first.mp4"), b"first").unwrap();
    fs::write(root.join("second.mp4"), b"second").unwrap();
    let server = TestServer::spawn_with_library_roots(vec![root]).await;

    let scan = server.scan_library().await;
    let first = scan
        .items
        .iter()
        .find(|item| item.relative_path == "first.mp4")
        .unwrap()
        .clone();
    let second = scan
        .items
        .iter()
        .find(|item| item.relative_path == "second.mp4")
        .unwrap()
        .clone();

    let created: Room = server
        .client
        .post(format!("{}/api/rooms", server.base_url))
        .json(&serde_json::json!({
            "name": "Swap Room",
            "mediaId": first.id.to_string(),
        }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();

    let mut socket = connect_room_socket(&server, &created.id.to_string(), "Operator").await;

    // Drain snapshot + presence.
    let _ = next_event(&mut socket).await;
    let _ = next_event(&mut socket).await;

    // Advance the clock so we can verify the reset.
    socket
        .send(Message::Text(
            serde_json::json!({ "type": "play", "positionSeconds": 12.5 })
                .to_string()
                .into(),
        ))
        .await
        .unwrap();
    let _ = next_event(&mut socket).await;

    socket
        .send(Message::Text(
            serde_json::json!({
                "type": "selectMedia",
                "mediaId": second.id.to_string(),
            })
            .to_string()
            .into(),
        ))
        .await
        .unwrap();

    match next_event(&mut socket).await {
        ServerEvent::MediaChanged { room, actor } => {
            assert_eq!(actor, "Operator");
            assert_eq!(room.media_id, Some(second.id));
            assert_eq!(room.media_title.as_deref(), Some(second.file_name.as_str()));
            assert_eq!(room.playback_state.status, PlaybackStatus::Paused);
            assert_eq!(room.playback_state.position_seconds, 0.0);
            assert_eq!(room.playback_state.anchor_position_seconds, 0.0);
        }
        other => panic!("expected media changed event, got {other:?}"),
    }

    // The REST snapshot should reflect the new media after persistence.
    let refreshed = server
        .rooms()
        .await
        .rooms
        .into_iter()
        .find(|entry| entry.id == created.id)
        .unwrap();
    assert_eq!(refreshed.media_id, Some(second.id));
    assert_eq!(refreshed.playback_state.position_seconds, 0.0);
}

#[tokio::test]
async fn select_media_broadcasts_change_to_other_clients() {
    let temp_dir = TempDir::new().unwrap();
    let root = temp_dir.path().join("library");
    fs::create_dir_all(&root).unwrap();
    fs::write(root.join("a.mp4"), b"a").unwrap();
    fs::write(root.join("b.mp4"), b"b").unwrap();
    let server = TestServer::spawn_with_library_roots(vec![root]).await;

    let scan = server.scan_library().await;
    let a = scan.items.iter().find(|i| i.relative_path == "a.mp4").unwrap().clone();
    let b = scan.items.iter().find(|i| i.relative_path == "b.mp4").unwrap().clone();

    let created: Room = server
        .client
        .post(format!("{}/api/rooms", server.base_url))
        .json(&serde_json::json!({
            "name": "Shared Room",
            "mediaId": a.id.to_string(),
        }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();

    let mut alpha = connect_room_socket(&server, &created.id.to_string(), "Alpha").await;
    let _ = next_event(&mut alpha).await; // snapshot
    let _ = next_event(&mut alpha).await; // presence
    let mut bravo = connect_room_socket(&server, &created.id.to_string(), "Bravo").await;
    let _ = next_event(&mut bravo).await; // snapshot for bravo
    let _ = next_event(&mut alpha).await; // presence for bravo join on alpha
    let _ = next_event(&mut bravo).await; // presence for self

    alpha
        .send(Message::Text(
            serde_json::json!({
                "type": "selectMedia",
                "mediaId": b.id.to_string(),
            })
            .to_string()
            .into(),
        ))
        .await
        .unwrap();

    for socket in [&mut alpha, &mut bravo] {
        match next_event(socket).await {
            ServerEvent::MediaChanged { room, .. } => {
                assert_eq!(room.media_id, Some(b.id));
            }
            other => panic!("expected media changed, got {other:?}"),
        }
    }
}

#[tokio::test]
async fn select_media_rejects_unknown_media_id() {
    let temp_dir = TempDir::new().unwrap();
    let root = temp_dir.path().join("library");
    fs::create_dir_all(&root).unwrap();
    fs::write(root.join("known.mp4"), b"known").unwrap();
    let server = TestServer::spawn_with_library_roots(vec![root]).await;

    let scan = server.scan_library().await;
    let known = scan.items.into_iter().next().unwrap();

    let created: Room = server
        .client
        .post(format!("{}/api/rooms", server.base_url))
        .json(&serde_json::json!({
            "name": "Rejecting Room",
            "mediaId": known.id.to_string(),
        }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();

    let mut socket = connect_room_socket(&server, &created.id.to_string(), "Operator").await;
    let _ = next_event(&mut socket).await; // snapshot
    let _ = next_event(&mut socket).await; // presence

    let phantom = uuid::Uuid::new_v4();
    socket
        .send(Message::Text(
            serde_json::json!({
                "type": "selectMedia",
                "mediaId": phantom.to_string(),
            })
            .to_string()
            .into(),
        ))
        .await
        .unwrap();

    match next_event(&mut socket).await {
        ServerEvent::Error { message } => {
            assert!(message.to_lowercase().contains("library"), "{message}");
        }
        other => panic!("expected error event, got {other:?}"),
    }

    // Room media stays pointed at the original item after the rejection.
    let refreshed = server
        .rooms()
        .await
        .rooms
        .into_iter()
        .find(|room| room.id == created.id)
        .unwrap();
    assert_eq!(refreshed.media_id, Some(known.id));
}

#[tokio::test]
async fn empty_room_is_deleted_after_grace_period() {
    let server = TestServer::spawn_with_grace(Duration::from_millis(200)).await;

    let created: Room = server
        .client
        .post(format!("{}/api/rooms", server.base_url))
        .json(&serde_json::json!({ "name": "Ephemeral" }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();

    assert!(
        server
            .rooms()
            .await
            .rooms
            .iter()
            .any(|room| room.id == created.id)
    );

    tokio::time::sleep(Duration::from_millis(450)).await;

    assert!(
        server
            .rooms()
            .await
            .rooms
            .iter()
            .all(|room| room.id != created.id),
        "empty room should have been cleaned up after the grace period"
    );
}

#[tokio::test]
async fn client_join_cancels_pending_cleanup() {
    let server = TestServer::spawn_with_grace(Duration::from_millis(200)).await;

    let created: Room = server
        .client
        .post(format!("{}/api/rooms", server.base_url))
        .json(&serde_json::json!({ "name": "Staying Alive" }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();

    // Connect before the grace elapses so the pending cleanup is aborted.
    let mut socket = connect_room_socket(&server, &created.id.to_string(), "Operator").await;
    let _ = next_event(&mut socket).await; // snapshot
    let _ = next_event(&mut socket).await; // presence

    tokio::time::sleep(Duration::from_millis(450)).await;

    assert!(
        server
            .rooms()
            .await
            .rooms
            .iter()
            .any(|room| room.id == created.id),
        "cleanup should have been cancelled by the joining client"
    );
}

#[tokio::test]
async fn last_client_leaving_triggers_cleanup_after_grace_period() {
    let server = TestServer::spawn_with_grace(Duration::from_millis(200)).await;

    let created: Room = server
        .client
        .post(format!("{}/api/rooms", server.base_url))
        .json(&serde_json::json!({ "name": "Short Lived" }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();

    let mut socket = connect_room_socket(&server, &created.id.to_string(), "Operator").await;
    let _ = next_event(&mut socket).await; // snapshot
    let _ = next_event(&mut socket).await; // presence

    socket.send(Message::Close(None)).await.unwrap();
    drop(socket);

    tokio::time::sleep(Duration::from_millis(450)).await;

    assert!(
        server
            .rooms()
            .await
            .rooms
            .iter()
            .all(|room| room.id != created.id),
        "room should be cleaned up after last client leaves"
    );
}

#[tokio::test]
async fn persisted_empty_room_is_cleaned_up_after_restart() {
    let temp_dir = TempDir::new().unwrap();
    let database_path = temp_dir.path().join("syncplay.db");

    // First boot: create a room and then let the process exit. Use the
    // default long grace here so the room survives to disk.
    let created: Room = {
        let first = TestServer::spawn_persistent(&database_path).await;
        let room = first
            .client
            .post(format!("{}/api/rooms", first.base_url))
            .json(&serde_json::json!({ "name": "Leftover" }))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        drop(first);
        room
    };

    // Second boot: short grace, so the persisted-but-empty room falls out
    // of the index on its own without anyone connecting.
    let persistence = Persistence::open_at(&database_path).await.unwrap();
    let app = build_app(
        load_state_with_library_and_grace(
            persistence,
            LibraryConfig::default(),
            Duration::from_millis(200),
        )
        .await
        .unwrap(),
    );
    let second = TestServer::spawn_with_app(app).await;

    assert!(
        second
            .rooms()
            .await
            .rooms
            .iter()
            .any(|room| room.id == created.id)
    );

    tokio::time::sleep(Duration::from_millis(450)).await;

    assert!(
        second
            .rooms()
            .await
            .rooms
            .iter()
            .all(|room| room.id != created.id)
    );
}
