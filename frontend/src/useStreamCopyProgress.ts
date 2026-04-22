import { useCallback, useEffect, useRef, useState } from "react";

import { api } from "./api";
import { isActiveStreamCopy } from "./streamCopyProgress";
import type { StreamCopySummary } from "./types";

const POLL_INTERVAL_MS = 750;

type Options = {
  mediaId: string | null;
  fallback: StreamCopySummary | null;
  onRefresh: () => void;
};

export type UseStreamCopyProgress = {
  summary: StreamCopySummary | null;
  seedFromCreate: (summary: StreamCopySummary) => void;
};

export function useStreamCopyProgress({
  mediaId,
  fallback,
  onRefresh,
}: Options): UseStreamCopyProgress {
  const [liveSummary, setLiveSummary] = useState<StreamCopySummary | null>(null);
  const onRefreshRef = useRef(onRefresh);
  onRefreshRef.current = onRefresh;

  useEffect(() => {
    setLiveSummary(null);
  }, [mediaId]);

  const summary = liveSummary ?? fallback;
  const shouldPoll = Boolean(mediaId) && isActiveStreamCopy(summary);

  useEffect(() => {
    if (!shouldPoll || !mediaId) return;

    let cancelled = false;
    let timerId: number | null = null;
    let refreshNotified = false;

    const notifyRefreshOnce = () => {
      if (refreshNotified) return;
      refreshNotified = true;
      onRefreshRef.current();
    };

    const tick = async () => {
      try {
        const response = await api.getStreamCopy(mediaId);
        if (cancelled) return;
        if (response === null) {
          setLiveSummary(null);
          notifyRefreshOnce();
          return;
        }
        setLiveSummary(response);
        if (response.status === "ready" || response.status === "failed") {
          notifyRefreshOnce();
          return;
        }
      } catch {
        // Transient error — try again on the next tick.
      }
      if (cancelled) return;
      timerId = window.setTimeout(tick, POLL_INTERVAL_MS);
    };

    timerId = window.setTimeout(tick, POLL_INTERVAL_MS);

    return () => {
      cancelled = true;
      if (timerId !== null) window.clearTimeout(timerId);
    };
  }, [shouldPoll, mediaId]);

  const seedFromCreate = useCallback((seed: StreamCopySummary) => {
    setLiveSummary(seed);
  }, []);

  return { summary, seedFromCreate };
}
