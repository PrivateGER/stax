import type { AudioStream, MediaItem, SubtitleStream, SubtitleTrack } from "./types";

let languageDisplayNames: Intl.DisplayNames | null | undefined;

export function displayMediaTitle(item: MediaItem) {
  const baseName = item.fileName.replace(/\.[^.]+$/, "");
  return baseName.replaceAll(/[._]+/g, " ").trim() || item.fileName;
}

export function formatDuration(durationSeconds: number) {
  const totalSeconds = Math.max(0, Math.round(durationSeconds));
  const hours = Math.floor(totalSeconds / 3600);
  const minutes = Math.floor((totalSeconds % 3600) / 60);

  if (hours > 0) {
    return `${hours}h ${minutes}m`;
  }

  if (minutes > 0) {
    return `${minutes}m`;
  }

  return `${totalSeconds}s`;
}

export function formatTimeCode(seconds: number) {
  const total = Math.max(0, Math.round(seconds));
  const hours = Math.floor(total / 3600);
  const minutes = Math.floor((total % 3600) / 60);
  const secs = total % 60;
  const minutePart = String(minutes).padStart(hours > 0 ? 2 : 1, "0");
  const secondPart = String(secs).padStart(2, "0");

  return hours > 0 ? `${hours}:${minutePart}:${secondPart}` : `${minutePart}:${secondPart}`;
}

export function formatBytes(bytes: number) {
  if (bytes < 1024) return `${bytes} B`;
  if (bytes < 1024 ** 2) return `${(bytes / 1024).toFixed(1)} KB`;
  if (bytes < 1024 ** 3) return `${(bytes / 1024 ** 2).toFixed(1)} MB`;
  return `${(bytes / 1024 ** 3).toFixed(1)} GB`;
}

export function formatResolution(width: number | null, height: number | null) {
  if (width === null || height === null) return null;
  if (height >= 2160) return "4K";
  if (height >= 1440) return "1440p";
  if (height >= 1080) return "1080p";
  if (height >= 720) return "720p";
  if (height >= 480) return "480p";
  return `${width}×${height}`;
}

export function mediaAspectRatio(width: number | null, height: number | null) {
  if (width === null || height === null || width <= 0 || height <= 0) return null;
  return `${width} / ${height}`;
}

export function rootFolderName(rootPath: string) {
  const normalized = rootPath.replaceAll("\\", "/");
  const parts = normalized.split("/").filter(Boolean);
  return parts[parts.length - 1] ?? rootPath;
}

export function mediaBadges(item: MediaItem): string[] {
  const badges = [
    formatResolution(item.width, item.height),
    item.videoCodec ? item.videoCodec.toUpperCase() : null,
    item.subtitleTracks.length > 0
      ? `CC · ${item.subtitleTracks.length}`
      : null,
  ];

  return badges.filter((value): value is string => Boolean(value));
}

export function posterInitials(title: string) {
  const words = title.split(/\s+/).filter(Boolean);

  if (words.length === 0) {
    return "·";
  }

  if (words.length === 1) {
    return words[0]!.slice(0, 2).toUpperCase();
  }

  return `${words[0]![0] ?? ""}${words[1]![0] ?? ""}`.toUpperCase();
}

export function formatLanguageName(language: string | null | undefined) {
  const value = language?.trim();
  if (!value) return null;

  const normalized = value.replaceAll("_", "-");
  if (languageDisplayNames === undefined) {
    languageDisplayNames =
      typeof Intl !== "undefined" && "DisplayNames" in Intl
        ? new Intl.DisplayNames(["en"], { type: "language" })
        : null;
  }

  try {
    const display = languageDisplayNames?.of(normalized);
    if (display) return capitalize(display);
  } catch {
    // Fall back to the raw tag below when the language code is malformed.
  }

  const [base] = normalized.split("-");
  if (!base) return normalized;
  return base.length <= 3 ? base.toUpperCase() : capitalize(base);
}

export function formatAudioStreamLabel(stream: AudioStream, position?: number) {
  const language = formatLanguageName(stream.language);
  const label = firstNonEmpty(
    stream.title,
    language,
    `Track ${position ?? stream.index + 1}`,
  );
  const metadata = [
    detailUnlessDuplicate(language, label),
    formatCodec(stream.codec),
    formatChannelLayout(stream.channels, stream.channelLayout),
    stream.default ? "default" : null,
  ];

  return joinTrackLabel(label, metadata);
}

export function formatSubtitleTrackLabel(
  track: SubtitleTrack,
  position?: number,
  includeKind = false,
) {
  const language = formatLanguageName(track.language);
  const label = firstNonEmpty(track.label, language, `Subtitle ${position ?? 1}`);
  const metadata = [
    detailUnlessDuplicate(language, label),
    formatCodec(track.extension),
    includeKind ? "sidecar" : null,
  ];

  return joinTrackLabel(label, metadata);
}

export function formatSubtitleStreamLabel(
  stream: SubtitleStream,
  position?: number,
  includeKind = false,
) {
  const language = formatLanguageName(stream.language);
  const label = firstNonEmpty(
    stream.title,
    language,
    `Subtitle ${position ?? stream.index + 1}`,
  );
  const metadata = [
    detailUnlessDuplicate(language, label),
    formatCodec(stream.codec),
    includeKind ? "embedded" : null,
    stream.default ? "default" : null,
    stream.forced ? "forced" : null,
  ];

  return joinTrackLabel(label, metadata);
}

function firstNonEmpty(...values: Array<string | null | undefined>) {
  return values.find((value) => value && value.trim().length > 0)?.trim() ?? "Unknown";
}

function joinTrackLabel(label: string, metadata: Array<string | null>) {
  const details = metadata.filter((value): value is string => Boolean(value));
  if (details.length === 0) return label;
  return `${label} · ${details.join(" · ")}`;
}

function detailUnlessDuplicate(detail: string | null, label: string) {
  if (!detail) return null;
  return detail.localeCompare(label, undefined, { sensitivity: "accent" }) === 0
    ? null
    : detail;
}

function formatCodec(value: string | null | undefined) {
  const codec = value?.trim();
  if (!codec) return null;

  const compact = codec.replaceAll("_", " ").replaceAll("-", " ");
  return compact
    .split(/\s+/)
    .filter(Boolean)
    .map((part) => part.toUpperCase())
    .join(" ");
}

function formatChannelLayout(
  channels: number | null | undefined,
  layout: string | null | undefined,
) {
  const normalized = layout?.trim().replace(/\(.+\)/, "");
  if (normalized) {
    const lower = normalized.toLowerCase();
    if (lower === "mono") return "Mono";
    if (lower === "stereo") return "Stereo";
    return normalized;
  }

  if (channels === null || channels === undefined) return null;
  if (channels === 1) return "Mono";
  if (channels === 2) return "Stereo";
  return `${channels}ch`;
}

function capitalize(value: string) {
  if (!value) return value;
  return `${value[0]!.toUpperCase()}${value.slice(1)}`;
}
