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
  const panelRef = useRef<HTMLDivElement | null>(null);
  const buttonRef = useRef<HTMLButtonElement | null>(null);

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
                onChange={(event) => onClientNameChange(event.target.value)}
                placeholder="Browser Viewer"
                value={clientName}
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
