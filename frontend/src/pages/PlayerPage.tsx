import { useCallback, useEffect, useRef, useState } from "react";

import { api, subtitleUrl } from "../api";
import {
  displayMediaTitle,
  formatAudioStreamLabel,
  formatLanguageName,
  formatSignedDelta,
  formatSubtitleStreamLabel,
  formatSubtitleTrackLabel,
  formatTimeCode,
} from "../format";
import { navigate } from "../router";
import { StreamCopyProgress } from "../streamCopyProgress";
import type { MediaItem, Room } from "../types";
import {
  deriveExpectedPosition,
  monotonicNow,
  useRoomSocket,
} from "../useRoomSocket";
import { usePlayerSource } from "../usePlayerSource";
import { useStreamCopyProgress } from "../useStreamCopyProgress";
import { useWatchTogether } from "../useWatchTogether";

type Props = {
  item: MediaItem | null;
  roomId: string | null;
  rooms: Room[];
  clientName: string;
  onClientNameChange: (name: string) => void;
  onRefresh: () => void;
  onRoomCreated: (room: Room) => void;
};

type AudioTrackEntry = { id: string; label: string };

// Minimal shape for the HTMLMediaElement.audioTracks API — present in Safari
// and behind flags in Chromium, and not modelled by TypeScript's lib.dom.
type AudioTrackLike = {
  id?: string;
  label?: string;
  language?: string;
  enabled: boolean;
};

type AudioTrackListLike = {
  length: number;
  [index: number]: AudioTrackLike | undefined;
  addEventListener: (type: string, listener: () => void) => void;
  removeEventListener: (type: string, listener: () => void) => void;
};

