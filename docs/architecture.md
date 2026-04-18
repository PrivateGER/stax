# Syncplay Architecture

This document describes the target architecture for Syncplay as it evolves from a realtime sync prototype into a simple, robust self-hosted media server for synchronized playback.

For product and UX priorities, see [product-direction.md](/home/latte/WebstormProjects/syncplay/docs/product-direction.md). This architecture document focuses on system shape, not final user-facing hierarchy.

It is intentionally biased toward:

- direct play before transcoding
- one well-supported web client before many device targets
- reliable room synchronization before advanced media-server features
- a small number of deployable processes

## Current State

Today the repository already provides:

- a Rust backend with HTTP and WebSocket endpoints
- authoritative room clocks for shared playback state
- drift classification for connected clients
- a React frontend with library browsing and real browser playback
- backend unit tests and HTTP/WebSocket integration tests

What does not exist yet:

- room-owned media selection
- a Plex-like, library-first frontend shell
- artwork-rich title/details pages
- robust browser-level watch-together validation
- file streaming fallback via HLS or transcoding
- auth and multi-user policy
- background transcoding jobs

## Product Goal

The near-term goal is not "replace Plex."

The goal is:

1. index a local media library
2. let a user browse titles and start playback quickly
3. serve supported media to a browser client
4. let users optionally create or join rooms for shared playback
5. keep playback synchronized with server-authoritative timing
6. degrade predictably when drift, lag, or unsupported media occurs

## System Overview

The recommended deployed system is a small monolith plus background workers:

```text
+-------------------+        HTTP / WebSocket        +----------------------+
| React Web Client  | <----------------------------> | Rust API / Sync App   |
| - room UI         |                                | - REST API            |
| - video player    |                                | - room sync service   |
| - drift handling  |                                | - streaming endpoints |
+-------------------+                                | - auth/session logic  |
                                                     +----------+-----------+
                                                                |
                                                                |
                                         +----------------------+----------------------+
                                         |                                             |
                                         v                                             v
                              +--------------------+                      +----------------------+
                              | SQLite             |                      | Local Filesystem     |
                              | - rooms            |                      | - media library      |
                              | - sessions         |                      | - poster cache       |
                              | - library index    |                      | - HLS/transcode temp |
                              +--------------------+                      +----------------------+
                                                                |
                                                                v
                                                     +----------------------+
                                                     | Worker Jobs          |
                                                     | - ffprobe metadata   |
                                                     | - thumbnails         |
                                                     | - optional HLS prep  |
                                                     +----------------------+
```

## High-Level Components

### 1. Web Client

Location:

- `frontend/`

Responsibilities:

- room list and room creation
- WebSocket connection lifecycle
- media playback UI
- drift reporting and correction handling
- library browsing and media selection

Non-goals for v1:

- native TV apps
- offline support
- multi-platform player wrappers

### 2. API and Sync App

Location:

- `backend/`

Responsibilities:

- REST endpoints for rooms, library, auth, and playback metadata
- WebSocket protocol for room sync
- authoritative playback clock per room
- validation, presence tracking, and room policy
- media streaming endpoints

Reasoning:

- keep the deployment simple
- keep the sync and streaming logic close together
- avoid splitting into multiple services before the domain stabilizes

### 3. Storage Layer

Primary store:

- SQLite for v1

Responsibilities:

- persist rooms and room membership metadata
- persist library index and probed media metadata
- persist playback sessions and resume points
- persist user settings later if auth is added

Why SQLite first:

- easy local deployment
- enough for a single-host self-hosted app
- supports transactional updates around room/session changes

### 4. Media Library and Streaming Layer

Responsibilities:

- scan configured folders
- record file identity, size, duration, codec info, subtitles, and timestamps
- serve direct-play media through HTTP range requests
- expose poster, thumbnail, and metadata endpoints
- optionally expose HLS manifests and segments for unsupported direct-play cases

### 5. Worker Jobs

Responsibilities:

- `ffprobe` media inspection
- thumbnail/poster extraction
- subtitle detection and normalization
- optional background HLS/transcode preparation

Recommended model:

- start in-process with a Tokio job queue
- move to a dedicated worker binary only if workload or failure isolation requires it

## Backend Module Boundaries

Recommended backend layout:

```text
backend/src/
  main.rs              # startup and bind/listen only
  lib.rs               # app assembly and module exports
  protocol.rs          # REST/WebSocket request and event types
  clock.rs             # authoritative playback clock and drift math
  rooms.rs             # room lifecycle, presence, orchestration
  library.rs           # media library API and scan orchestration
  streaming.rs         # range requests, HLS endpoints, content headers
  persistence.rs       # SQLite access and transactions
  auth.rs              # optional, later phase
  workers.rs           # background job coordination
```

