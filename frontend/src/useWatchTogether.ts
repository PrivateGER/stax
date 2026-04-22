import { useCallback, useState } from "react";

import { api } from "./api";
import { displayMediaTitle } from "./format";
import { navigate } from "./router";
import type { MediaItem, Room } from "./types";

export function useWatchTogether(
  item: MediaItem | null,
  onRoomCreated: (room: Room) => void,
) {
  const [creating, setCreating] = useState(false);
  const [error, setError] = useState<string | null>(null);

  const start = useCallback(async () => {
    if (!item) return;

    try {
      setCreating(true);
      setError(null);

      const title = displayMediaTitle(item);
      const room = await api.createRoom({
        name: title,
        mediaId: item.id,
        mediaTitle: title,
      });

      onRoomCreated(room);
      navigate({ name: "watch", mediaId: item.id, roomId: room.id });
    } catch (failure) {
      setError(
        failure instanceof Error ? failure.message : "Could not start Watch Together.",
      );
    } finally {
      setCreating(false);
    }
  }, [item, onRoomCreated]);

  return { creating, error, start };
}