export function PlayerPage({
  item,
  roomId,
  rooms,
  clientName,
  onClientNameChange,
  onRefresh,
  onRoomCreated,
}: Props) {
  const videoRef = useRef<HTMLVideoElement | null>(null);
  const nudgeResetRef = useRef<number | null>(null);
  const tracksMenuRef = useRef<HTMLDivElement | null>(null);
  const tracksButtonRef = useRef<HTMLButtonElement | null>(null);
  const [selectedSubtitleIndex, setSelectedSubtitleIndex] = useState<number | null>(null);
  const [audioTracks, setAudioTracks] = useState<AudioTrackEntry[]>([]);
  const [selectedAudioId, setSelectedAudioId] = useState<string | null>(null);
  const [tracksMenuOpen, setTracksMenuOpen] = useState(false);
  const [playerError, setPlayerError] = useState<string | null>(null);
  const [creatingStreamCopy, setCreatingStreamCopy] = useState(false);
  const [streamCopyError, setStreamCopyError] = useState<string | null>(null);
  const [clockTickMs, setClockTickMs] = useState<number>(monotonicNow());
  const [showSessionPanel, setShowSessionPanel] = useState<boolean>(Boolean(roomId));

  const { summary: liveStreamCopy, seedFromCreate: seedLiveStreamCopy } =
    useStreamCopyProgress({
      mediaId: item?.id ?? null,
      fallback: item?.streamCopy ?? null,
      onRefresh,
    });

  const socket = useRoomSocket(roomId, clientName);
  const watchTogether = useWatchTogether(item, onRoomCreated);
  const live = socket.connectionState === "live";
  const { fatalError: sourceError } = usePlayerSource(videoRef, item);
  const playable = item?.preparationState === "direct" || item?.preparationState === "prepared";
  const subtitleSources = item
    ? item.preparationState === "prepared" && item.streamCopy?.subtitleUrl
      ? [
          {
            key: `${item.id}-prepared`,
            label: preparedSubtitleLabel(item),
            src: item.streamCopy.subtitleUrl,
            language: preparedSubtitleLanguage(item),
          },
        ]
      : item.subtitleTracks.map((track, index) => ({
          key: `${item.id}-${track.relativePath}`,
          label: formatSubtitleTrackLabel(track, index + 1),
          src: subtitleUrl(item.id, index),
          language: track.language ?? undefined,
        }))
    : [];

  useEffect(() => {
    if (sourceError) {
      setPlayerError(sourceError);
    }
  }, [sourceError]);

  useEffect(() => {
    setShowSessionPanel(Boolean(roomId));
  }, [roomId]);

  useEffect(() => {
    setSelectedSubtitleIndex(null);
    setAudioTracks([]);
    setSelectedAudioId(null);
    setTracksMenuOpen(false);
    setPlayerError(null);
    setStreamCopyError(null);
  }, [item?.id]);

  // Track the set of embedded audio tracks exposed by the browser. Support is
  // patchy (Safari: yes; Chromium: behind a flag), so when `audioTracks` is
  // missing we just render nothing — no-op on unsupported browsers.
  useEffect(() => {
    const video = videoRef.current;
    if (!video) return;

    const list = (video as HTMLVideoElement & { audioTracks?: AudioTrackListLike })
      .audioTracks;
    if (!list) return;

    const rebuild = () => {
      const entries: AudioTrackEntry[] = [];
      let activeId: string | null = null;
      for (let index = 0; index < list.length; index += 1) {
        const track = list[index];
        if (!track) continue;
        const id = track.id || String(index);
        const label = formatBrowserAudioTrackLabel(track, item, index);
        entries.push({ id, label });
        if (track.enabled) activeId = id;
      }
      setAudioTracks(entries);
      setSelectedAudioId(activeId);
    };

    rebuild();
    list.addEventListener("addtrack", rebuild);
    list.addEventListener("removetrack", rebuild);
    list.addEventListener("change", rebuild);
    // Tracks may only appear after metadata is parsed.
    video.addEventListener("loadedmetadata", rebuild);

    return () => {
      list.removeEventListener("addtrack", rebuild);
      list.removeEventListener("removetrack", rebuild);
      list.removeEventListener("change", rebuild);
      video.removeEventListener("loadedmetadata", rebuild);
    };
  }, [item?.id, item?.audioStreams]);

  // Close the tracks menu on outside click / Escape.
  useEffect(() => {
    if (!tracksMenuOpen) return;

    const handlePointer = (event: MouseEvent) => {
      const target = event.target as Node | null;
      if (!target) return;
      if (tracksMenuRef.current?.contains(target)) return;
      if (tracksButtonRef.current?.contains(target)) return;
      setTracksMenuOpen(false);
    };
    const handleKey = (event: KeyboardEvent) => {
      if (event.key === "Escape") setTracksMenuOpen(false);
    };

    document.addEventListener("mousedown", handlePointer);
    document.addEventListener("keydown", handleKey);

    return () => {
      document.removeEventListener("mousedown", handlePointer);
      document.removeEventListener("keydown", handleKey);
    };
  }, [tracksMenuOpen]);

  // If the room is anchored to a different media item than what's in the URL,
  // follow the room to its canonical media. This is what makes shared
  // Watch Together links robust.
  useEffect(() => {
    if (!roomId || !socket.room) return;
    const roomMediaId = socket.room.mediaId;
    if (!roomMediaId) return;
    if (item && roomMediaId === item.id) return;
    navigate({ name: "watch", mediaId: roomMediaId, roomId });
  }, [item, roomId, socket.room]);

  useEffect(() => {
    const tickId = window.setInterval(() => {
      setClockTickMs(monotonicNow());
    }, 250);

    return () => window.clearInterval(tickId);
  }, []);

  useEffect(() => {
    const video = videoRef.current;
    if (!video) return;

    for (let index = 0; index < video.textTracks.length; index += 1) {
      const track = video.textTracks[index]!;
      track.mode = selectedSubtitleIndex === index ? "showing" : "disabled";
    }
  }, [item, selectedSubtitleIndex]);

  useEffect(() => () => {
    if (nudgeResetRef.current !== null) {
      window.clearTimeout(nudgeResetRef.current);
    }
  }, []);

  // Periodically report our position to the room for drift correction.
  useEffect(() => {
    if (!live || !item || !socket.room) return;

    const video = videoRef.current;
    if (!video) return;

    const interval = window.setInterval(() => {
      const v = videoRef.current;
      if (!v) return;
      socket.send({
        type: "reportPosition",
        positionSeconds: Number(Math.max(0, v.currentTime).toFixed(3)),
      });
    }, 2000);

    return () => window.clearInterval(interval);
  }, [live, item, socket.room, socket]);

  // Follow room authoritative state when live.
  useEffect(() => {
    if (!live || !socket.room || socket.authoritativeReceiptAtMs === null || !item) return;

    const video = videoRef.current;
    if (!video) return;

    const expected = deriveExpectedPosition(
      socket.room,
      socket.authoritativeReceiptAtMs,
      clockTickMs,
    );
    const delta = video.currentTime - expected;
    const baseRate = socket.room.playbackState.playbackRate;

    if (Math.abs(video.playbackRate - baseRate) > 0.001) {
      video.playbackRate = baseRate;
    }

    if (socket.room.playbackState.status === "paused") {
      if (!video.paused) video.pause();
      if (Math.abs(delta) > 0.2) video.currentTime = expected;
      return;
    }

    const hardTolerance = Math.max(socket.room.playbackState.driftToleranceSeconds, 1.25);
    if (Math.abs(delta) > hardTolerance) {
      video.currentTime = expected;
    }

    if (video.paused && video.readyState >= HTMLMediaElement.HAVE_CURRENT_DATA) {
      void video.play().catch(() => {
        setPlayerError("Press play once — the browser blocked autoplay.");
      });
    }
  }, [live, socket.room, socket.authoritativeReceiptAtMs, clockTickMs, item]);

  // Apply drift correction nudges/seeks. Keep `item` out of the deps: library
  // polls hand us a fresh item reference every ~10s, and re-running this effect
  // would replay the last (stale) `expectedPositionSeconds` and yank playback
  // backward. A corrections-only trigger is what we actually want.
  useEffect(() => {
    if (!socket.lastCorrection || !live || !item) return;

    const video = videoRef.current;
    if (!video) return;

    const baseRate = socket.room?.playbackState.playbackRate ?? 1;

    if (nudgeResetRef.current !== null) {
      window.clearTimeout(nudgeResetRef.current);
      nudgeResetRef.current = null;
    }

    if (socket.lastCorrection.suggestedAction === "seek") {
      video.currentTime = socket.lastCorrection.expectedPositionSeconds;
      video.playbackRate = baseRate;
      return;
    }

    if (socket.lastCorrection.suggestedAction === "nudge") {
      const adjusted = baseRate + (socket.lastCorrection.deltaSeconds > 0 ? -0.08 : 0.08);
      video.playbackRate = Math.min(1.12, Math.max(0.88, Number(adjusted.toFixed(2))));
      nudgeResetRef.current = window.setTimeout(() => {
        if (videoRef.current) videoRef.current.playbackRate = baseRate;
      }, 1500);
      return;
    }

    video.playbackRate = baseRate;
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [socket.lastCorrection, live, item?.id, socket.room]);

  const handleSelectAudioTrack = useCallback((trackId: string) => {
    const video = videoRef.current;
    if (!video) return;
    const list = (video as HTMLVideoElement & { audioTracks?: AudioTrackListLike })
      .audioTracks;
    if (!list) return;

    for (let index = 0; index < list.length; index += 1) {
      const track = list[index];
      if (!track) continue;
      const id = track.id || String(index);
      track.enabled = id === trackId;
    }
    setSelectedAudioId(trackId);
  }, []);

  const handleSessionPlay = useCallback(() => {
    const v = videoRef.current;
    if (!v) return;
    socket.send({ type: "play", positionSeconds: Number(v.currentTime.toFixed(1)) });
  }, [socket]);

  const handleSessionPause = useCallback(() => {
    const v = videoRef.current;
    if (!v) return;
    socket.send({ type: "pause", positionSeconds: Number(v.currentTime.toFixed(1)) });
  }, [socket]);

  const handleSessionSeek = useCallback(() => {
    const v = videoRef.current;
    if (!v) return;
    socket.send({ type: "seek", positionSeconds: Number(v.currentTime.toFixed(1)) });
  }, [socket]);

  const handleCatchUp = useCallback(() => {
    if (!socket.room || socket.authoritativeReceiptAtMs === null) return;
    const video = videoRef.current;
    if (!video) return;

    const expected = deriveExpectedPosition(
      socket.room,
      socket.authoritativeReceiptAtMs,
      monotonicNow(),
    );
    video.currentTime = expected;

    if (socket.room.playbackState.status === "playing") {
      void video.play().catch(() => {
        setPlayerError("Press play once to catch up with the room.");
      });
    }
  }, [socket.room, socket.authoritativeReceiptAtMs]);

  function handleLeaveSession() {
    if (!item) return;
    navigate({ name: "watch", mediaId: item.id, roomId: null });
  }

  async function handleCreateStreamCopy() {
    if (!item) return;

    try {
      setCreatingStreamCopy(true);
      setStreamCopyError(null);
      const response = await api.createStreamCopy(item.id, {
        audioStreamIndex:
          item.audioStreams.find((stream) => stream.default)?.index ??
          item.audioStreams[0]?.index ??
          null,
        subtitleMode: "off",
        subtitle: null,
      });
      seedLiveStreamCopy(response);
      onRefresh();
    } catch (error) {
      setStreamCopyError(
        error instanceof Error ? error.message : "Could not create the stream copy.",
      );
    } finally {
      setCreatingStreamCopy(false);
    }
  }

  if (!item) {
    return (
      <section className="title-missing">
        <h1>No media selected</h1>
        <button
          className="primary-button"
          onClick={() => navigate({ name: "library", folder: null })}
          type="button"
        >
          Back to library
        </button>
      </section>
    );
  }

  const title = displayMediaTitle(item);

  if (!playable) {
    const liveActive =
      liveStreamCopy?.status === "queued" || liveStreamCopy?.status === "running";
    const isPreparing = liveActive || item.preparationState === "preparing";
    const liveFailed = liveStreamCopy?.status === "failed";
    const message = isPreparing
      ? "A stream copy is still being prepared for this title."
      : liveFailed || item.preparationState === "failed"
        ? liveStreamCopy?.error ??
          item.streamCopy?.error ??
          "The last stream copy attempt failed. Create a new one to try again."
        : item.preparationState === "unsupported"
          ? "This title is not supported for browser playback."
          : "This title needs a stream copy before it can be played.";

    return (
      <section className="title-missing">
        <h1>{title}</h1>
        <p className="muted">{message}</p>
        <StreamCopyProgress summary={liveStreamCopy} />
        {item.playbackMode === "needsPreparation" ? (
          <button
            className="primary-button"
            disabled={creatingStreamCopy || isPreparing}
            onClick={() => void handleCreateStreamCopy()}
            type="button"
          >
            {liveStreamCopy?.status === "queued"
              ? "Queued…"
              : liveStreamCopy?.status === "running"
                ? "Preparing…"
                : isPreparing
                  ? "Preparing…"
                  : creatingStreamCopy
                    ? "Submitting…"
                    : "Create stream copy"}
          </button>
        ) : null}
        {streamCopyError ? <p className="error">{streamCopyError}</p> : null}
        <button
          className="ghost-button"
          onClick={() => navigate({ name: "title", mediaId: item.id })}
          type="button"
        >
          Back to title
        </button>
      </section>
    );
  }

  return (
    <div className="player-page">
      <header className="player-bar">
        <button
          className="link-button"
          onClick={() => navigate({ name: "title", mediaId: item.id })}
          type="button"
        >
          ← {title}
        </button>

        <div className="player-bar-right">
          {subtitleSources.length > 0 || audioTracks.length > 1 ? (
            <div className="tracks-menu">
              <button
                aria-expanded={tracksMenuOpen}
                aria-haspopup="menu"
                className="ghost-button tracks-menu-button"
                onClick={() => setTracksMenuOpen((open) => !open)}
                ref={tracksButtonRef}
                type="button"
              >
                Tracks
              </button>
              {tracksMenuOpen ? (
                <div
                  className="tracks-menu-panel"
                  ref={tracksMenuRef}
                  role="menu"
                >
                  {subtitleSources.length > 0 ? (
                    <div className="tracks-menu-section">
                      <h3>Subtitles</h3>
                      <button
                        className={`tracks-menu-option ${selectedSubtitleIndex === null ? "active" : ""}`}
                        onClick={() => {
                          setSelectedSubtitleIndex(null);
                          setTracksMenuOpen(false);
                        }}
                        role="menuitemradio"
                        aria-checked={selectedSubtitleIndex === null}
                        type="button"
                      >
                        <span>Off</span>
                        {selectedSubtitleIndex === null ? (
                          <span className="tracks-menu-option-marker" aria-hidden="true">•</span>
                        ) : null}
                      </button>
                      {subtitleSources.map((track, index) => (
                        <button
                          className={`tracks-menu-option ${selectedSubtitleIndex === index ? "active" : ""}`}
                          key={track.key}
                          onClick={() => {
                            setSelectedSubtitleIndex(index);
                            setTracksMenuOpen(false);
                          }}
                          role="menuitemradio"
                          aria-checked={selectedSubtitleIndex === index}
                          type="button"
                        >
                          <span>{track.label}</span>
                          {selectedSubtitleIndex === index ? (
                            <span className="tracks-menu-option-marker" aria-hidden="true">•</span>
                          ) : null}
                        </button>
                      ))}
                    </div>
                  ) : null}

                  {audioTracks.length > 1 ? (
                    <>
                      {subtitleSources.length > 0 ? (
                        <div className="tracks-menu-divider" />
                      ) : null}
                      <div className="tracks-menu-section">
                        <h3>Audio</h3>
                        {audioTracks.map((track) => (
                          <button
                            className={`tracks-menu-option ${selectedAudioId === track.id ? "active" : ""}`}
                            key={track.id}
                            onClick={() => {
                              handleSelectAudioTrack(track.id);
                              setTracksMenuOpen(false);
                            }}
                            role="menuitemradio"
                            aria-checked={selectedAudioId === track.id}
                            type="button"
                          >
                            <span>{track.label}</span>
                            {selectedAudioId === track.id ? (
                              <span className="tracks-menu-option-marker" aria-hidden="true">•</span>
                            ) : null}
                          </button>
                        ))}
                      </div>
                    </>
                  ) : null}
                </div>
              ) : null}
            </div>
          ) : null}

          {roomId ? (
            <button
              className="ghost-button"
              onClick={() => setShowSessionPanel((visible) => !visible)}
              type="button"
            >
              {showSessionPanel ? "Hide session" : "Show session"}
            </button>
          ) : (
            <button
              className="primary-button"
              disabled={watchTogether.creating}
              onClick={() => void watchTogether.start()}
              type="button"
            >
              {watchTogether.creating ? "Starting…" : "Watch Together"}
            </button>
          )}
        </div>
      </header>

      {watchTogether.error ? (
        <p className="error player-error">{watchTogether.error}</p>
      ) : null}
      {playerError ? <p className="error player-error">{playerError}</p> : null}

      <div className={`player-layout ${roomId && showSessionPanel ? "with-session" : ""}`}>
        <div className="player-stage">
          <video
            autoPlay={!roomId}
            className="player-video"
            controls
            key={item.id}
            onError={() => setPlayerError("The browser could not load this file.")}
            playsInline
            preload="metadata"
            ref={videoRef}
          >
            {subtitleSources.map((track) => (
              <track
                key={track.key}
                kind="subtitles"
                label={track.label}
                src={track.src}
                srcLang={track.language}
              />
            ))}
          </video>
        </div>

        {roomId && showSessionPanel ? (
          <aside className="session-panel">
            <div className="session-head">
              <div>
                <p className="eyebrow">Watch Together</p>
                <h2>{socket.room?.name ?? "Session"}</h2>
              </div>
              <span className={`connection-pill ${socket.connectionState}`}>
                {labelForConnection(socket.connectionState)}
              </span>
            </div>

            <p className="session-presence">
              {socket.presenceCount === 0
                ? "No one else here yet."
                : `${socket.presenceCount} watcher${socket.presenceCount === 1 ? "" : "s"} connected`}
            </p>

            <label className="input-stack">
              <span className="label-text">Your name</span>
              <input
                onChange={(event) => onClientNameChange(event.target.value)}
                placeholder="Browser Viewer"
                value={clientName}
              />
            </label>

            <div className="session-cta-row">
              <button
                className="ghost-button"
                disabled={!live}
                onClick={handleSessionPlay}
                type="button"
              >
                Sync play
              </button>
              <button
                className="ghost-button"
                disabled={!live}
                onClick={handleSessionPause}
                type="button"
              >
                Sync pause
              </button>
              <button
                className="ghost-button"
                disabled={!live}
                onClick={handleSessionSeek}
                type="button"
              >
                Sync seek
              </button>
            </div>

            <button
              className="ghost-button wide"
              disabled={!live}
              onClick={handleCatchUp}
              type="button"
            >
              Catch up to room
            </button>

            <p className="muted session-activity">{socket.activity}</p>
            {socket.error ? <p className="error">{socket.error}</p> : null}

            <details className="session-debug">
              <summary>Diagnostics</summary>
              <dl>
                <div>
                  <dt>Room clock</dt>
                  <dd>
                    {formatTimeCode(
                      deriveExpectedPosition(
                        socket.room,
                        socket.authoritativeReceiptAtMs,
                        clockTickMs,
                      ),
                    )}
                  </dd>
                </div>
                <div>
                  <dt>Last drift</dt>
                  <dd>
                    {socket.lastCorrection
                      ? `${formatSignedDelta(socket.lastCorrection.deltaSeconds)}s`
                      : "n/a"}
                  </dd>
                </div>
                <div>
                  <dt>Correction</dt>
                  <dd>
                    {socket.lastCorrection
                      ? socket.lastCorrection.suggestedAction
                      : "—"}
                  </dd>
                </div>
              </dl>
            </details>

            <div className="session-footer">
              <button
                className="link-button"
                onClick={handleLeaveSession}
                type="button"
              >
                Leave session
              </button>
              {rooms.length > 0 ? (
                <p className="muted">{rooms.length} active session{rooms.length === 1 ? "" : "s"}</p>
              ) : null}
            </div>
          </aside>
        ) : null}
      </div>
    </div>
  );
}

function labelForConnection(state: string) {
  switch (state) {
    case "live":
      return "Live";
    case "connecting":
      return "Connecting";
    case "error":
      return "Error";
    default:
      return "Offline";
  }
}

function formatBrowserAudioTrackLabel(
  track: AudioTrackLike,
  item: MediaItem | null,
  index: number,
) {
  const indexedStream = item?.audioStreams[index];
  if (indexedStream) {
    return formatAudioStreamLabel(indexedStream, index + 1);
  }

  const label = track.label?.trim();
  const language = formatLanguageName(track.language);
  return label || language || `Track ${index + 1}`;
}

function preparedSubtitleLabel(item: MediaItem) {
  const selection = item.streamCopy?.subtitle;
  if (!selection) return "Prepared subtitles";

  if (selection.kind === "sidecar") {
    const track = item.subtitleTracks[selection.index];
    return track
      ? `${formatSubtitleTrackLabel(track, selection.index + 1)} · prepared`
      : "Prepared subtitles";
  }

  const streamIndex = item.subtitleStreams.findIndex(
    (stream) => stream.index === selection.index,
  );
  const stream = streamIndex >= 0 ? item.subtitleStreams[streamIndex] : null;
  return stream
    ? `${formatSubtitleStreamLabel(stream, streamIndex + 1)} · prepared`
    : "Prepared subtitles";
}

function preparedSubtitleLanguage(item: MediaItem) {
  const selection = item.streamCopy?.subtitle;
  if (!selection) return undefined;

  if (selection.kind === "sidecar") {
    return item.subtitleTracks[selection.index]?.language ?? undefined;
  }

  return (
    item.subtitleStreams.find((stream) => stream.index === selection.index)?.language ??
    undefined
  );
}
