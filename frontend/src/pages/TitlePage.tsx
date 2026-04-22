import { useState } from "react";

import { api, thumbnailUrl } from "../api";
import {
  displayMediaTitle,
  formatBytes,
  formatDuration,
  formatResolution,
  posterInitials,
  rootFolderName,
} from "../format";
import { navigate } from "../router";
import type { MediaItem, Room } from "../types";

type Props = {
  item: MediaItem | null;
  rooms: Room[];
  onRoomCreated: (room: Room) => void;
};

export function TitlePage({ item, rooms, onRoomCreated }: Props) {
  const [creating, setCreating] = useState(false);
  const [error, setError] = useState<string | null>(null);

  if (!item) {
    return (
      <section className="title-missing">
        <h1>Title not found</h1>
        <p className="muted">This media is not in the library anymore.</p>
        <button
          className="primary-button"
          onClick={() => navigate({ name: "library", folder: null })}
          type="button"
        >
          Back to library
        </button>
      </section>
    );
  }

  const title = displayMediaTitle(item);
  const relatedRooms = rooms.filter((room) => room.mediaId === item.id);

  async function handleStartWatchTogether() {
    if (!item) return;

    try {
      setCreating(true);
      setError(null);

      const room = await api.createRoom({
        name: `${title} · Watch Party`,
        mediaId: item.id,
        mediaTitle: title,
      });

      onRoomCreated(room);
      navigate({ name: "watch", mediaId: item.id, roomId: room.id });
    } catch (creationError) {
      setError(
        creationError instanceof Error
          ? creationError.message
          : "Could not start Watch Together.",
      );
    } finally {
      setCreating(false);
    }
  }

  const metaRows: Array<[string, string]> = [];
  if (item.durationSeconds !== null) {
    metaRows.push(["Runtime", formatDuration(item.durationSeconds)]);
  }
  const resolution = formatResolution(item.width, item.height);
  if (resolution) metaRows.push(["Resolution", resolution]);
  if (item.videoCodec) metaRows.push(["Video", item.videoCodec.toUpperCase()]);
  if (item.audioCodec) metaRows.push(["Audio", item.audioCodec.toUpperCase()]);
  if (item.containerName) metaRows.push(["Container", item.containerName]);
  metaRows.push(["File size", formatBytes(item.sizeBytes)]);
  metaRows.push(["Library", rootFolderName(item.rootPath)]);

  return (
    <article className="title-page">
      <div className="title-hero">
        <div className="title-hero-art">
          {item.thumbnailGeneratedAt ? (
            <img alt="" className="title-hero-image" src={thumbnailUrl(item.id)} />
          ) : (
            <span className="poster-initials large">{posterInitials(title)}</span>
          )}
        </div>

        <div className="title-hero-copy">
          <p className="eyebrow">
            {item.durationSeconds !== null ? formatDuration(item.durationSeconds) : "Movie"}
            {resolution ? ` · ${resolution}` : ""}
            {item.subtitleTracks.length > 0
              ? ` · ${item.subtitleTracks.length} subtitle${item.subtitleTracks.length === 1 ? "" : "s"}`
              : ""}
          </p>
          <h1>{title}</h1>
          <p className="muted title-path">{item.relativePath}</p>

          <div className="title-cta">
            <button
              className="primary-button"
              onClick={() => navigate({ name: "watch", mediaId: item.id, roomId: null })}
              type="button"
            >
              ▶ Play
            </button>
            <button
              className="ghost-button"
              disabled={creating}
              onClick={() => void handleStartWatchTogether()}
              type="button"
            >
              {creating ? "Starting…" : "Watch Together"}
            </button>
          </div>

          {error ? <p className="error">{error}</p> : null}
        </div>
      </div>

      <section className="title-details">
        <div>
          <h2>Details</h2>
          <dl className="detail-grid">
            {metaRows.map(([label, value]) => (
              <div className="detail-row" key={label}>
                <dt>{label}</dt>
                <dd>{value}</dd>
              </div>
            ))}
          </dl>
        </div>

        <div>
          <h2>Subtitles</h2>
          {item.subtitleTracks.length === 0 ? (
            <p className="muted">No subtitle tracks indexed.</p>
          ) : (
            <ul className="subtitle-list">
              {item.subtitleTracks.map((track) => (
                <li key={track.relativePath}>
                  <strong>{track.label}</strong>
                  {track.language ? <span className="muted"> · {track.language}</span> : null}
                </li>
              ))}
            </ul>
          )}
        </div>

        {relatedRooms.length > 0 ? (
          <div>
            <h2>Active sessions</h2>
            <ul className="related-rooms">
              {relatedRooms.map((room) => (
                <li key={room.id}>
                  <button
                    className="room-link"
                    onClick={() =>
                      navigate({ name: "watch", mediaId: item.id, roomId: room.id })
                    }
                    type="button"
                  >
                    <strong>{room.name}</strong>
                    <span className="muted">
                      {room.playbackState.status === "playing" ? "Playing" : "Paused"}
                    </span>
                  </button>
                </li>
              ))}
            </ul>
          </div>
        ) : null}
      </section>
    </article>
  );
}
