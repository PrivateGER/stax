import { useEffect, useMemo, useRef, useState } from "react";

import shimakazeUrl from "../../asset/shimakaze_sfw.png";
import { api, thumbnailUrl } from "../api";
import {
  displayMediaTitle,
  formatAudioStreamLabel,
  formatBytes,
  formatDuration,
  formatResolution,
  formatSubtitleStreamLabel,
  formatSubtitleTrackLabel,
  posterInitials,
  rootFolderName,
} from "../format";
import { navigate } from "../router";
import { StreamCopyProgress, isActiveStreamCopy } from "../streamCopyProgress";
import type {
  CreateStreamCopyRequest,
  MediaItem,
  Room,
  StreamCopySubtitleSelection,
  StreamCopySummary,
  SubtitleMode,
  SubtitleSourceKind,
} from "../types";
import { useStreamCopyProgress } from "../useStreamCopyProgress";
import { useWatchTogether } from "../useWatchTogether";

type Props = {
  item: MediaItem | null;
  rooms: Room[];
  onRoomCreated: (room: Room) => void;
  onRefresh: () => void;
};

type SubtitleOption = {
  value: string;
  kind: SubtitleSourceKind;
  index: number;
  label: string;
};

export function TitlePage({ item, rooms, onRoomCreated, onRefresh }: Props) {
  const watchTogether = useWatchTogether(item, onRoomCreated);

  const { summary: liveStreamCopy, seedFromCreate: seedLiveStreamCopy } =
    useStreamCopyProgress({
      mediaId: item?.id ?? null,
      fallback: item?.streamCopy ?? null,
      onRefresh,
    });

  const subtitleOptions = useMemo<SubtitleOption[]>(() => {
    if (!item) return [];
    const sidecarOptions = item.subtitleTracks.map((track, index) => ({
      value: `sidecar:${index}`,
      kind: "sidecar" as const,
      index,
      label: formatSubtitleTrackLabel(track, index + 1, true),
    }));
    const embeddedOptions = item.subtitleStreams.map((stream, index) => ({
      value: `embedded:${stream.index}`,
      kind: "embedded" as const,
      index: stream.index,
      label: formatSubtitleStreamLabel(stream, index + 1, true),
    }));

    return [...sidecarOptions, ...embeddedOptions];
  }, [item?.subtitleStreams, item?.subtitleTracks]);

  const [creatingStreamCopy, setCreatingStreamCopy] = useState(false);
  const [streamCopyError, setStreamCopyError] = useState<string | null>(null);
  const [subtitleMode, setSubtitleMode] = useState<SubtitleMode>("off");
  const [subtitleSelection, setSubtitleSelection] = useState<string>(
    () => subtitleOptions[0]?.value ?? "",
  );

  // Reset the form when the underlying title changes. Match on `id`: library
  // polls hand us fresh object references even when content is identical, and
  // clobbering in-flight selections every poll would wipe the form the user
  // is still filling in.
  const prevItemIdRef = useRef<string | null>(item?.id ?? null);
  useEffect(() => {
    const currentItemId = item?.id ?? null;
    if (currentItemId === prevItemIdRef.current) return;

    prevItemIdRef.current = currentItemId;
    setSubtitleMode("off");
    setSubtitleSelection(subtitleOptions[0]?.value ?? "");
    setStreamCopyError(null);
  }, [item?.id, subtitleOptions]);

  if (!item) {
    return (
      <section className="title-missing">
        <img alt="" aria-hidden="true" className="title-missing-mascot" src={shimakazeUrl} />
        <h1>Title not found</h1>
        <p className="muted">This media is not in the library anymore.</p>
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
  const relatedRooms = rooms
    .filter((room) => room.mediaId === item.id)
    .slice()
    .sort((left, right) => right.createdAt.localeCompare(left.createdAt));
  const joinableRoom = relatedRooms[0] ?? null;
  const canPlay = item.preparationState === "direct" || item.preparationState === "prepared";
  const subtitleSourceCount = item.subtitleTracks.length + item.subtitleStreams.length;

  async function handleCreateStreamCopy() {
    if (!item) return;

    const request: CreateStreamCopyRequest = {
      audioStreamIndex: null,
      subtitleMode,
      subtitle:
        subtitleMode === "off"
          ? null
          : parseSubtitleSelection(subtitleSelection),
    };

    try {
      setCreatingStreamCopy(true);
      setStreamCopyError(null);
      const response = await api.createStreamCopy(item.id, request);
      seedLiveStreamCopy(response);
      onRefresh();
    } catch (creationError) {
      setStreamCopyError(
        creationError instanceof Error
          ? creationError.message
          : "Could not create the stream copy.",
      );
    } finally {
      setCreatingStreamCopy(false);
    }
  }

  const metaRows: Array<[string, string]> = [];
  if (item.durationSeconds !== null) {
    metaRows.push(["Runtime", formatDuration(item.durationSeconds)]);
  }
  const resolution = formatResolution(item.width, item.height);
  if (resolution) metaRows.push(["Resolution", resolution]);
  if (item.videoCodec) metaRows.push(["Video", item.videoCodec.toUpperCase()]);
  if (item.audioCodec) metaRows.push(["Audio", item.audioCodec.toUpperCase()]);
  if (item.containerName) metaRows.push(["Container", item.containerName]);
  if (item.audioStreams.length > 0) {
    metaRows.push([
      "Audio tracks",
      `${item.audioStreams.length} track${item.audioStreams.length === 1 ? "" : "s"}`,
    ]);
  }
  if (subtitleSourceCount > 0) {
    metaRows.push([
      "Subtitles",
      `${subtitleSourceCount} source${subtitleSourceCount === 1 ? "" : "s"}`,
    ]);
  }
  metaRows.push(["File size", formatBytes(item.sizeBytes)]);
  metaRows.push(["Library", rootFolderName(item.rootPath)]);
  metaRows.push(["Playback", playbackLabel(item.preparationState)]);

  return (
    <article className="title-page">
      <div className="title-hero">
        <div className="title-hero-art">
          {item.thumbnailGeneratedAt ? (
            <img alt="" className="title-hero-image" src={thumbnailUrl(item.id)} />
          ) : (
            <span className="poster-initials large">{posterInitials(title)}</span>
          )}
        </div>

        <div className="title-hero-copy">
          <p className="eyebrow">
            {item.durationSeconds !== null ? formatDuration(item.durationSeconds) : "Movie"}
            {resolution ? ` · ${resolution}` : ""}
            {subtitleSourceCount > 0
              ? ` · ${subtitleSourceCount} subtitle source${subtitleSourceCount === 1 ? "" : "s"}`
              : ""}
          </p>
          <h1>{title}</h1>
          <p className="muted title-path">{item.relativePath}</p>

          <div className="title-cta">
            <button
              className="primary-button"
              disabled={!canPlay}
              onClick={() => navigate({ name: "watch", mediaId: item.id, roomId: null })}
              type="button"
            >
              {canPlay ? "▶ Play" : "Needs stream copy"}
            </button>
            {joinableRoom ? (
              <button
                className="ghost-button"
                onClick={() =>
                  navigate({ name: "watch", mediaId: item.id, roomId: joinableRoom.id })
                }
                type="button"
              >
                Join session
              </button>
            ) : (
              <button
                className="ghost-button"
                disabled={watchTogether.creating}
                onClick={() => void watchTogether.start()}
                type="button"
              >
                {watchTogether.creating ? "Starting…" : "Watch Together"}
              </button>
            )}
            {joinableRoom ? (
              <button
                className="link-button"
                disabled={watchTogether.creating}
                onClick={() => void watchTogether.start()}
                type="button"
              >
                {watchTogether.creating ? "Starting…" : "Start a new session"}
              </button>
            ) : null}
          </div>

          {watchTogether.error ? <p className="error">{watchTogether.error}</p> : null}

          {item.playbackMode === "needsPreparation" ? (
            <div className="title-stream-copy">
              <h2>Stream copy</h2>
              <p className="muted">
                {streamCopyDescription(
                  liveStreamCopy,
                  item.preparationState,
                )}
              </p>

              <StreamCopyProgress summary={liveStreamCopy} />

              <label className="input-stack">
                <span className="label-text">Subtitles</span>
                <select
                  onChange={(event) => setSubtitleMode(event.target.value as SubtitleMode)}
                  value={subtitleMode}
                >
                  <option value="off">Off</option>
                  <option value="sidecar">Sidecar WebVTT</option>
                  <option value="burned">Burn into video</option>
                </select>
              </label>

              {subtitleMode !== "off" ? (
                <label className="input-stack">
                  <span className="label-text">Subtitle source</span>
                  <select
                    disabled={subtitleOptions.length === 0}
                    onChange={(event) => setSubtitleSelection(event.target.value)}
                    value={subtitleSelection}
                  >
                    {subtitleOptions.length === 0 ? (
                      <option value="">No subtitles available</option>
                    ) : (
                      subtitleOptions.map((option) => (
                        <option key={option.value} value={option.value}>
                          {option.label}
                        </option>
                      ))
                    )}
                  </select>
                </label>
              ) : null}

              <div className="title-cta">
                <button
                  className="ghost-button"
                  disabled={
                    creatingStreamCopy ||
                    isActiveStreamCopy(liveStreamCopy) ||
                    (subtitleMode !== "off" && subtitleOptions.length === 0)
                  }
                  onClick={() => void handleCreateStreamCopy()}
                  type="button"
                >
                  {streamCopyButtonLabel({
                    submitting: creatingStreamCopy,
                    summary: liveStreamCopy,
                    preparationState: item.preparationState,
                  })}
                </button>
              </div>

              {streamCopyError ? <p className="error">{streamCopyError}</p> : null}
            </div>
          ) : null}
        </div>
      </div>

      <section className="title-details">
        <div>
          <h2>Details</h2>
          <dl className="detail-grid">
            {metaRows.map(([label, value]) => (
              <div className="detail-row" key={label}>
                <dt>{label}</dt>
                <dd>{value}</dd>
              </div>
            ))}
          </dl>
        </div>

        <div>
          <h2>Subtitles</h2>
          {subtitleSourceCount === 0 ? (
            <p className="muted">No subtitle tracks indexed.</p>
          ) : (
            <ul className="subtitle-list">
              {item.subtitleTracks.map((track, index) => (
                <li key={track.relativePath}>
                  {formatSubtitleTrackLabel(track, index + 1, true)}
                </li>
              ))}
              {item.subtitleStreams.map((stream, index) => (
                <li key={`embedded-${stream.index}`}>
                  {formatSubtitleStreamLabel(stream, index + 1, true)}
                </li>
              ))}
            </ul>
          )}
        </div>

        <div>
          <h2>Audio</h2>
          {item.audioStreams.length === 0 ? (
            <p className="muted">No audio streams indexed.</p>
          ) : (
            <ul className="subtitle-list">
              {item.audioStreams.map((stream, index) => (
                <li key={`audio-${stream.index}`}>
                  {formatAudioStreamLabel(stream, index + 1)}
                </li>
              ))}
            </ul>
          )}
        </div>

        {relatedRooms.length > 0 ? (
          <div>
            <h2>Active sessions</h2>
            <ul className="related-rooms">
              {relatedRooms.map((room) => (
                <li key={room.id}>
                  <button
                    className="room-link"
                    onClick={() =>
                      navigate({ name: "watch", mediaId: item.id, roomId: room.id })
                    }
                    type="button"
                  >
                    <strong>{room.name}</strong>
                    <span className="muted">
                      {room.playbackState.status === "playing" ? "Playing" : "Paused"}
                    </span>
                  </button>
                </li>
              ))}
            </ul>
          </div>
        ) : null}
      </section>
    </article>
  );
}

function parseSubtitleSelection(value: string): StreamCopySubtitleSelection | null {
  const [kind, rawIndex] = value.split(":");
  if (!kind || rawIndex === undefined) return null;
  const index = Number(rawIndex);
  if (!Number.isFinite(index)) return null;
  if (kind !== "sidecar" && kind !== "embedded") return null;

  return {
    kind,
    index,
  };
}

function playbackLabel(state: MediaItem["preparationState"]) {
  switch (state) {
    case "direct":
      return "Direct stream";
    case "prepared":
      return "Prepared stream copy";
    case "preparing":
      return "Preparing stream copy";
    case "failed":
      return "Stream copy failed";
    case "needsPreparation":
      return "Needs stream copy";
    default:
      return "Unsupported";
  }
}

function streamCopyDescription(
  summary: StreamCopySummary | null,
  preparationState: MediaItem["preparationState"],
) {
  if (summary) {
    switch (summary.status) {
      case "queued":
        return "A stream copy is queued and will start shortly.";
      case "running":
        return "A stream copy is being prepared in the background.";
      case "ready":
        return "A prepared stream copy is ready and will be used for browser playback.";
      case "failed":
        return summary.error ?? "The last stream copy attempt failed.";
    }
  }

  switch (preparationState) {
    case "prepared":
      return "A prepared stream copy is ready and will be used for browser playback.";
    case "preparing":
      return "A stream copy is being prepared in the background.";
    case "failed":
      return "The last stream copy attempt failed.";
    case "needsPreparation":
      return "This file is not browser-safe as-is. Create a stream copy to play it.";
    default:
      return "This title can already be streamed directly.";
  }
}

function streamCopyButtonLabel(args: {
  submitting: boolean;
  summary: StreamCopySummary | null;
  preparationState: MediaItem["preparationState"];
}) {
  if (args.submitting) return "Submitting…";
  if (args.summary?.status === "queued") return "Queued…";
  if (args.summary?.status === "running") return "Preparing…";
  if (args.summary?.status === "ready") return "Recreate stream copy";
  if (args.summary?.status === "failed") return "Retry stream copy";
  if (args.preparationState === "prepared") return "Recreate stream copy";
  if (args.preparationState === "failed") return "Retry stream copy";
  return "Create stream copy";
}
