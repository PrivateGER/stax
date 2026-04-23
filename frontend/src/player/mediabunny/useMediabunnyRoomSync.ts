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
};

// How far local playback has to diverge from the room clock before a local
// `seeked` event is treated as user intent rather than the echo of our own
// follow-room effect seeking the controller back onto the authoritative clock.
const SEEK_ECHO_TOLERANCE_SECONDS = 0.5;

export function useMediabunnyRoomSync({
  controllerRef,
  playerState,
  socket,
  item,
  onAutoplayBlocked,
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
  roomRef.current = socket.room;
  receiptRef.current = socket.authoritativeReceiptAtMs;
  sendRef.current = socket.send;
  latencyRef.current = socket.oneWayLatencyMs;

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

    const controller = controllerRef.current;
    if (!controller) return;

    void controller.seek(socket.lastCorrection.expectedPositionSeconds);
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
      const expected = deriveExpectedPosition(
        room,
        receiptRef.current,
        monotonicNow(),
        latencyRef.current,
      );
      if (
        Math.abs(controller.state.currentTime - expected) <
        SEEK_ECHO_TOLERANCE_SECONDS
      )
        return;
      send({ type: "seek", positionSeconds: positionNow() });
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
