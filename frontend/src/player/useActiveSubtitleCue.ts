import { useEffect, useMemo, useRef, useState } from "react";

import type { SubtitleSource } from "./subtitleSources";
import { parseWebVtt, type WebVttCue } from "./webvtt";

type UseActiveSubtitleCue = {
  activeCues: string[];
  error: string | null;
};

export function useActiveSubtitleCue(
  selectedSource: SubtitleSource | null,
  currentTime: number,
): UseActiveSubtitleCue {
  const cacheRef = useRef(new Map<string, WebVttCue[]>());
  const [cues, setCues] = useState<WebVttCue[]>([]);
  const [error, setError] = useState<string | null>(null);
  const selectedKey = selectedSource?.key ?? null;
  const selectedSrc = selectedSource?.src ?? null;

  useEffect(() => {
    if (!selectedKey || !selectedSrc) {
      setCues([]);
      setError(null);
      return;
    }

    const cached = cacheRef.current.get(selectedKey);
    if (cached) {
      setCues(cached);
      setError(null);
      return;
    }

    let cancelled = false;
    setCues([]);
    setError(null);

    void (async () => {
      try {
        const response = await fetch(selectedSrc);
        if (!response.ok) {
          throw new Error(`Failed to load subtitle track (${response.status}).`);
        }

        const parsed = parseWebVtt(await response.text());
        if (cancelled) return;

        cacheRef.current.set(selectedKey, parsed);
        setCues(parsed);
      } catch (loadError) {
        if (cancelled) return;
        setCues([]);
        setError(
          loadError instanceof Error
            ? loadError.message
            : "Could not load the selected subtitle track.",
        );
      }
    })();

    return () => {
      cancelled = true;
    };
  }, [selectedKey, selectedSrc]);

  const activeCues = useMemo(() => {
    if (!selectedKey) return [];

    const anchorIndex = findLastCueStartingBeforeOrAt(cues, currentTime);
    if (anchorIndex === -1) return [];

    let firstVisibleIndex = anchorIndex;
    while (
      firstVisibleIndex > 0 &&
      cues[firstVisibleIndex - 1] &&
      currentTime < cues[firstVisibleIndex - 1].end
    ) {
      firstVisibleIndex -= 1;
    }

    const visible: string[] = [];
    for (let index = firstVisibleIndex; index < cues.length; index += 1) {
      const cue = cues[index];
      if (!cue || cue.start > currentTime) break;
      if (currentTime >= cue.end) continue;
      const text = cue.text.trim();
      if (text.length > 0) visible.push(text);
    }

    return Array.from(new Set(visible));
  }, [cues, currentTime, selectedKey]);

  return { activeCues, error };
}

function findLastCueStartingBeforeOrAt(cues: WebVttCue[], currentTime: number): number {
  let low = 0;
  let high = cues.length - 1;
  let result = -1;

  while (low <= high) {
    const mid = Math.floor((low + high) / 2);
    const cue = cues[mid];
    if (!cue) break;
    if (cue.start <= currentTime) {
      result = mid;
      low = mid + 1;
    } else {
      high = mid - 1;
    }
  }

  return result;
}
