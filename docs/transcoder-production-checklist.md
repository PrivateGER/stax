# Transcoder Hardening Checklist

This checklist is ordered by implementation sequence.
Each item is framed as a shippable issue with acceptance criteria.

## 1) Add Targeted Regression Coverage

- Scope:
  - Add integration tests for known transcoder failure paths before behavior changes.
  - Cover mixed-browser requests, duplicate audio rendition names, missing duration, and out-of-range segment requests.
- Acceptance criteria:
  - Tests fail on current broken behavior and pass after fixes.
  - Tests run in `backend/tests/sync_api.rs` and are deterministic on CI.
  - Each bug fixed below has at least one explicit regression test.

## 2) Fix Browser-Capability Session Pinning

- Scope:
  - Stop letting the first requester's user agent decide transcoder behavior for all later clients on the same media session.
  - Key session compatibility by effective pipeline requirements, or split incompatible clients into separate sessions.
- Acceptance criteria:
  - A Safari-first request does not break Chrome/Firefox playback for the same media.
  - Concurrent Safari and non-Safari clients can both play the same title successfully.
  - Session behavior is stable and documented in code comments and tests.

## 3) Make Audio Rendition IDs Collision-Proof

- Scope:
  - Replace name-only derivation for audio variant identity with stable unique identifiers.
  - Keep human-readable labels while guaranteeing unique URIs and `var_stream_map` names.
- Acceptance criteria:
  - Two audio tracks with identical language/title do not collide.
  - Master playlist audio URIs always map to files ffmpeg actually writes.
  - Duplicate-name regression test passes.

## 4) Enforce Segment Bounds Before Respawn

- Scope:
  - Validate requested segment index against the `SegmentPlan` range.
  - Reject impossible segment requests early instead of respawning ffmpeg at end-of-file.
- Acceptance criteria:
  - Requests above the max segment index return a bounded client error (`404` or `416` per chosen contract), without respawn.
  - Respawn counter remains unchanged for invalid out-of-range requests.
  - Valid in-range segments still work unchanged.

## 5) Fix Unknown-Duration Plan Behavior

- Scope:
  - Remove zero-length VOD plan behavior when `duration_seconds` is missing.
  - Use a safe fallback mode that does not advertise fake final duration.
- Acceptance criteria:
  - No synthesized playlist is emitted with only `EXTINF:0.000` for unknown-duration media.
  - Playback starts and remains seek-safe according to chosen fallback semantics.
  - Regression test for missing duration passes.

## 6) Hold In-Flight Guard Through Full Response Streaming

- Scope:
  - Ensure eviction protection lives for the lifetime of response body transmission, not only pre-response setup.
  - Tie guard lifetime to response body stream completion.
- Acceptance criteria:
  - Idle eviction cannot remove a session while a segment/init/master response is actively streaming.
  - Concurrency test with forced eviction interval shows no mid-stream teardown.
  - Session `in_flight` accounting returns to zero after stream completion.

## 7) Surface Actionable ffmpeg Startup Errors

- Scope:
  - Persist stderr snippets from startup failures into session error state.
  - Return structured, useful error context in API responses/logs without leaking unsafe internals.
- Acceptance criteria:
  - Startup failures include concrete reasons (codec mismatch, missing encoder, invalid arg, permission error).
  - `HlsError::SpawnFailed` includes propagated stderr excerpt.
  - Logs and API behavior are consistent and tested.

## 8) Add Respawn Backoff and Abuse Protection

- Scope:
  - Prevent repeated expensive respawns from malformed or hostile request patterns.
  - Add cooldown/backoff and per-session respawn rate limits.
- Acceptance criteria:
  - Burst invalid seek/segment requests do not trigger unbounded respawns.
  - Backoff behavior is deterministic and observable via metrics/log fields.
  - Legitimate seeks still succeed within acceptable latency.

## 9) Add Resource Limits and Cleanup Guarantees

- Scope:
  - Add hard limits for concurrent sessions, per-session runtime, scratch disk usage, and stale artifact cleanup.
  - Ensure cleanup works after crash/restart and under partial failure.
- Acceptance criteria:
  - Configurable caps are enforced under load tests.
  - Scratch directory cannot grow without bound.
  - Process and artifact cleanup completes on shutdown and idle eviction.

## 10) Add Transcoder Observability and SLO Gates

- Scope:
  - Emit metrics and logs needed to operate the transcoder in production.
  - Define explicit SLO targets for startup and seek latency, and error rates.
- Acceptance criteria:
  - Metrics available at minimum:
    - startup latency (`spawn` to first playable segment)
    - seek latency (request to segment served)
    - respawn count and reason
    - ffmpeg exit status/reason
    - HLS endpoint error rates by status
  - Alert thresholds are documented.
  - Release gate documented with target p95/p99 values and max error budget.

## Release Gate (Must Be True Before "Production Ready")

- All checklist items above are implemented and merged.
- Regression tests for every previously identified transcoder bug are passing.
- End-to-end soak test (long playback + repeated seeks + multi-client mixed browser) passes without leaked sessions/processes.
- On-call diagnostics can identify root cause of startup and seek failures from logs and metrics alone.
