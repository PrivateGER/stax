# Syncplay

Starter monorepo for a synced media server with a Rust backend and React frontend.

## Stack

- Backend: Rust, Axum, Tokio
- Frontend: React, TypeScript, Vite

## Layout

- `backend/`: Rust API server, room clock, websocket protocol, and integration tests
- `frontend/`: browser UI for library browsing, playback, and optional Watch Together sessions
- `docs/`: architecture and implementation planning documents

## Architecture

- See [docs/architecture.md](/home/latte/WebstormProjects/syncplay/docs/architecture.md) for the target system design and phased implementation roadmap.

## Product Direction

- See [docs/product-direction.md](/home/latte/WebstormProjects/syncplay/docs/product-direction.md) for the intended Plex-like, library-first product shape and UX guardrails.

## Quick start

### Backend

```bash
cd backend
cargo run
```

The API listens on `http://127.0.0.1:3001` by default.
Room and playback state now persist in SQLite. Set `SYNCPLAY_DATABASE_PATH=/path/to/syncplay.db` to override the default `backend/syncplay.db` location when running from the backend directory.
Set `SYNCPLAY_LIBRARY_ROOTS=/media/movies:/media/shows` to configure one or more library roots for indexing on Unix-like systems.
Set `SYNCPLAY_FFPROBE_BIN=/usr/local/bin/ffprobe` to override the probe binary used for media metadata extraction.
Set `SYNCPLAY_FFMPEG_BIN=/usr/local/bin/ffmpeg` (or empty to disable) to control the binary used for thumbnail generation, and `SYNCPLAY_THUMBNAIL_DIR=/var/lib/syncplay/thumbnails` to choose where generated thumbnails are cached on disk.

### Frontend

```bash
cd frontend
npm install
npm run dev
```

The Vite dev server listens on `http://127.0.0.1:5173` and proxies `/api` requests to the backend.

## Current endpoints

- `GET /api/health`
- `GET /api/library`
- `POST /api/library/scan`
- `GET /api/media/:media_id/stream`
- `GET /api/media/:media_id/subtitles/:track_index`
- `GET /api/media/:media_id/subtitles/embedded/:stream_index`
- `GET /api/media/:media_id/thumbnail`
- `GET /api/rooms`
- `POST /api/rooms`
- `GET /api/rooms/:room_id/ws`

## Realtime protocol

The room WebSocket accepts JSON playback commands:

- `{"type":"play","positionSeconds":12.3}`
- `{"type":"pause","positionSeconds":12.3}`
- `{"type":"seek","positionSeconds":42.0}`
- `{"type":"reportPosition","positionSeconds":42.4}`

The backend sends JSON events for:

- initial room snapshots
- playback updates
- presence changes
- drift correction suggestions
- protocol errors

Room snapshots and playback updates now include authoritative clock metadata:

- `positionSeconds`: effective position at the time the event was emitted
- `anchorPositionSeconds`: server-side anchor position used for the room clock
- `clockUpdatedAt`: when the room clock anchor last changed
- `emittedAt`: when the current state snapshot was generated
- `playbackRate`: current server playback rate
- `driftToleranceSeconds`: delta considered “in sync”

## Testing

### Backend

```bash
cd backend
cargo test
```

The backend now includes unit tests plus integration tests that exercise real HTTP and WebSocket flows, including validation failures and drift correction behavior.
Those tests also cover restart recovery against a real SQLite database, repeated library rescans for changed or deleted files, stubbed `ffprobe` metadata extraction success and failure paths, sidecar subtitle discovery and persistence, subtitle streaming with on-the-fly SRT to WebVTT conversion, and byte-range media streaming.

## Next steps

- Add room-selected media so the synchronized player is tied to room state instead of a local preview choice
- Add browser tests for two-tab pause/play/seek behavior with the real player
- Add optional HLS transcoding for formats browsers cannot direct-play
