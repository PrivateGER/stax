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

    const visible = cues
      .filter((cue) => cue.start <= currentTime && currentTime < cue.end)
      .map((cue) => cue.text.trim())
      .filter((cue) => cue.length > 0);

    return Array.from(new Set(visible));
  }, [cues, currentTime, selectedKey]);

  return { activeCues, error };
}
