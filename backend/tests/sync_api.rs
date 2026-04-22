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
use reqwest::header::{CONTENT_RANGE, RANGE};
use syncplay_backend::{
    build_app,
    library::LibraryConfig,
    load_state, load_state_with_library, load_state_with_library_and_hls,
    muxing::{HlsConfig, HlsSessionManager},
    persistence::Persistence,
    protocol::{
        DriftCorrectionAction, LibraryResponse, LibraryScanResponse, Room, RoomsResponse,
        ServerEvent,
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
    hls: Option<HlsSessionManager>,
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

    async fn spawn_persistent_with_library_config(
        database_path: &Path,
        config: LibraryConfig,
    ) -> Self {
        let persistence = Persistence::open_at(database_path).await.unwrap();
        let app = build_app(load_state_with_library(persistence, config).await.unwrap());

        Self::spawn_with_app(app).await
    }

    async fn spawn_with_library_and_hls(config: LibraryConfig, hls_config: HlsConfig) -> Self {
        let persistence = Persistence::open_in_memory().await.unwrap();
        let state = load_state_with_library_and_hls(persistence, config, hls_config)
            .await
            .unwrap();
        let hls = state.hls();
        let app = build_app(state);

        let mut server = Self::spawn_with_app(app).await;
        server.hls = Some(hls);
        server
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
            hls: None,
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
        self.rooms().await.rooms.into_iter().next().unwrap()
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
            if let Some(item) = library.items.into_iter().next() {
                if predicate(&item) {
                    return item;
                }
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
                    item.thumbnail_generated_at.is_none() && item.thumbnail_error.is_none()
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
    let result = timeout(Duration::from_millis(250), socket.next()).await;
    assert!(
        result.is_err(),
        "expected no websocket event, but one arrived"
    );
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

    assert_eq!(
        names,
        vec!["Alpha Night", "Friday Watch Party", "Zulu Night"]
    );
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
    let missing_root = missing_root;
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

    let created_rooms = join_all(room_names.iter().cloned().map(|room_name| {
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

    assert_eq!(listed_rooms.len(), room_names.len() + 1);

    for room_name in room_names {
        assert!(listed_rooms.iter().any(|room| room.name == room_name));
    }
}

#[tokio::test]
async fn persistent_startup_seeds_preview_room_only_once() {
    let temp_dir = TempDir::new().unwrap();
    let database_path = temp_dir.path().join("syncplay.db");

    let first_server = TestServer::spawn_persistent(&database_path).await;
    let first_rooms = first_server.rooms().await.rooms;

    assert_eq!(first_rooms.len(), 1);
    assert_eq!(first_rooms[0].name, "Friday Watch Party");

    drop(first_server);

    let second_server = TestServer::spawn_persistent(&database_path).await;
    let second_rooms = second_server.rooms().await.rooms;

    assert_eq!(second_rooms.len(), 1);
    assert_eq!(second_rooms[0].name, "Friday Watch Party");
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

// ---------------------------------------------------------------------------
// HLS integration tests use real ffmpeg/ffprobe end-to-end. The shim approach
// missed several real-ffmpeg integration bugs (path doubling under
// -hls_fmp4_init_filename, %v non-substitution in single-variant runs,
// relative cache_dir resolution against ffmpeg's CWD) so we run the actual
// pipeline. Tests are skipped (with a printed reason) on hosts without the
// binaries on PATH.
// ---------------------------------------------------------------------------

/// Returns true iff both `ffmpeg` and `ffprobe` resolve on PATH.
fn external_av_tools_available() -> bool {
    fn probe(bin: &str) -> bool {
        std::process::Command::new(bin)
            .arg("-version")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .map(|status| status.success())
            .unwrap_or(false)
    }
    probe("ffmpeg") && probe("ffprobe")
}

/// Generate a tiny H.264 + AC3 MKV using real ffmpeg. The classifier sees this
/// as `HlsAudioTranscode` (browser-safe video, browser-hostile audio) which is
/// the most common real-world tier.
fn generate_h264_ac3_mkv(path: &Path) {
    let output = std::process::Command::new("ffmpeg")
        .args([
            "-y",
            "-loglevel",
            "error",
            "-f",
            "lavfi",
            "-i",
            "testsrc=duration=2:size=320x240:rate=30",
            "-f",
            "lavfi",
            "-i",
            "sine=frequency=440:duration=2",
            "-c:v",
            "libx264",
            "-preset",
            "ultrafast",
            "-pix_fmt",
            "yuv420p",
            "-c:a",
            "ac3",
            "-b:a",
            "128k",
            "-shortest",
        ])
        .arg(path)
        .output()
        .expect("ffmpeg invocation failed to start");
    assert!(
        output.status.success(),
        "failed to generate ac3 mkv: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

/// Generate a tiny H.264 + AAC MP4 using real ffmpeg — classifier sees
/// `Direct` (no muxing required).
fn generate_h264_aac_mp4(path: &Path) {
    let output = std::process::Command::new("ffmpeg")
        .args([
            "-y",
            "-loglevel",
            "error",
            "-f",
            "lavfi",
            "-i",
            "testsrc=duration=2:size=320x240:rate=30",
            "-f",
            "lavfi",
            "-i",
            "sine=frequency=440:duration=2",
            "-c:v",
            "libx264",
            "-preset",
            "ultrafast",
            "-pix_fmt",
            "yuv420p",
            "-c:a",
            "aac",
            "-b:a",
            "96k",
            "-shortest",
        ])
        .arg(path)
        .output()
        .expect("ffmpeg invocation failed to start");
    assert!(
        output.status.success(),
        "failed to generate aac mp4: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[cfg(unix)]
async fn hls_test_setup(temp_dir: &TempDir) -> (TestServer, syncplay_backend::protocol::MediaItem) {
    let root = temp_dir.path().join("library");
    fs::create_dir_all(&root).unwrap();
    generate_h264_ac3_mkv(&root.join("movie.mkv"));

    let cache_dir = temp_dir.path().join("hls-cache");

    // Use real ffprobe (no probe-command override) and real ffmpeg.
    let library = LibraryConfig::from_paths(vec![root]);
    let hls_config = HlsConfig {
        cache_dir,
        max_concurrent: 4,
        idle_secs: 60,
        ffmpeg_command: Some(PathBuf::from("ffmpeg")),
        hw_accel: Default::default(),
        vaapi_device: None,
    };

    let server = TestServer::spawn_with_library_and_hls(library, hls_config).await;
    server.scan_library().await;
    // Probe runs in the background after the walk; HLS tests need
    // playback_mode populated, which only lands after probe completes.
    let library = server.wait_for_probes_complete().await;
    let item = library
        .items
        .into_iter()
        .next()
        .expect("scanned media item");
    (server, item)
}

#[cfg(unix)]
#[tokio::test]
async fn hls_master_returns_415_when_media_is_unsupported() {
    // This test exercises the endpoint contract "classifier emits Unsupported
    // → endpoint returns 415". Triggering Unsupported deterministically requires
    // probe metadata that has zero streams (a different code path from "ffprobe
    // couldn't read the file at all", which falls back to Direct). A probe
    // stub is the cleanest way to drive that exact state — orthogonal to
    // whether real ffmpeg is involved downstream.
    let temp_dir = TempDir::new().unwrap();
    let root = temp_dir.path().join("library");
    fs::create_dir_all(&root).unwrap();
    fs::write(root.join("broken.mp4"), b"x").unwrap();
    let probe = write_probe_script(
        &temp_dir,
        "probe-empty.sh",
        r#"#!/usr/bin/env bash
set -euo pipefail
cat <<'JSON'
{ "format": { "format_name": "matroska,webm", "duration": "1.0" }, "streams": [] }
JSON
"#,
    );

    let library = LibraryConfig::from_paths(vec![root]).with_probe_command(probe);
    let hls_config = HlsConfig {
        cache_dir: temp_dir.path().join("hls-cache"),
        max_concurrent: 1,
        idle_secs: 60,
        ffmpeg_command: Some(PathBuf::from("ffmpeg")),
        hw_accel: Default::default(),
        vaapi_device: None,
    };
    let server = TestServer::spawn_with_library_and_hls(library, hls_config).await;
    server.scan_library().await;
    let library = server.wait_for_probes_complete().await;
    let item = library.items.into_iter().next().unwrap();
    assert_eq!(
        item.playback_mode,
        syncplay_backend::protocol::PlaybackMode::Unsupported
    );

    let response = server
        .client
        .get(format!(
            "{}/api/media/{}/hls/master.m3u8",
            server.base_url, item.id
        ))
        .send()
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::UNSUPPORTED_MEDIA_TYPE);
}

#[cfg(unix)]
#[tokio::test]
async fn hls_master_returns_409_for_direct_playback() {
    if !external_av_tools_available() {
        eprintln!("skipping: ffmpeg/ffprobe not on PATH");
        return;
    }

    let temp_dir = TempDir::new().unwrap();
    let root = temp_dir.path().join("library");
    fs::create_dir_all(&root).unwrap();
    generate_h264_aac_mp4(&root.join("movie.mp4"));

    let library = LibraryConfig::from_paths(vec![root]);
    let hls_config = HlsConfig {
        cache_dir: temp_dir.path().join("hls-cache"),
        max_concurrent: 1,
        idle_secs: 60,
        ffmpeg_command: Some(PathBuf::from("ffmpeg")),
        hw_accel: Default::default(),
        vaapi_device: None,
    };
    let server = TestServer::spawn_with_library_and_hls(library, hls_config).await;
    server.scan_library().await;
    let library = server.wait_for_probes_complete().await;
    let item = library.items.into_iter().next().unwrap();

    assert_eq!(
        item.playback_mode,
        syncplay_backend::protocol::PlaybackMode::Direct,
        "h264+aac mp4 should be classified as direct-playable"
    );

    let response = server
        .client
        .get(format!(
            "{}/api/media/{}/hls/master.m3u8",
            server.base_url, item.id
        ))
        .send()
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::CONFLICT);
}

#[cfg(unix)]
#[tokio::test]
async fn hls_master_returns_playable_manifest_for_eligible_media() {
    if !external_av_tools_available() {
        eprintln!("skipping: ffmpeg/ffprobe not on PATH");
        return;
    }

    let temp_dir = TempDir::new().unwrap();
    let (server, item) = hls_test_setup(&temp_dir).await;

    assert!(item.playback_mode.is_hls(), "expected an HLS playback mode");
    assert!(item.hls_master_url.is_some());

    let response = server
        .client
        .get(format!(
            "{}/api/media/{}/hls/master.m3u8",
            server.base_url, item.id
        ))
        .send()
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response
            .headers()
            .get("content-type")
            .and_then(|value| value.to_str().ok()),
        Some("application/vnd.apple.mpegurl")
    );
    let master_body = response.text().await.unwrap();
    assert!(master_body.contains("#EXTM3U"));
    // Master lists at least one variant playlist.
    assert!(
        master_body.lines().any(|line| line.ends_with(".m3u8")),
        "master playlist should reference at least one variant: {master_body}"
    );

    // Variant playlist must reference an init segment that actually exists on
    // disk and is reachable via the server. This is the assertion that catches
    // the fmp4 init bugs the shim missed.
    let variant_name = master_body
        .lines()
        .find(|line| line.ends_with(".m3u8"))
        .expect("variant playlist line");

    let variant_response = server
        .client
        .get(format!(
            "{}/api/media/{}/hls/{}",
            server.base_url, item.id, variant_name
        ))
        .send()
        .await
        .unwrap();
    assert_eq!(variant_response.status(), StatusCode::OK);
    let variant_body = variant_response.text().await.unwrap();

    let init_uri = variant_body
        .lines()
        .find_map(|line| line.strip_prefix("#EXT-X-MAP:URI=\""))
        .and_then(|rest| rest.split('"').next())
        .expect("variant playlist must declare an init segment via #EXT-X-MAP");

    let init_response = server
        .client
        .get(format!(
            "{}/api/media/{}/hls/{}",
            server.base_url, item.id, init_uri
        ))
        .send()
        .await
        .unwrap();
    assert_eq!(
        init_response.status(),
        StatusCode::OK,
        "init segment '{init_uri}' must be servable"
    );
    let init_bytes = init_response.bytes().await.unwrap();
    assert!(
        !init_bytes.is_empty(),
        "init segment should contain real fmp4 bytes"
    );
}

#[cfg(unix)]
#[tokio::test]
async fn hls_segment_is_real_fmp4_bytes() {
    if !external_av_tools_available() {
        eprintln!("skipping: ffmpeg/ffprobe not on PATH");
        return;
    }

    let temp_dir = TempDir::new().unwrap();
    let (server, item) = hls_test_setup(&temp_dir).await;

    // Master kicks off the session; wait for it to be ready.
    let _ = server
        .client
        .get(format!(
            "{}/api/media/{}/hls/master.m3u8",
            server.base_url, item.id
        ))
        .send()
        .await
        .unwrap();

    // Locate the first segment by reading the variant playlist.
    let master = server
        .client
        .get(format!(
            "{}/api/media/{}/hls/master.m3u8",
            server.base_url, item.id
        ))
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    let variant_name = master
        .lines()
        .find(|line| line.ends_with(".m3u8"))
        .expect("variant in master");
    let variant = server
        .client
        .get(format!(
            "{}/api/media/{}/hls/{}",
            server.base_url, item.id, variant_name
        ))
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    let segment_name = variant
        .lines()
        .find(|line| line.ends_with(".m4s"))
        .expect("variant must reference at least one .m4s segment");

    let response = server
        .client
        .get(format!(
            "{}/api/media/{}/hls/{}",
            server.base_url, item.id, segment_name
        ))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response
            .headers()
            .get("content-type")
            .and_then(|value| value.to_str().ok()),
        Some("video/iso.segment")
    );
    let body = response.bytes().await.unwrap();
    // fMP4 segment should start with a `styp` or `moof` box (size + 4-byte tag
    // at offset 4). Sanity-check we got real container bytes.
    assert!(body.len() > 16, "segment too small to be real fmp4");
    let box_tag = std::str::from_utf8(&body[4..8]).unwrap_or("");
    assert!(
        matches!(box_tag, "styp" | "moof" | "ftyp"),
        "segment first box should be styp/moof/ftyp, got '{box_tag}'"
    );
}

#[cfg(unix)]
#[tokio::test]
async fn hls_unknown_duration_serves_ffmpeg_variant_playlist() {
    if !external_av_tools_available() {
        eprintln!("skipping: ffmpeg/ffprobe not on PATH");
        return;
    }

    let temp_dir = TempDir::new().unwrap();
    let root = temp_dir.path().join("library");
    fs::create_dir_all(&root).unwrap();
    generate_h264_ac3_mkv(&root.join("movie.mkv"));

    // Duration intentionally omitted to exercise the unknown-duration path.
    let probe = write_probe_script(
        &temp_dir,
        "probe-unknown-duration.sh",
        r#"#!/usr/bin/env bash
set -euo pipefail
cat <<'JSON'
{
  "format": { "format_name": "matroska,webm" },
  "streams": [
    { "codec_type": "video", "codec_name": "h264", "width": 320, "height": 240 },
    { "codec_type": "audio", "codec_name": "ac3" }
  ]
}
JSON
"#,
    );

    let library = LibraryConfig::from_paths(vec![root]).with_probe_command(probe);
    let hls_config = HlsConfig {
        cache_dir: temp_dir.path().join("hls-cache"),
        max_concurrent: 4,
        idle_secs: 60,
        ffmpeg_command: Some(PathBuf::from("ffmpeg")),
        hw_accel: Default::default(),
        vaapi_device: None,
    };

    let server = TestServer::spawn_with_library_and_hls(library, hls_config).await;
    server.scan_library().await;
    let library = server.wait_for_probes_complete().await;
    let item = library
        .items
        .into_iter()
        .next()
        .expect("scanned media item");

    assert!(item.duration_seconds.is_none());
    assert!(item.playback_mode.is_hls());

    // Kick session startup first.
    let master = server
        .client
        .get(format!(
            "{}/api/media/{}/hls/master.m3u8",
            server.base_url, item.id
        ))
        .send()
        .await
        .unwrap();
    assert_eq!(master.status(), StatusCode::OK);

    let variant = server
        .client
        .get(format!(
            "{}/api/media/{}/hls/stream_0.m3u8",
            server.base_url, item.id
        ))
        .send()
        .await
        .unwrap();
    assert_eq!(variant.status(), StatusCode::OK);
    let body = variant.text().await.unwrap();

    // Unknown-duration sessions should follow ffmpeg's variant directly,
    // which references the raw init name, not the synthesized locked init.
    assert!(
        body.contains("#EXT-X-MAP:URI=\"init_0.mp4\""),
        "expected ffmpeg variant MAP for unknown-duration session: {body}"
    );
}

#[cfg(unix)]
#[tokio::test]
async fn hls_concurrent_master_requests_share_one_session() {
    if !external_av_tools_available() {
        eprintln!("skipping: ffmpeg/ffprobe not on PATH");
        return;
    }

    let temp_dir = TempDir::new().unwrap();
    let (server, item) = hls_test_setup(&temp_dir).await;

    let url = format!("{}/api/media/{}/hls/master.m3u8", server.base_url, item.id);
    let (a, b) = tokio::join!(
        server.client.get(&url).send(),
        server.client.get(&url).send()
    );
    assert_eq!(a.unwrap().status(), StatusCode::OK);
    assert_eq!(b.unwrap().status(), StatusCode::OK);

    // Each session corresponds to exactly one ffmpeg spawn (idempotent under
    // the per-session startup mutex). With both callers racing, the manager
    // must still hold only one session for this media id.
    let manager = server.hls.as_ref().expect("hls manager handle");
    assert_eq!(
        manager.session_count().await,
        1,
        "concurrent callers should share a single HLS session"
    );
}

#[cfg(unix)]
#[tokio::test]
async fn hls_full_transcode_uses_separate_sessions_for_safari_and_non_safari() {
    if !external_av_tools_available() {
        eprintln!("skipping: ffmpeg/ffprobe not on PATH");
        return;
    }

    let temp_dir = TempDir::new().unwrap();
    let root = temp_dir.path().join("library");
    fs::create_dir_all(&root).unwrap();
    generate_h264_ac3_mkv(&root.join("movie.mkv"));

    // Force `HlsFullTranscode` classification regardless of the synthetic
    // source file so we can verify browser-specific session splitting:
    // Safari should get the passthrough bucket, non-Safari the transcode one.
    let probe = write_probe_script(
        &temp_dir,
        "probe-full-transcode.sh",
        r#"#!/usr/bin/env bash
set -euo pipefail
cat <<'JSON'
{
  "format": { "format_name": "matroska,webm", "duration": "2.0" },
  "streams": [
    { "codec_type": "video", "codec_name": "hevc", "width": 320, "height": 240 },
    { "codec_type": "audio", "codec_name": "ac3" }
  ]
}
JSON
"#,
    );

    let library = LibraryConfig::from_paths(vec![root]).with_probe_command(probe);
    let hls_config = HlsConfig {
        cache_dir: temp_dir.path().join("hls-cache"),
        max_concurrent: 4,
        idle_secs: 60,
        ffmpeg_command: Some(PathBuf::from("ffmpeg")),
        hw_accel: Default::default(),
        vaapi_device: None,
    };

    let server = TestServer::spawn_with_library_and_hls(library, hls_config).await;
    server.scan_library().await;
    let library = server.wait_for_probes_complete().await;
    let item = library
        .items
        .into_iter()
        .next()
        .expect("scanned media item");

    assert_eq!(
        item.playback_mode,
        syncplay_backend::protocol::PlaybackMode::HlsFullTranscode
    );

    let url = format!("{}/api/media/{}/hls/master.m3u8", server.base_url, item.id);
    let safari_ua = "Mozilla/5.0 (Macintosh; Intel Mac OS X 12_6) AppleWebKit/605.1.15 (KHTML, like Gecko) Version/16.6 Safari/605.1.15";
    let chrome_ua = "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/120.0.0.0 Safari/537.36";

    let safari_response = server
        .client
        .get(&url)
        .header("user-agent", safari_ua)
        .send()
        .await
        .unwrap();
    assert_eq!(safari_response.status(), StatusCode::OK);

    let chrome_response = server
        .client
        .get(&url)
        .header("user-agent", chrome_ua)
        .send()
        .await
        .unwrap();
    assert_eq!(chrome_response.status(), StatusCode::OK);

    let manager = server.hls.as_ref().expect("hls manager handle");
    assert_eq!(
        manager.session_count().await,
        2,
        "incompatible browser pipelines should not share one HLS session"
    );
}

#[cfg(unix)]
#[tokio::test]
async fn hls_rejects_invalid_filenames() {
    if !external_av_tools_available() {
        eprintln!("skipping: ffmpeg/ffprobe not on PATH");
        return;
    }

    let temp_dir = TempDir::new().unwrap();
    let (server, item) = hls_test_setup(&temp_dir).await;

    let response = server
        .client
        .get(format!(
            "{}/api/media/{}/hls/secret.txt",
            server.base_url, item.id
        ))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
}

#[cfg(unix)]
#[tokio::test]
async fn hls_out_of_range_segment_returns_404_without_respawn() {
    if !external_av_tools_available() {
        eprintln!("skipping: ffmpeg/ffprobe not on PATH");
        return;
    }

    let temp_dir = TempDir::new().unwrap();
    let (server, item) = hls_test_setup(&temp_dir).await;
    let manager = server.hls.as_ref().expect("hls manager handle");

    // First request boots the session and spawns ffmpeg once.
    let master = server
        .client
        .get(format!(
            "{}/api/media/{}/hls/master.m3u8",
            server.base_url, item.id
        ))
        .send()
        .await
        .unwrap();
    assert_eq!(master.status(), StatusCode::OK);
    assert_eq!(manager.total_spawn_count().await, 1);

    // Deliberately impossible segment index for a tiny test clip.
    let out_of_range = server
        .client
        .get(format!(
            "{}/api/media/{}/hls/seg_0_99999.m4s",
            server.base_url, item.id
        ))
        .send()
        .await
        .unwrap();
    assert_eq!(out_of_range.status(), StatusCode::NOT_FOUND);
    assert_eq!(
        manager.total_spawn_count().await,
        1,
        "out-of-range segment must not trigger a respawn"
    );

    // Sanity: valid segment fetch still works and still without extra spawns.
    let variant = server
        .client
        .get(format!(
            "{}/api/media/{}/hls/stream_0.m3u8",
            server.base_url, item.id
        ))
        .send()
        .await
        .unwrap();
    assert_eq!(variant.status(), StatusCode::OK);
    let variant_body = variant.text().await.unwrap();
    let first_segment = variant_body
        .lines()
        .find(|line| line.ends_with(".m4s"))
        .expect("variant should list at least one segment");

    let ok_segment = server
        .client
        .get(format!(
            "{}/api/media/{}/hls/{}",
            server.base_url, item.id, first_segment
        ))
        .send()
        .await
        .unwrap();
    assert_eq!(ok_segment.status(), StatusCode::OK);
    assert_eq!(manager.total_spawn_count().await, 1);
}
