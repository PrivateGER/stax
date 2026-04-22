import { useCallback, useEffect, useState, type RefObject } from "react";

import { formatAudioStreamLabel, formatLanguageName } from "../format";
import type { MediaItem } from "../types";

// Minimal shape for HTMLMediaElement.audioTracks — present in Safari and
// behind a flag in Chromium, not modelled by lib.dom.
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

export type AudioTrackEntry = { id: string; label: string };

export type UseAudioTracks = {
  tracks: AudioTrackEntry[];
  selectedId: string | null;
  select: (trackId: string) => void;
};

function getList(video: HTMLVideoElement): AudioTrackListLike | undefined {
  return (video as HTMLVideoElement & { audioTracks?: AudioTrackListLike })
    .audioTracks;
}

export function useAudioTracks(
  videoRef: RefObject<HTMLVideoElement | null>,
  item: MediaItem | null,
): UseAudioTracks {
  const [tracks, setTracks] = useState<AudioTrackEntry[]>([]);
  const [selectedId, setSelectedId] = useState<string | null>(null);

  useEffect(() => {
    setTracks([]);
    setSelectedId(null);
  }, [item?.id]);

  useEffect(() => {
    const video = videoRef.current;
    if (!video) return;

    const list = getList(video);
    if (!list) return;

    const rebuild = () => {
      const entries: AudioTrackEntry[] = [];
      let activeId: string | null = null;
      for (let index = 0; index < list.length; index += 1) {
        const track = list[index];
        if (!track) continue;
        const id = track.id || String(index);
        entries.push({ id, label: labelFor(track, item, index) });
        if (track.enabled) activeId = id;
      }
      setTracks(entries);
      setSelectedId(activeId);
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
  }, [videoRef, item?.id, item?.audioStreams]);

  const select = useCallback(
    (trackId: string) => {
      const video = videoRef.current;
      if (!video) return;
      const list = getList(video);
      if (!list) return;

      for (let index = 0; index < list.length; index += 1) {
        const track = list[index];
        if (!track) continue;
        const id = track.id || String(index);
        track.enabled = id === trackId;
      }
      setSelectedId(trackId);
    },
    [videoRef],
  );

  return { tracks, selectedId, select };
}

function labelFor(track: AudioTrackLike, item: MediaItem | null, index: number) {
  const indexedStream = item?.audioStreams[index];
  if (indexedStream) {
    return formatAudioStreamLabel(indexedStream, index + 1);
  }

  const label = track.label?.trim();
  const language = formatLanguageName(track.language);
  return label || language || `Track ${index + 1}`;
}
