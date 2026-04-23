import { useEffect, useRef, useState } from "react";

import { randomName } from "../randomName";
import type { RoomSocketApi } from "../useRoomSocket";

type Props = {
  socket: RoomSocketApi;
  clientName: string;
  onClientNameChange: (name: string) => void;
  onLeave: () => void;
};

export function SessionMenu({
  socket,
  clientName,
  onClientNameChange,
  onLeave,
}: Props) {
  const [open, setOpen] = useState(false);
  const [copied, setCopied] = useState(false);
  // Local draft so in-progress typing doesn't churn the websocket — the
  // connection rebuilds whenever `clientName` changes, so we commit only
  // on blur/Enter.
  const [nameDraft, setNameDraft] = useState(clientName);
  const [prevClientName, setPrevClientName] = useState(clientName);
  const panelRef = useRef<HTMLDivElement | null>(null);
  const buttonRef = useRef<HTMLButtonElement | null>(null);

  if (clientName !== prevClientName) {
    setPrevClientName(clientName);
    setNameDraft(clientName);
  }

  const commitName = (value: string) => {
    const trimmed = value.trim();
    if (trimmed.length === 0) {
      setNameDraft(clientName);
      return;
    }
    if (trimmed === clientName) return;
    onClientNameChange(trimmed);
  };

  useEffect(() => {
    if (!open) return;

    const handlePointer = (event: MouseEvent) => {
      const target = event.target as Node | null;
      if (!target) return;
      if (panelRef.current?.contains(target)) return;
      if (buttonRef.current?.contains(target)) return;
      setOpen(false);
    };
    const handleKey = (event: KeyboardEvent) => {
      if (event.key === "Escape") setOpen(false);
    };

    document.addEventListener("mousedown", handlePointer);
    document.addEventListener("keydown", handleKey);
    return () => {
      document.removeEventListener("mousedown", handlePointer);
      document.removeEventListener("keydown", handleKey);
    };
  }, [open]);

  const copyInvite = async () => {
    const url = window.location.href;
    try {
      await navigator.clipboard.writeText(url);
    } catch {
      window.prompt("Copy invite link", url);
      return;
    }
    setCopied(true);
    window.setTimeout(() => setCopied(false), 2000);
  };

  return (
    <div className="session-menu">
      <button
        aria-expanded={open}
        aria-haspopup="menu"
        className="session-pill"
        onClick={() => setOpen((value) => !value)}
        ref={buttonRef}
        type="button"
      >
        <span
          aria-hidden="true"
          className={`session-pill-dot ${socket.connectionState}`}
        />
        <span>{pillLabel(socket)}</span>
      </button>
      {open ? (
        <div className="session-menu-panel" ref={panelRef} role="menu">
          <div className="session-menu-head">
            <p className="eyebrow">Watch Together</p>
            <h3>{socket.room?.name ?? "Session"}</h3>
            <p className="muted">{presenceText(socket)}</p>
          </div>

          {socket.participants.length > 0 ? (
            <ul className="watcher-list">
              {socket.participants.map((participant) => (
                <li className="watcher-row" key={participant.id}>
                  <span className="watcher-name">
                    {participant.name}
                    {participant.name === clientName ? (
                      <span className="watcher-self muted"> (you)</span>
                    ) : null}
                  </span>
                  <span className={driftClass(participant.driftSeconds)}>
                    {formatDrift(participant.driftSeconds)}
                  </span>
                </li>
              ))}
            </ul>
          ) : null}

          <button
            className="ghost-button wide"
            onClick={() => void copyInvite()}
            type="button"
          >
            {copied ? "Link copied" : "Copy invite link"}
          </button>

          <label className="input-stack">
            <span className="label-text">Your name</span>
            <div className="name-input-row">
              <input
                onBlur={(event) => commitName(event.target.value)}
                onChange={(event) => setNameDraft(event.target.value)}
                onKeyDown={(event) => {
                  if (event.key === "Enter") {
                    event.preventDefault();
                    commitName(event.currentTarget.value);
                    event.currentTarget.blur();
                  }
                }}
                placeholder="Browser Viewer"
                value={nameDraft}
              />
              <button
                className="ghost-button"
                onClick={() => onClientNameChange(randomName())}
                title="Pick a random name"
                type="button"
              >
                Shuffle
              </button>
            </div>
          </label>

          <button className="link-button" onClick={onLeave} type="button">
            Leave session
          </button>
        </div>
      ) : null}
    </div>
  );
}

function pillLabel(socket: RoomSocketApi): string {
  switch (socket.connectionState) {
    case "connecting":
      return "Connecting…";
    case "error":
      return "Connection error";
    case "offline":
      return "Offline";
    case "live":
      return socket.presenceCount > 1
        ? `Live · ${socket.presenceCount}`
        : "Live";
  }
}

function presenceText(socket: RoomSocketApi): string {
  if (socket.connectionState !== "live") return "Not connected.";
  if (socket.presenceCount <= 1) return "You're the only one here.";
  return `${socket.presenceCount} watcher${socket.presenceCount === 1 ? "" : "s"} connected.`;
}

function formatDrift(driftSeconds: number | null): string {
  if (driftSeconds === null) return "—";
  if (Math.abs(driftSeconds) < 0.05) return "in sync";
  const sign = driftSeconds > 0 ? "+" : "−";
  return `${sign}${Math.abs(driftSeconds).toFixed(2)}s`;
}

function driftClass(driftSeconds: number | null): string {
  if (driftSeconds === null) return "watcher-drift muted";
  const magnitude = Math.abs(driftSeconds);
  if (magnitude < 0.35) return "watcher-drift in-sync";
  if (magnitude < 1.5) return "watcher-drift nudge";
  return "watcher-drift out-of-sync";
}
