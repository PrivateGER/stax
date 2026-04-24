import { formatBytes, formatTimeCode, rootFolderName } from "../format";
import type { HealthResponse, LibraryRoot, MediaSummary, Room } from "../types";

type Props = {
  health: HealthResponse | null;
  roots: LibraryRoot[];
  items: MediaSummary[];
  rooms: Room[];
  scanning: boolean;
  onRescan: () => void;
};

export function AdminPage({ health, roots, items, rooms, scanning, onRescan }: Props) {
  const totalBytes = items.reduce((sum, item) => sum + item.sizeBytes, 0);
  const probeErrors = items.filter((item) => item.probeError);
  const scanErrors = roots.filter((root) => root.lastScanError);

  return (
    <div className="admin-page">
      <header className="admin-header">
        <h1>Admin & diagnostics</h1>
        <p className="muted">
          Operator-facing details. None of this should show up in the normal watch flow.
        </p>
      </header>

      <section className="admin-grid">
        <article className="stat-card">
          <p className="eyebrow">Service</p>
          <strong>{health ? `${health.service} v${health.version}` : "—"}</strong>
          <p className="muted">{health?.status ?? "Not reachable"}</p>
        </article>

        <article className="stat-card">
          <p className="eyebrow">Library</p>
          <strong>{items.length} titles</strong>
          <p className="muted">{formatBytes(totalBytes)} total</p>
        </article>

        <article className="stat-card">
          <p className="eyebrow">Sessions</p>
          <strong>
            {rooms.length} room{rooms.length === 1 ? "" : "s"}
          </strong>
          <p className="muted">Watch Together state</p>
        </article>

        <article className="stat-card">
          <p className="eyebrow">Issues</p>
          <strong>
            {scanErrors.length + probeErrors.length} warning
            {scanErrors.length + probeErrors.length === 1 ? "" : "s"}
          </strong>
          <p className="muted">Scan + probe errors</p>
        </article>
      </section>

      <section className="admin-section">
        <div className="admin-section-head">
          <h2>Library roots</h2>
          <button className="ghost-button" disabled={scanning} onClick={onRescan} type="button">
            {scanning ? "Scanning…" : "Rescan now"}
          </button>
        </div>

        {roots.length === 0 ? (
          <p className="muted">
            No roots configured. Set <code>STAX_LIBRARY_ROOTS</code>.
          </p>
        ) : (
          <table className="admin-table">
            <thead>
              <tr>
                <th>Root</th>
                <th>Last scanned</th>
                <th>Status</th>
              </tr>
            </thead>
            <tbody>
              {roots.map((root) => (
                <tr key={root.path}>
                  <td>
                    <strong>{rootFolderName(root.path)}</strong>
                    <div className="muted mono">{root.path}</div>
                  </td>
                  <td>{root.lastScannedAt ?? "—"}</td>
                  <td>
                    {root.lastScanError ? (
                      <span className="error">{root.lastScanError}</span>
                    ) : (
                      <span className="ok">OK</span>
                    )}
                  </td>
                </tr>
              ))}
            </tbody>
          </table>
        )}
      </section>

      {probeErrors.length > 0 ? (
        <section className="admin-section">
          <div className="admin-section-head">
            <h2>Probe failures</h2>
            <span className="muted">{probeErrors.length}</span>
          </div>
          <ul className="admin-list">
            {probeErrors.map((item) => (
              <li key={item.id}>
                <strong>{item.relativePath}</strong>
                <div className="muted">{item.probeError}</div>
              </li>
            ))}
          </ul>
        </section>
      ) : null}

      <section className="admin-section">
        <div className="admin-section-head">
          <h2>Active rooms</h2>
          <span className="muted">{rooms.length}</span>
        </div>

        {rooms.length === 0 ? (
          <p className="muted">No rooms exist right now.</p>
        ) : (
          <table className="admin-table">
            <thead>
              <tr>
                <th>Room</th>
                <th>Media</th>
                <th>State</th>
                <th>Position</th>
              </tr>
            </thead>
            <tbody>
              {rooms.map((room) => (
                <tr key={room.id}>
                  <td>
                    <strong>{room.name}</strong>
                    <div className="muted mono">{room.id}</div>
                  </td>
                  <td>{room.mediaTitle ?? "—"}</td>
                  <td>{room.playbackState.status}</td>
                  <td>{formatTimeCode(room.playbackState.positionSeconds)}</td>
                </tr>
              ))}
            </tbody>
          </table>
        )}
      </section>
    </div>
  );
}
