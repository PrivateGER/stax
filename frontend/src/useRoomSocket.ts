import { useCallback, useEffect, useRef, useState } from "react";

import { socketUrl } from "./api";
import type {
  ConnectionState,
  DriftCorrectionEvent,
  Room,
  SocketEvent,
} from "./types";

export type RoomSocketCommand =
  | { type: "play"; positionSeconds?: number }
  | { type: "pause"; positionSeconds?: number }
  | { type: "seek"; positionSeconds: number }
  | { type: "reportPosition"; positionSeconds: number };

export type RoomSocketState = {
  connectionState: ConnectionState;
  room: Room | null;
  presenceCount: number;
  error: string | null;
  lastCorrection: DriftCorrectionEvent | null;
  authoritativeReceiptAtMs: number | null;
  activity: string;
};

export type RoomSocketApi = RoomSocketState & {
  send: (command: RoomSocketCommand) => void;
};

export function monotonicNow() {
  return typeof performance !== "undefined" ? performance.now() : Date.now();
}

const IDLE_STATE: RoomSocketState = {
  connectionState: "offline",
  room: null,
  presenceCount: 0,
  error: null,
  lastCorrection: null,
  authoritativeReceiptAtMs: null,
  activity: "Not connected",
};

export function useRoomSocket(roomId: string | null, clientName: string): RoomSocketApi {
  const [state, setState] = useState<RoomSocketState>(IDLE_STATE);
  const socketRef = useRef<WebSocket | null>(null);

  useEffect(() => {
    if (!roomId) {
      socketRef.current?.close();
      socketRef.current = null;
      setState(IDLE_STATE);
      return;
    }

    setState({
      ...IDLE_STATE,
      connectionState: "connecting",
      activity: "Opening Watch Together session…",
    });

    const socket = new WebSocket(socketUrl(roomId, clientName));
    socketRef.current = socket;

    socket.onopen = () => {
      if (socketRef.current !== socket) return;
      setState((current) => ({
        ...current,
        activity: "Connected. Waiting for snapshot…",
      }));
    };

    socket.onmessage = (event) => {
      if (socketRef.current !== socket || typeof event.data !== "string") return;

      try {
        const message = JSON.parse(event.data) as SocketEvent;
        const receipt = monotonicNow();

        if (message.type === "snapshot") {
          setState({
            connectionState: "live",
            room: message.room,
            presenceCount: message.connectionCount,
            error: null,
            lastCorrection: null,
            authoritativeReceiptAtMs: receipt,
            activity:
              message.connectionCount === 1
                ? "You're the only one here."
                : `${message.connectionCount} watchers connected.`,
          });
          return;
        }

        if (message.type === "playbackUpdated") {
          setState((current) => ({
            ...current,
            room: message.room,
            authoritativeReceiptAtMs: receipt,
            activity: `${message.actor} · ${message.action}`,
          }));
          return;
        }

        if (message.type === "presenceChanged") {
          if (message.roomId !== roomId) return;
          setState((current) => ({
            ...current,
            presenceCount: message.connectionCount,
            activity: `${message.actor} ${message.joined ? "joined" : "left"}`,
          }));
          return;
        }

        if (message.type === "driftCorrection") {
          setState((current) => ({ ...current, lastCorrection: message }));
          return;
        }

        setState((current) => ({ ...current, error: message.message }));
      } catch (error) {
        setState((current) => ({
          ...current,
          connectionState: "error",
          error: error instanceof Error ? error.message : "Socket sent unexpected data.",
        }));
      }
    };

    socket.onerror = () => {
      if (socketRef.current !== socket) return;
      setState((current) => ({
        ...current,
        connectionState: "error",
        error: "Watch Together transport error.",
      }));
    };

    socket.onclose = () => {
      if (socketRef.current !== socket) return;
      socketRef.current = null;
      setState((current) => ({
        ...current,
        connectionState: "offline",
        activity: "Watch Together session closed.",
      }));
    };

    return () => {
      if (socketRef.current === socket) socketRef.current = null;
      socket.close();
    };
  }, [roomId, clientName]);

  const send = useCallback((command: RoomSocketCommand) => {
    const socket = socketRef.current;
    if (!socket || socket.readyState !== WebSocket.OPEN) return;
    socket.send(JSON.stringify(command));
  }, []);

  return { ...state, send };
}

export function deriveExpectedPosition(
  room: Room | null,
  receiptAtMs: number | null,
  nowMs: number,
) {
  if (!room) return 0;

  if (room.playbackState.status !== "playing" || receiptAtMs === null) {
    return room.playbackState.positionSeconds;
  }

  const elapsedSeconds = Math.max(0, nowMs - receiptAtMs) / 1000;

  return Number(
    (
      room.playbackState.positionSeconds +
      elapsedSeconds * room.playbackState.playbackRate
    ).toFixed(3),
  );
}
