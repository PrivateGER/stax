import { useEffect, useRef, useState, type RefObject } from "react";

import type { MediaItem } from "../../types";
import {
  deriveExpectedPosition,
  monotonicNow,
  type RoomSocketApi,
  type RoomSocketCommand,
} from "../../useRoomSocket";
import type { MediabunnyController, MediabunnyState } from "./MediabunnyController";

type Options = {
  controllerRef: RefObject<MediabunnyController | null>;
  playerState: MediabunnyState;
  socket: RoomSocketApi;
  item: MediaItem | null;
  onAutoplayBlocked: (message: string) => void;
  onResync?: (deltaSeconds: number) => void;
};

// How far local playback has to diverge from the room clock before a local
// `seeked` event is treated as user intent rather than the echo of our own
// follow-room effect seeking the controller back onto the authoritative clock.
const SEEK_ECHO_TOLERANCE_SECONDS = 0.5;
const LOCAL_SEEK_GRACE_MS = 3_000;

type PendingSeek = {
  positionSeconds: number;
  sentAtMs: number;
};

export function useMediabunnyRoomSync({
  controllerRef,
  playerState,
  socket,
  item,
  onAutoplayBlocked,
  onResync,
}: Options): void {
  const live = socket.connectionState === "live";
  const ready = playerState.ready;
  const [clockTickMs, setClockTickMs] = useState<number>(monotonicNow());

  // Mirror the latest authoritative state so event forwarders don't need to
  // tear down listeners on every socket update.
  const roomRef = useRef(socket.room);
  const receiptRef = useRef(socket.authoritativeReceiptAtMs);
  const sendRef = useRef(socket.send);
  const latencyRef = useRef(socket.oneWayLatencyMs);
  const pendingSeekRef = useRef<PendingSeek | null>(null);
  const onResyncRef = useRef(onResync);
  roomRef.current = socket.room;
  receiptRef.current = socket.authoritativeReceiptAtMs;
  sendRef.current = socket.send;
  latencyRef.current = socket.oneWayLatencyMs;
  onResyncRef.current = onResync;

  useEffect(() => {
    pendingSeekRef.current = null;
  }, [item?.id, socket.room?.id]);

  useEffect(() => {
    const tickId = window.setInterval(() => {
      setClockTickMs(monotonicNow());
    }, 250);
    return () => window.clearInterval(tickId);
  }, []);

  // Periodically report our position so the server can classify drift.
  useEffect(() => {
    if (!live || !item || !socket.room || !ready) return;

    const interval = window.setInterval(() => {
      const controller = controllerRef.current;
      if (!controller) return;
      if (hasActivePendingSeek(pendingSeekRef, monotonicNow())) return;
      socket.send({
        type: "reportPosition",
        positionSeconds: Number(
          Math.max(0, controller.state.currentTime).toFixed(3),
        ),
      });
    }, 2000);

    return () => window.clearInterval(interval);
  }, [live, item, socket.room, socket, ready, controllerRef]);

  // Follow the room's authoritative state.
  useEffect(() => {
    if (
      !live ||
      !socket.room ||
      socket.authoritativeReceiptAtMs === null ||
      !item ||
      !ready
    )
      return;

    const controller = controllerRef.current;
    if (!controller) return;

    const pendingSeek = pendingSeekRef.current;
    if (pendingSeek) {
      const expectedForRoom = deriveExpectedPosition(
        socket.room,
        socket.authoritativeReceiptAtMs,
        clockTickMs,
        socket.oneWayLatencyMs,
      );
      const tolerance = Math.max(
        socket.room.playbackState.driftToleranceSeconds,
        SEEK_ECHO_TOLERANCE_SECONDS,
      );
      if (Math.abs(expectedForRoom - pendingSeek.positionSeconds) <= tolerance) {
        pendingSeekRef.current = null;
      } else if (clockTickMs - pendingSeek.sentAtMs < LOCAL_SEEK_GRACE_MS) {
        return;
      } else {
        pendingSeekRef.current = null;
      }
    }

    const expected = deriveExpectedPosition(
      socket.room,
      socket.authoritativeReceiptAtMs,
      clockTickMs,
      socket.oneWayLatencyMs,
    );
    const delta = controller.state.currentTime - expected;

    if (socket.room.playbackState.status === "paused") {
      if (controller.state.playing) controller.pause();
      if (Math.abs(delta) > 0.2) void controller.seek(expected);
      return;
    }

    const hardTolerance = Math.max(
      socket.room.playbackState.driftToleranceSeconds,
      1.25,
    );
    if (Math.abs(delta) > hardTolerance) {
      void controller.seek(expected);
    }

    if (!controller.state.playing) {
      void controller.play().catch(() => {
        onAutoplayBlocked("Press play once — the browser blocked autoplay.");
      });
    }
  }, [
    live,
    socket.room,
    socket.authoritativeReceiptAtMs,
    clockTickMs,
    item,
    ready,
    controllerRef,
    onAutoplayBlocked,
  ]);

  // Apply drift corrections. Mediabunny has no variable playback rate, so any
  // out-of-sync report collapses to a hard seek; the `nudge` branch from the
  // native-video sync path is intentionally absent.
  useEffect(() => {
    if (!socket.lastCorrection || !live || !item || !ready) return;
    if (socket.lastCorrection.suggestedAction === "inSync") return;

    const pendingSeek = pendingSeekRef.current;
    if (pendingSeek) {
      const ageMs = monotonicNow() - pendingSeek.sentAtMs;
      if (
        Math.abs(
          socket.lastCorrection.expectedPositionSeconds -
            pendingSeek.positionSeconds,
        ) <= SEEK_ECHO_TOLERANCE_SECONDS
      ) {
        pendingSeekRef.current = null;
      } else if (ageMs < LOCAL_SEEK_GRACE_MS) {
        return;
      } else {
        pendingSeekRef.current = null;
      }
    }

    const controller = controllerRef.current;
    if (!controller) return;

    void controller.seek(socket.lastCorrection.expectedPositionSeconds);
    onResyncRef.current?.(socket.lastCorrection.deltaSeconds);
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [socket.lastCorrection, live, item?.id, ready]);

  // Forward controller play/pause/seeked events as room intent, filtering out
  // echoes of the follow-room effect above.
  useEffect(() => {
    if (!live || !ready) return;
    const controller = controllerRef.current;
    if (!controller) return;

    const send = (command: RoomSocketCommand) => sendRef.current(command);
    const positionNow = () => Number(controller.state.currentTime.toFixed(1));

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
      const now = monotonicNow();
      const expected = deriveExpectedPosition(
        room,
        receiptRef.current,
        now,
        latencyRef.current,
      );
      if (
        Math.abs(controller.state.currentTime - expected) <
        SEEK_ECHO_TOLERANCE_SECONDS
      )
        return;
      const positionSeconds = positionNow();
      pendingSeekRef.current = {
        positionSeconds,
        sentAtMs: now,
      };
      send({ type: "seek", positionSeconds });
    };

    const offs = [
      controller.on("play", handlePlay),
      controller.on("pause", handlePause),
      controller.on("seeked", handleSeeked),
    ];

    return () => {
      for (const off of offs) off();
    };
  }, [live, ready, controllerRef]);
}

function hasActivePendingSeek(
  pendingSeekRef: RefObject<PendingSeek | null>,
  nowMs: number,
): boolean {
  const pendingSeek = pendingSeekRef.current;
  if (!pendingSeek) return false;
  if (nowMs - pendingSeek.sentAtMs < LOCAL_SEEK_GRACE_MS) return true;
  pendingSeekRef.current = null;
  return false;
}
