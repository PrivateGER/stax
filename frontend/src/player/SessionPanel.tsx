import { formatSignedDelta, formatTimeCode } from "../format";
import type { Room } from "../types";
import { deriveExpectedPosition, type RoomSocketApi } from "../useRoomSocket";

type Props = {
  socket: RoomSocketApi;
  clientName: string;
  onClientNameChange: (name: string) => void;
  onCatchUp: () => void;
  onLeave: () => void;
  rooms: Room[];
  clockTickMs: number;
};

export function SessionPanel({
  socket,
  clientName,
  onClientNameChange,
  onCatchUp,
  onLeave,
  rooms,
  clockTickMs,
}: Props) {
  const live = socket.connectionState === "live";

  return (
    <aside className="session-panel">
      <div className="session-head">
        <div>
          <p className="eyebrow">Watch Together</p>
          <h2>{socket.room?.name ?? "Session"}</h2>
        </div>
        <span className={`connection-pill ${socket.connectionState}`}>
          {labelForConnection(socket.connectionState)}
        </span>
      </div>

      <p className="session-presence">
        {socket.presenceCount === 0
          ? "No one else here yet."
          : `${socket.presenceCount} watcher${socket.presenceCount === 1 ? "" : "s"} connected`}
      </p>

      <label className="input-stack">
        <span className="label-text">Your name</span>
        <input
          onChange={(event) => onClientNameChange(event.target.value)}
          placeholder="Browser Viewer"
          value={clientName}
        />
      </label>

      <p className="muted session-hint">
        Play, pause, and scrub from the video — everyone in the room follows.
      </p>

      <button
        className="ghost-button wide"
        disabled={!live}
        onClick={onCatchUp}
        type="button"
      >
        Catch up to room
      </button>

      <p className="muted session-activity">{socket.activity}</p>
      {socket.error ? <p className="error">{socket.error}</p> : null}

      <details className="session-debug">
        <summary>Diagnostics</summary>
        <dl>
          <div>
            <dt>Room clock</dt>
            <dd>
              {formatTimeCode(
                deriveExpectedPosition(
                  socket.room,
                  socket.authoritativeReceiptAtMs,
                  clockTickMs,
                ),
              )}
            </dd>
          </div>
          <div>
            <dt>Last drift</dt>
            <dd>
              {socket.lastCorrection
                ? `${formatSignedDelta(socket.lastCorrection.deltaSeconds)}s`
                : "n/a"}
            </dd>
          </div>
          <div>
            <dt>Correction</dt>
            <dd>{socket.lastCorrection?.suggestedAction ?? "—"}</dd>
          </div>
        </dl>
      </details>

      <div className="session-footer">
        <button className="link-button" onClick={onLeave} type="button">
          Leave session
        </button>
        {rooms.length > 0 ? (
          <p className="muted">
            {rooms.length} active session{rooms.length === 1 ? "" : "s"}
          </p>
        ) : null}
      </div>
    </aside>
  );
}

function labelForConnection(state: RoomSocketApi["connectionState"]) {
  switch (state) {
    case "live":
      return "Live";
    case "connecting":
      return "Connecting";
    case "error":
      return "Error";
    default:
      return "Offline";
  }
}
