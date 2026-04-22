import { useEffect, type RefObject } from "react";

import { streamUrl } from "./api";
import type { MediaItem } from "./types";

export function usePlayerSource(
  videoRef: RefObject<HTMLVideoElement | null>,
  item: MediaItem | null,
): void {
  const itemId = item?.id ?? null;
  const preparationState = item?.preparationState ?? null;

  useEffect(() => {
    const video = videoRef.current;
    if (!video || !itemId) return;
    if (preparationState !== "direct" && preparationState !== "prepared") return;

    video.src = streamUrl(itemId);

    return () => {
      video.removeAttribute("src");
      try {
        video.load();
      } catch {
        // Ignored — some browsers throw when load() runs after src is cleared.
      }
    };
  }, [videoRef, itemId, preparationState]);
}
