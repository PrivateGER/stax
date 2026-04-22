import { useEffect, useState, type RefObject } from "react";
import Hls from "hls.js";

import { streamUrl } from "./api";
import type { MediaItem } from "./types";

type FatalError = {
  message: string;
};

export function usePlayerSource(
  videoRef: RefObject<HTMLVideoElement | null>,
  item: MediaItem | null,
): { fatalError: FatalError | null } {
  const [fatalError, setFatalError] = useState<FatalError | null>(null);

  useEffect(() => {
    setFatalError(null);
    const video = videoRef.current;
    if (!video || !item) return;

    let cancelled = false;
    let hls: Hls | null = null;

    const attach = async () => {
      if (item.playbackMode === "direct" || item.playbackMode === "unsupported") {
        // Direct path (also fall back to direct for unsupported as a last resort —
        // the user might still get a useful error message from the <video> element).
        video.src = streamUrl(item.id);
        return;
      }

      const masterUrl = item.hlsMasterUrl ?? `/api/media/${item.id}/hls/master.m3u8`;

      // Native HLS (Safari, iOS, some smart TVs).
      if (video.canPlayType("application/vnd.apple.mpegurl")) {
        video.src = masterUrl;
        return;
      }

      if (!Hls.isSupported()) {
        setFatalError({
          message:
            "This file requires server-side transcoding, but your browser does not support HLS.",
        });
        return;
      }

      // hls.js defaults give up after ~15s of manifest retries. For the
      // transcode tier the first segment can take noticeably longer than that
      // on modest hardware (e.g. 10-bit HEVC → h264_vaapi), during which the
      // backend returns 503 so hls.js can back off and try again. Widen the
      // retry window so we keep polling until the backend's readiness watcher
      // flips `is_ready`, rather than surfacing a fatal error the user has to
      // manually retry through.
      hls = new Hls({
        manifestLoadingMaxRetry: 8,
        manifestLoadingRetryDelay: 1000,
        manifestLoadingMaxRetryTimeout: 8000,
        levelLoadingMaxRetry: 8,
        levelLoadingRetryDelay: 1000,
        levelLoadingMaxRetryTimeout: 8000,
      });
      hls.on(Hls.Events.ERROR, (_event, data) => {
        if (!data.fatal) return;
        if (data.response?.code === 415) {
          setFatalError({
            message: "This media is not supported for browser playback.",
          });
        } else {
          setFatalError({
            message: `HLS playback failed: ${data.details ?? "unknown error"}.`,
          });
        }
      });
      hls.loadSource(masterUrl);
      hls.attachMedia(video);

      if (cancelled && hls) {
        hls.destroy();
        hls = null;
      }
    };

    attach().catch((error) => {
      setFatalError({ message: String(error) });
    });

    return () => {
      cancelled = true;
      if (hls) {
        hls.destroy();
        hls = null;
      }
      // Clear the src so a stale stream isn't held open.
      video.removeAttribute("src");
      try {
        video.load();
      } catch {
        // ignore
      }
    };
  }, [videoRef, item]);

  return { fatalError };
}
