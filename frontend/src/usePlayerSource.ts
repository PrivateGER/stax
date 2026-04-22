import { useEffect, useState, type RefObject } from "react";

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

  const itemId = item?.id ?? null;
  const preparationState = item?.preparationState ?? null;
  const streamCopyError = item?.streamCopy?.error ?? null;

  useEffect(() => {
    setFatalError(null);
    const video = videoRef.current;
    if (!video || !itemId || !preparationState) return;

    if (preparationState === "unsupported") {
      setFatalError({
        message: "This media is not supported for browser playback.",
      });
      return;
    }
    if (preparationState === "needsPreparation") {
      setFatalError({
        message: "This media needs a stream copy before it can be played.",
      });
      return;
    }
    if (preparationState === "preparing") {
      setFatalError({
        message: "A stream copy is still being prepared for this media.",
      });
      return;
    }
    if (preparationState === "failed") {
      setFatalError({
        message:
          streamCopyError ??
          "The last stream copy attempt failed. Create a new stream copy to try again.",
      });
      return;
    }

    video.src = streamUrl(itemId);

    return () => {
      video.removeAttribute("src");
      try {
        video.load();
      } catch {
        // ignore
      }
    };
  }, [videoRef, itemId, preparationState, streamCopyError]);

  return { fatalError };
}
