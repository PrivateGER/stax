import type { MediaItem } from "./types";

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

export function formatSignedDelta(value: number) {
  const prefix = value >= 0 ? "+" : "";
  return `${prefix}${value.toFixed(2)}`;
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
