import { embeddedSubtitleUrl, subtitleUrl } from "../api";
import { formatSubtitleStreamLabel, formatSubtitleTrackLabel } from "../format";
import type { MediaItem } from "../types";

export type SubtitleSource = {
  key: string;
  label: string;
  src: string;
  language?: string;
};

export function deriveSubtitleSources(item: MediaItem): SubtitleSource[] {
  if (item.preparationState === "prepared" && item.streamCopy?.subtitleUrl) {
    return [
      {
        key: `${item.id}-prepared`,
        label: preparedLabel(item),
        src: item.streamCopy.subtitleUrl,
        language: preparedLanguage(item),
      },
    ];
  }

  const sidecarSources = item.subtitleTracks.map((track, index) => ({
    key: `${item.id}-${track.relativePath}`,
    label: formatSubtitleTrackLabel(track, index + 1),
    src: subtitleUrl(item.id, index),
    language: track.language ?? undefined,
  }));
  const embeddedSources = item.subtitleStreams
    .filter((stream) => isTextSubtitleCodec(stream.codec))
    .map((stream, index) => ({
      key: `${item.id}-embedded-${stream.index}`,
      label: formatSubtitleStreamLabel(stream, index + 1),
      src: embeddedSubtitleUrl(item.id, stream.index),
      language: stream.language ?? undefined,
    }));

  return [...sidecarSources, ...embeddedSources];
}

function preparedLabel(item: MediaItem) {
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

function preparedLanguage(item: MediaItem) {
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

function isTextSubtitleCodec(codec: string | null) {
  switch (codec?.trim().toLowerCase()) {
    case "ass":
    case "mov_text":
    case "srt":
    case "ssa":
    case "subrip":
    case "text":
    case "webvtt":
      return true;
    default:
      return false;
  }
}
