import { useEffect, useState, type RefObject } from "react";

import { streamUrl } from "./api";
import type { MediaItem } from "./types";

export function usePlayerSource(
  videoRef: RefObject<HTMLVideoElement | null>,
  item: MediaItem | null,
): { fatalError: string | null } {
  const [fatalError, setFatalError] = useState<string | null>(null);

  const itemId = item?.id ?? null;
  const preparationState = item?.preparationState ?? null;
  const streamCopyError = item?.streamCopy?.error ?? null;

  useEffect(() => {
    setFatalError(null);
    const video = videoRef.current;
    if (!video || !itemId || !preparationState) return;

    switch (preparationState) {
      case "unsupported":
        setFatalError("This media is not supported for browser playback.");
        return;
      case "needsPreparation":
        setFatalError("This media needs a stream copy before it can be played.");
        return;
      case "preparing":
        setFatalError("A stream copy is still being prepared for this media.");
        return;
      case "failed":
        setFatalError(
          streamCopyError ??
            "The last stream copy attempt failed. Create a new stream copy to try again.",
        );
        return;
    }

    video.src = streamUrl(itemId);

    return () => {
      video.removeAttribute("src");
      try {
        video.load();
      } catch {
        // Ignored — some browsers throw when load() runs after src is cleared.
      }
    };
  }, [videoRef, itemId, preparationState, streamCopyError]);

  return { fatalError };
}
