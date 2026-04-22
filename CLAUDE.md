# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Repository Layout

Monorepo with two independent projects:

- `backend/` — Rust (Axum + Tokio + SQLx/SQLite) HTTP+WebSocket server
- `frontend/` — React 19 + TypeScript + Vite SPA
- `docs/architecture.md` and `docs/product-direction.md` — target system design, phased roadmap, and UX guardrails. Read these before making architectural changes; they also distinguish v1 non-goals (auth, native apps, transcoding) from what belongs in scope.

## Common Commands

Backend (run from `backend/`):

```bash
cargo run                       # starts API on 127.0.0.1:3001
cargo test                      # runs unit tests + tests/sync_api.rs integration tests
cargo test <name>               # runs a single test by substring
cargo test --test sync_api      # runs only the integration test binary
```

Frontend (run from `frontend/`):

```bash
npm install
npm run dev                     # vite dev server on 127.0.0.1:5173, proxies /api → backend
npm run build                   # tsc -b && vite build (type-check is part of build)
npm run preview
```

There is no top-level workspace — backend and frontend are built independently.

## Runtime Configuration

Backend reads from env vars (see `main.rs` and `library.rs`):

- `SYNCPLAY_API_ADDR` — bind address, defaults to `127.0.0.1:3001`
- `SYNCPLAY_DATABASE_PATH` — SQLite file, defaults to `backend/syncplay.db`
- `SYNCPLAY_LIBRARY_ROOTS` — colon-separated list of media directories to index
- `SYNCPLAY_WALK_WORKERS` — concurrent directory listings during the scan walk (default `8`). Tune higher when indexing high-latency mounts (SMB, sshfs) where each `read_dir` round-trips over the network. The walk is the synchronous portion of `POST /api/library/scan`; probes and thumbnails happen afterwards in the background pools.
- `SYNCPLAY_FFPROBE_BIN` — override the `ffprobe` binary used for probing
- `SYNCPLAY_PROBE_WORKERS` — concurrent `ffprobe` invocations the background probe pool runs in parallel (default `4`). The scan walk only persists rows; ffprobe runs asynchronously and the frontend polls until `probed_at` / `probe_error` lands. Scans reuse persisted probe results when a file's `(size_bytes, modified_at)` is unchanged, so a re-scan of a stable library issues zero probes.
- `SYNCPLAY_FFMPEG_BIN` — override the `ffmpeg` binary used for thumbnail generation (empty string disables)
- `SYNCPLAY_THUMBNAIL_DIR` — cache directory for generated thumbnails (default `syncplay-thumbnails`)
- `SYNCPLAY_THUMBNAIL_WORKERS` — concurrent ffmpeg processes the background thumbnail pool will run (default `2`). Generation is asynchronous: scans return immediately and workers fill in `thumbnail_generated_at` / `thumbnail_error` as they complete.
- `SYNCPLAY_FRONTEND_ORIGIN` — tightens CORS to a specific origin (default: Any)
- `SYNCPLAY_HLS_HW_ACCEL` — hardware accelerator for the HLS full-transcode tier; one of `none` (default), `nvenc`, `vaapi`, `qsv`, `videotoolbox`. Unknown values warn and fall back to `none`. Only the `HlsFullTranscode` tier is affected — `HlsRemux` and `HlsAudioTranscode` always use `-c:v copy`.
- `SYNCPLAY_HLS_VAAPI_DEVICE` — DRM render node for VAAPI (default `/dev/dri/renderD128`).

## Architecture Notes

### Server-authoritative sync model

The backend owns the playback clock for every room. Clients send *intent* (`play`, `pause`, `seek`, `reportPosition`) over a per-room WebSocket; the server updates `AuthoritativePlaybackClock` (in `clock.rs`), broadcasts `PlaybackUpdated` events to the room, and responds to `reportPosition` with a `DriftCorrection` classified as `inSync` / `nudge` / `seek`. Clients derive their current position locally between events. Never move authoritative state to the client.

### Backend module boundaries (`backend/src/`)

- `main.rs` — bind/listen only
- `lib.rs` — app assembly: `AppState`, `RoomHub`, router, WebSocket handler, request validation
- `protocol.rs` — all HTTP request/response and WebSocket event types (serde)
- `clock.rs` — `AuthoritativePlaybackClock`, drift math, timestamp formatting
- `library.rs` — library roots, recursive scanning, `ffprobe` metadata, sidecar subtitle discovery
- `thumbnails.rs` — background `ThumbnailWorkerPool` (semaphore-bounded), source-aware generation pipeline (sidecar art → embedded `attached_pic` → `ffmpeg thumbnail` filter)
- `streaming.rs` — HTTP range responses for media, on-the-fly SRT → WebVTT conversion for subtitles
- `persistence.rs` — SQLx/SQLite access, migrations in `backend/migrations/*.sql`

`RoomHub` holds per-room state: a `tokio::sync::RwLock<RoomRecord>` guarded by a `broadcast` channel for fanout, plus a connection counter. Playback commands acquire the write lock, mutate a clone of the `RoomRecord`, persist via `Persistence::save_room`, then swap it in — so a failed write leaves in-memory state untouched. Keep this pattern when adding new state-mutating WebSocket commands.

### Persistence

SQLite is the source of durable truth. Rooms and library metadata are loaded into memory at startup (`load_state_with_library`) and checkpointed back on mutation. Migrations are numbered SQL files in `backend/migrations/` and applied by `sqlx::migrate!` at startup. Add a new migration rather than editing existing ones.

### Tests

- Unit tests live inline (`#[cfg(test)] mod tests`) in backend modules.
- Integration tests in `backend/tests/sync_api.rs` spin up a real HTTP/WebSocket server against an in-memory SQLite and exercise restart recovery, rescan behavior, stubbed `ffprobe`, subtitle conversion, and byte-range streaming. Use `Persistence::open_in_memory()` + `load_state_with_library` for new integration tests.

### Frontend

- Hash-based router in `router.ts` with routes `library`, `title`, `watch`, `admin`. No react-router.
- Pages in `src/pages/` (`LibraryPage`, `TitlePage`, `PlayerPage`, `AdminPage`) — library-first navigation per `docs/product-direction.md`; the room/control UI is a layer on top of playback, not the landing page.
- `useRoomSocket.ts` owns the WebSocket lifecycle and exposes playback state + drift events to the player.
- API types in `types.ts` mirror the Rust `protocol.rs` shapes — keep them in sync when adding fields.
- Vite proxies `/api` (including WebSocket upgrades) to the backend, so frontend code should always use relative `/api/...` paths.

## Working Style

- Follow the phased roadmap in `docs/architecture.md`. Persistence, library scanning, and direct-play streaming are done; selected-media-per-room, improved drift handling, and HLS fallback are the next phases.
- Before adding a feature, check whether `docs/product-direction.md` classifies it as an explicit non-goal (e.g. device targets beyond the browser, drift diagnostics in the main UI).