Current repo status:

- `main.rs`, `lib.rs`, `protocol.rs`, and `clock.rs` already exist
- `rooms.rs`, `library.rs`, `streaming.rs`, and `persistence.rs` are the next useful modules

## Frontend Module Boundaries

Recommended frontend layout:

```text
frontend/src/
  App.tsx
  api/
    client.ts
    rooms.ts
    library.ts
  hooks/
    useRoomSocket.ts
    useAuthoritativeClock.ts
  features/
    rooms/
    player/
    library/
  types/
    protocol.ts
```

The important separation is:

- network transport and protocol parsing
- player state and correction logic
- UI components

## Core Domain Model

### Room

A room is the collaboration boundary for synchronized playback.

Fields:

- `id`
- `name`
- `media_id` or selected media reference
- `created_at`
- `playback_state`
- `connection_count`

### Playback State

Current shape already includes:

- `status`
- `positionSeconds`
- `anchorPositionSeconds`
- `clockUpdatedAt`
- `emittedAt`
- `playbackRate`
- `driftToleranceSeconds`

The backend should remain authoritative for this state.

### Media Item

Recommended fields:

- `id`
- `library_path`
- `container`
- `video_codec`
- `audio_codec`
- `duration_seconds`
- `width`
- `height`
- `bitrate`
- `subtitle_tracks`
- `poster_path`
- `created_at`
- `updated_at`

### Playback Session

Recommended fields:

- `room_id`
- `media_id`
- `started_at`
- `last_state_change_at`
- `last_anchor_position_seconds`
- `status`

## Sync Model

The sync model should stay server-authoritative.

Rules:

1. clients send intent, not truth
2. the server updates the room clock
3. the server broadcasts the new state
4. clients derive current position locally between events
5. clients periodically report observed position back to the server
6. the server responds with correction guidance

Correction classes:

- `inSync`: client is close enough, do nothing
- `nudge`: adjust playback rate or small offset locally
- `seek`: jump to the authoritative position

Recommended practical thresholds:

- tolerance: around `0.25s` to `0.35s`
- nudge band: up to around `1.5s`
- beyond that: hard seek

## Media Delivery Strategy

### Phase 1: Direct Play Only

Support:

- MP4/H.264/AAC first

Transport:

- HTTP range requests from the Rust backend

Why:

- simplest robust path
- works with the native browser video element
- avoids immediate transcoding complexity

### Phase 2: HLS Fallback

Use HLS when:

- the container or codec is not browser-friendly
- subtitles need burn-in or packaging support
- seeking/buffering behavior benefits from segmentation

Implementation detail:

- use `ffmpeg` as an external process
- cache manifests/segments in a temp work directory
- limit concurrent transcodes

### Phase 3: Pre-transcode or Background Preparation

Only add this if needed for user experience or CPU smoothing.

## Persistence Strategy

Use SQLite tables roughly like:

```text
rooms
media_items
media_tracks
playback_sessions
library_roots
scan_jobs
artifacts
```

Suggested rule:

- room sync stays in memory for speed
- room-relevant durable state is checkpointed to SQLite
- library and scan metadata lives primarily in SQLite

## API Surface

### Existing

- `GET /api/health`
- `GET /api/rooms`
- `POST /api/rooms`
- `GET /api/rooms/:room_id/ws`

### Recommended Next HTTP Endpoints

- `GET /api/library`
- `POST /api/library/roots`
- `POST /api/library/scan`
- `GET /api/media/:media_id`
- `GET /api/media/:media_id/stream`
- `GET /api/media/:media_id/poster`
- `GET /api/media/:media_id/hls/master.m3u8`

### Recommended Next WebSocket Events

Current events are good for sync.

Possible additions later:

- `mediaSelected`
- `bufferingStateChanged`
- `subtitleTrackChanged`
- `serverNotice`

## Security and Trust Model

For v1:

- single trusted household or friend group
- simple shared-secret or session-based auth is enough

Later:

- user accounts
- room permissions
- media library access policy

Do not block core streaming/sync work on a full auth system.

## Deployment Model

Recommended v1 deployment:

- one Rust backend process
- one frontend dev server in development
- static frontend assets served by the backend in production later
- local filesystem media library mount
- SQLite file on local disk

Production simplification later:

- build frontend static assets
- serve them from the Rust app or a reverse proxy

## Observability

Add basic observability early:

- structured logs for room joins, leaves, play, pause, seek, drift reports
- counters for active rooms and active sockets
- timing for library scans and probe jobs
- error logs for stream startup and transcode failures

This can remain lightweight for a long time.

