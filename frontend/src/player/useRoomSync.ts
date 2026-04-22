import { useEffect, useRef, useState, type RefObject } from "react";

import type { MediaItem } from "../types";
import {
  deriveExpectedPosition,
  monotonicNow,
  type RoomSocketApi,
  type RoomSocketCommand,
} from "../useRoomSocket";

type Options = {
  videoRef: RefObject<HTMLVideoElement | null>;
  socket: RoomSocketApi;
  item: MediaItem | null;
  onAutoplayBlocked: (message: string) => void;
};

// How far local playback has to diverge from the room clock before a local
// `seeked` event is treated as user intent rather than the echo of our own
// follow-room effect writing `video.currentTime` back.
const SEEK_ECHO_TOLERANCE_SECONDS = 0.5;

export function useRoomSync({
  videoRef,
  socket,
  item,
  onAutoplayBlocked,
}: Options): void {
  const live = socket.connectionState === "live";
  const nudgeResetRef = useRef<number | null>(null);
  const [clockTickMs, setClockTickMs] = useState<number>(monotonicNow());

  // Mirror of the latest authoritative state so the native-event forwarders
  // below don't need to tear down their listeners on every socket update.
  const roomRef = useRef(socket.room);
  const receiptRef = useRef(socket.authoritativeReceiptAtMs);
  const sendRef = useRef(socket.send);
  roomRef.current = socket.room;
  receiptRef.current = socket.authoritativeReceiptAtMs;
  sendRef.current = socket.send;

  useEffect(() => {
    const tickId = window.setInterval(() => {
      setClockTickMs(monotonicNow());
    }, 250);
    return () => window.clearInterval(tickId);
  }, []);

  useEffect(
    () => () => {
      if (nudgeResetRef.current !== null) {
        window.clearTimeout(nudgeResetRef.current);
      }
    },
    [],
  );

  // Periodically report our position to the room for drift correction.
  useEffect(() => {
    if (!live || !item || !socket.room) return;

    const interval = window.setInterval(() => {
      const video = videoRef.current;
      if (!video) return;
      socket.send({
        type: "reportPosition",
        positionSeconds: Number(Math.max(0, video.currentTime).toFixed(3)),
      });
    }, 2000);

    return () => window.clearInterval(interval);
  }, [live, item, socket.room, socket, videoRef]);

  // Follow room authoritative state when live.
  useEffect(() => {
    if (!live || !socket.room || socket.authoritativeReceiptAtMs === null || !item) return;

    const video = videoRef.current;
    if (!video) return;

    const expected = deriveExpectedPosition(
      socket.room,
      socket.authoritativeReceiptAtMs,
      clockTickMs,
    );
    const delta = video.currentTime - expected;
    const baseRate = socket.room.playbackState.playbackRate;

    if (Math.abs(video.playbackRate - baseRate) > 0.001) {
      video.playbackRate = baseRate;
    }

    if (socket.room.playbackState.status === "paused") {
      if (!video.paused) video.pause();
      if (Math.abs(delta) > 0.2) video.currentTime = expected;
      return;
    }

    const hardTolerance = Math.max(socket.room.playbackState.driftToleranceSeconds, 1.25);
    if (Math.abs(delta) > hardTolerance) {
      video.currentTime = expected;
    }

    if (video.paused && video.readyState >= HTMLMediaElement.HAVE_CURRENT_DATA) {
      void video.play().catch(() => {
        onAutoplayBlocked("Press play once — the browser blocked autoplay.");
      });
    }
  }, [
    live,
    socket.room,
    socket.authoritativeReceiptAtMs,
    clockTickMs,
    item,
    videoRef,
    onAutoplayBlocked,
  ]);

  // Apply drift correction nudges/seeks. Keep `item` out of the deps: library
  // polls hand us a fresh item reference every ~10s, and re-running this effect
  // would replay the last (stale) `expectedPositionSeconds` and yank playback
  // backward. A corrections-only trigger is what we actually want.
  useEffect(() => {
    if (!socket.lastCorrection || !live || !item) return;

    const video = videoRef.current;
    if (!video) return;

    const baseRate = socket.room?.playbackState.playbackRate ?? 1;

    if (nudgeResetRef.current !== null) {
      window.clearTimeout(nudgeResetRef.current);
      nudgeResetRef.current = null;
    }

    if (socket.lastCorrection.suggestedAction === "seek") {
      video.currentTime = socket.lastCorrection.expectedPositionSeconds;
      video.playbackRate = baseRate;
      return;
    }

    if (socket.lastCorrection.suggestedAction === "nudge") {
      const adjusted = baseRate + (socket.lastCorrection.deltaSeconds > 0 ? -0.08 : 0.08);
      video.playbackRate = Math.min(1.12, Math.max(0.88, Number(adjusted.toFixed(2))));
      nudgeResetRef.current = window.setTimeout(() => {
        if (videoRef.current) videoRef.current.playbackRate = baseRate;
      }, 1500);
      return;
    }

    video.playbackRate = baseRate;
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [socket.lastCorrection, live, item?.id, socket.room]);

  // Forward native video-element play/pause/seeked events as room intent.
  // Compare against the authoritative state so programmatic updates we apply
  // from the follow-room effect don't round-trip back to the server.
  useEffect(() => {
    if (!live) return;
    const video = videoRef.current;
    if (!video) return;

    const send = (command: RoomSocketCommand) => sendRef.current(command);
    const positionNow = () => Number(video.currentTime.toFixed(1));

    const handlePlay = () => {
      const room = roomRef.current;
      if (!room || room.playbackState.status === "playing") return;
      send({ type: "play", positionSeconds: positionNow() });
    };

    const handlePause = () => {
      const room = roomRef.current;
      if (!room || room.playbackState.status === "paused") return;
      send({ type: "pause", positionSeconds: positionNow() });
    };

    const handleSeeked = () => {
      const room = roomRef.current;
      if (!room) return;
      const expected = deriveExpectedPosition(
        room,
        receiptRef.current,
        monotonicNow(),
      );
      if (Math.abs(video.currentTime - expected) < SEEK_ECHO_TOLERANCE_SECONDS) return;
      send({ type: "seek", positionSeconds: positionNow() });
    };

    video.addEventListener("play", handlePlay);
    video.addEventListener("pause", handlePause);
    video.addEventListener("seeked", handleSeeked);

    return () => {
      video.removeEventListener("play", handlePlay);
      video.removeEventListener("pause", handlePause);
      video.removeEventListener("seeked", handleSeeked);
    };
  }, [live, videoRef]);

}
