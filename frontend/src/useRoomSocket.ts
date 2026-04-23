import { useCallback, useEffect, useRef, useState } from "react";

import { socketUrl } from "./api";
import type {
  ConnectionState,
  DriftCorrectionEvent,
  Participant,
  Room,
  SocketEvent,
} from "./types";

export type RoomSocketCommand =
  | { type: "play"; positionSeconds?: number; clientOneWayMs?: number }
  | { type: "pause"; positionSeconds?: number; clientOneWayMs?: number }
  | { type: "seek"; positionSeconds: number; clientOneWayMs?: number }
  | { type: "selectMedia"; mediaId: string }
  | { type: "reportPosition"; positionSeconds: number };

type OutgoingMessage =
  | RoomSocketCommand
  | { type: "ping"; clientSentAtMs: number };

export type RoomSocketState = {
  connectionState: ConnectionState;
  room: Room | null;
  presenceCount: number;
  participants: Participant[];
  error: string | null;
  lastCorrection: DriftCorrectionEvent | null;
  authoritativeReceiptAtMs: number | null;
  activity: string;
  oneWayLatencyMs: number | null;
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
  participants: [],
  error: null,
  lastCorrection: null,
  authoritativeReceiptAtMs: null,
  activity: "Not connected",
  oneWayLatencyMs: null,
};

const PING_INTERVAL_MS = 5_000;
const LATENCY_WINDOW = 5;

export function useRoomSocket(roomId: string | null, clientName: string): RoomSocketApi {
  const [state, setState] = useState<RoomSocketState>(IDLE_STATE);
  const socketRef = useRef<WebSocket | null>(null);
  // Ref mirror of the live latency estimate so `send` can inject
  // `clientOneWayMs` without re-creating itself on every sample.
  const latencyRef = useRef<number | null>(null);

  useEffect(() => {
    if (!roomId) {
      socketRef.current?.close();
      socketRef.current = null;
      latencyRef.current = null;
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
    latencyRef.current = null;
    // Rolling window of recent one-way latency samples; we take the min so
    // GC/jitter spikes don't inflate the projection correction.
    const latencySamples: number[] = [];
    let pingInterval: number | null = null;

    const sendPing = () => {
      if (socket.readyState !== WebSocket.OPEN) return;
      socket.send(
        JSON.stringify({ type: "ping", clientSentAtMs: monotonicNow() }),
      );
    };

    socket.onopen = () => {
      if (socketRef.current !== socket) return;
      setState((current) => ({
        ...current,
        activity: "Connected. Waiting for snapshot…",
      }));
      sendPing();
      pingInterval = window.setInterval(sendPing, PING_INTERVAL_MS);
    };

    socket.onmessage = (event) => {
      if (socketRef.current !== socket || typeof event.data !== "string") return;

      try {
        const message = JSON.parse(event.data) as SocketEvent;
        const receipt = monotonicNow();

        if (message.type === "pong") {
          const rtt = receipt - message.clientSentAtMs;
          if (rtt >= 0 && Number.isFinite(rtt)) {
            const oneWay = rtt / 2;
            latencySamples.push(oneWay);
            if (latencySamples.length > LATENCY_WINDOW) latencySamples.shift();
            const min = Math.min(...latencySamples);
            latencyRef.current = min;
            setState((current) => ({ ...current, oneWayLatencyMs: min }));
          }
          return;
        }

        if (message.type === "snapshot") {
          setState((current) => ({
            connectionState: "live",
            room: message.room,
            presenceCount: message.connectionCount,
            participants: message.participants,
            error: null,
            lastCorrection: null,
            authoritativeReceiptAtMs: receipt,
            activity:
              message.connectionCount === 1
                ? "You're the only one here."
                : `${message.connectionCount} watchers connected.`,
            oneWayLatencyMs: current.oneWayLatencyMs,
          }));
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

        if (message.type === "mediaChanged") {
          setState((current) => ({
            ...current,
            room: message.room,
            authoritativeReceiptAtMs: receipt,
            lastCorrection: null,
            activity: `${message.actor} · changed video`,
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

        if (message.type === "participantsUpdated") {
          if (message.roomId !== roomId) return;
          setState((current) => ({ ...current, participants: message.participants }));
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
      if (pingInterval !== null) window.clearInterval(pingInterval);
      latencyRef.current = null;
      setState((current) => ({
        ...current,
        connectionState: "offline",
        activity: "Watch Together session closed.",
        oneWayLatencyMs: null,
      }));
    };

    return () => {
      if (socketRef.current === socket) socketRef.current = null;
      if (pingInterval !== null) window.clearInterval(pingInterval);
      latencyRef.current = null;
      socket.close();
    };
  }, [roomId, clientName]);

  const send = useCallback((command: RoomSocketCommand) => {
    const socket = socketRef.current;
    if (!socket || socket.readyState !== WebSocket.OPEN) return;

    let outgoing: OutgoingMessage = command;
    if (
      (command.type === "play" ||
        command.type === "pause" ||
        command.type === "seek") &&
      command.clientOneWayMs === undefined &&
      latencyRef.current !== null
    ) {
      outgoing = { ...command, clientOneWayMs: Math.round(latencyRef.current) };
    }

    socket.send(JSON.stringify(outgoing));
  }, []);

  return { ...state, send };
}

export function deriveExpectedPosition(
  room: Room | null,
  receiptAtMs: number | null,
  nowMs: number,
  oneWayLatencyMs: number | null,
) {
  if (!room) return 0;

  if (room.playbackState.status !== "playing" || receiptAtMs === null) {
    return room.playbackState.positionSeconds;
  }

  // Pretend the message arrived when the server sent it, so our projection
  // is measured from the authoritative emit time rather than our local receipt.
  const adjustedReceiptMs =
    oneWayLatencyMs !== null ? receiptAtMs - oneWayLatencyMs : receiptAtMs;
  const elapsedSeconds = Math.max(0, nowMs - adjustedReceiptMs) / 1000;

  return Number(
    (
      room.playbackState.positionSeconds +
      elapsedSeconds * room.playbackState.playbackRate
    ).toFixed(3),
  );
}