## Failure Modes To Design Around

- client reconnects during active playback
- duplicate or malformed WebSocket commands
- drift-report spam or bursty reports
- unsupported codec discovered only at playback time
- media file changed or deleted after indexing
- long-running transcodes consuming all CPU
- browser buffering causing apparent drift

## Implementation Roadmap

The implementation order matters. This roadmap is designed to keep the app usable at every stage.

### Phase 0: Current Baseline

Status:

- already done

Includes:

- room CRUD
- room WebSockets
- authoritative room clock
- drift classification
- control-booth frontend
- backend integration tests

### Phase 1: Persistence

Status:

- done for the current slice
- room and playback checkpoint persistence are implemented
- SQL migrations are implemented
- restart recovery tests are implemented

Goal:

- make rooms and library metadata durable

Steps:

1. add SQLite and a small persistence layer
2. persist room creation and room-selected media
3. persist playback session checkpoints when state changes
4. add migrations
5. add tests for restart-safe room/session recovery

Deliverable:

- a restart no longer loses the important room and playback metadata

### Phase 2: Library Roots and Scanning

Status:

- in progress
- configured library roots are supported
- recursive media discovery with extension filtering is implemented
- `ffprobe` metadata extraction is implemented
- library items are persisted and exposed over HTTP
- repeated scan, changed file, deleted file, and restart recovery tests are implemented

Goal:

- understand what media exists on disk

Steps:

1. add `library_roots` configuration
2. implement recursive file discovery with extension filtering
3. run `ffprobe` for media metadata extraction
4. persist media items and track details
5. expose library listing endpoints
6. add tests for repeated scans, changed files, and deleted files

Deliverable:

- frontend can browse indexed media instead of typing titles manually

### Phase 3: Real Media Playback in Browser

Status:

- in progress
- direct media streaming with HTTP range support is implemented
- browser player wiring is implemented
- sidecar subtitle discovery and browser subtitle selection are implemented

Goal:

- replace the simulated drift monitor with a real `<video>` player

Steps:

1. add `GET /api/media/:media_id/stream` with HTTP range support
2. build a player panel in the frontend
3. connect room state to real media element controls
4. report actual player position on an interval
5. apply `inSync` / `nudge` / `seek` responses to the player
6. expose subtitle sidecars and browser subtitle track selection
7. add manual tests for pause/play/seek/reconnect across two clients

Deliverable:

- two browser tabs can actually watch the same file in sync

### Phase 4: Selected Media Per Room

Goal:

- rooms should coordinate around a concrete media item

Steps:

1. add `mediaSelected` room state
2. let room host or participants pick a media item
3. broadcast selection changes over WebSocket
4. prevent playback commands until media is selected
5. add tests for selection switching during an active session

Deliverable:

- rooms become tied to actual indexed media, not placeholder titles

### Phase 5: Better Drift Handling

Goal:

- turn drift classification into practical correction behavior

Steps:

1. add client-side nudge handling with small temporary playback-rate changes
2. suppress corrections while the media element is buffering
3. include optional buffering reports in the protocol
4. tune thresholds based on observed behavior
5. add integration or browser tests around correction frequency and convergence

Deliverable:

- sync feels smooth instead of jumpy

### Phase 6: HLS Fallback

Goal:

- play a wider range of media while preserving sync

Steps:

1. detect when a file is not direct-play compatible
2. add `ffmpeg`-driven HLS manifest/segment generation
3. cache manifests and segments
4. expose HLS endpoints
5. add frontend player fallback for HLS
6. add resource limits and cleanup policies

Deliverable:

- browser playback works for more than one ideal codec stack

### Phase 7: Basic Authentication and Shared Access

Goal:

- support non-trusted local deployments a bit more safely

Steps:

1. add simple auth
2. gate library management and scan operations
3. add session cookies or token-based access
4. add audit-friendly logs for room joins and media access

Deliverable:

- safer multi-user home deployment

### Phase 8: Production Hardening

Goal:

- make the system stable for longer-running usage

Steps:

1. serve frontend assets from production backend
2. add startup recovery for persisted room sessions
3. add graceful worker shutdown and scan resume
4. add artifact cleanup and disk-space safeguards
5. add configuration docs and deployment examples

Deliverable:

- a credible self-hosted beta

## Immediate Next Tasks

If work continues from the current repository state, the next best sequence is:

1. add SQLite migrations and a persistence module
2. persist room and playback-session state
3. implement library roots plus `ffprobe` scanning
4. add direct media streaming with HTTP range requests
5. replace the frontend drift simulator with a real video player

This ordering keeps the existing sync work relevant and avoids building a player around fake media forever.
