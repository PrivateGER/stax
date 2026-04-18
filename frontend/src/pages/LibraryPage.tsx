import { useMemo, useState } from "react";

import { thumbnailUrl } from "../api";
import {
  displayMediaTitle,
  formatDuration,
  mediaBadges,
  posterInitials,
  rootFolderName,
} from "../format";
import { navigate } from "../router";
import type { LibraryRoot, MediaItem } from "../types";

type Props = {
  items: MediaItem[];
  roots: LibraryRoot[];
  loading: boolean;
  scanning: boolean;
  error: string | null;
  onRescan: () => void;
};

export function LibraryPage({ items, roots, loading, scanning, error, onRescan }: Props) {
  const [query, setQuery] = useState("");
  const [activeRoot, setActiveRoot] = useState<string | null>(null);

  const sorted = useMemo(() => {
    return [...items].sort(
      (left, right) =>
        right.indexedAt.localeCompare(left.indexedAt) ||
        left.fileName.localeCompare(right.fileName),
    );
  }, [items]);

  const filtered = useMemo(() => {
    const normalized = query.trim().toLowerCase();

    return sorted.filter((item) => {
      if (activeRoot && item.rootPath !== activeRoot) {
        return false;
      }

      if (!normalized) return true;

      return [
        item.fileName,
        item.relativePath,
        item.containerName ?? "",
        item.videoCodec ?? "",
        item.audioCodec ?? "",
      ]
        .join(" ")
        .toLowerCase()
        .includes(normalized);
    });
  }, [activeRoot, query, sorted]);

  const recent = sorted.slice(0, 6);

  if (roots.length === 0 && !loading) {
    return (
      <section className="library-empty">
        <h1>Add a library root to get started</h1>
        <p className="muted">
          Set <code>SYNCPLAY_LIBRARY_ROOTS</code> to one or more directories and rescan.
          The app is built around your media — it won't look right without it.
        </p>
        <button className="primary-button" disabled={scanning} onClick={onRescan} type="button">
          {scanning ? "Scanning…" : "Rescan now"}
        </button>
        {error ? <p className="error">{error}</p> : null}
      </section>
    );
  }

  return (
    <div className="library-page">
      <header className="library-header">
        <div>
          <h1>Library</h1>
          <p className="muted">
            {items.length} title{items.length === 1 ? "" : "s"} indexed
            {roots.length > 0
              ? ` across ${roots.length} root${roots.length === 1 ? "" : "s"}`
              : ""}
            .
          </p>
        </div>

        <div className="library-header-controls">
          <input
            className="search-input"
            onChange={(event) => setQuery(event.target.value)}
            placeholder="Search titles…"
            value={query}
          />
          <button className="ghost-button" disabled={scanning} onClick={onRescan} type="button">
            {scanning ? "Scanning…" : "Rescan"}
          </button>
        </div>
      </header>

      {roots.length > 1 ? (
        <div className="chip-row">
          <button
            className={`chip ${activeRoot === null ? "active" : ""}`}
            onClick={() => setActiveRoot(null)}
            type="button"
          >
            All
          </button>
          {roots.map((root) => (
            <button
              className={`chip ${activeRoot === root.path ? "active" : ""} ${
                root.lastScanError ? "chip-error" : ""
              }`}
              key={root.path}
              onClick={() => setActiveRoot(root.path)}
              type="button"
            >
              {rootFolderName(root.path)}
            </button>
          ))}
        </div>
      ) : null}

      {error ? <p className="error">{error}</p> : null}

      {recent.length > 0 && !query && !activeRoot ? (
        <section className="library-section">
          <div className="library-section-head">
            <h2>Recently added</h2>
          </div>
          <div className="poster-row">
            {recent.map((item) => (
              <PosterCard key={item.id} item={item} />
            ))}
          </div>
        </section>
      ) : null}

      <section className="library-section">
        <div className="library-section-head">
          <h2>{query || activeRoot ? "Results" : "All titles"}</h2>
          <span className="muted">
            {filtered.length} of {items.length}
          </span>
        </div>

        {loading ? (
          <p className="muted">Loading library…</p>
        ) : filtered.length === 0 ? (
          <p className="muted">
            {items.length === 0
              ? "No media indexed yet. Add files to a library root and rescan."
              : "No titles match the current filter."}
          </p>
        ) : (
          <div className="poster-grid">
            {filtered.map((item) => (
              <PosterCard key={item.id} item={item} />
            ))}
          </div>
        )}
      </section>
    </div>
  );
}

function PosterCard({ item }: { item: MediaItem }) {
  const title = displayMediaTitle(item);
  const badges = mediaBadges(item);
  const hasThumbnail = item.thumbnailGeneratedAt !== null;

  return (
    <button
      className="poster-card"
      onClick={() => navigate({ name: "title", mediaId: item.id })}
      type="button"
    >
      <div className="poster-art">
        {hasThumbnail ? (
          <img
            alt=""
            className="poster-image"
            loading="lazy"
            src={thumbnailUrl(item.id)}
          />
        ) : (
          <span className="poster-initials">{posterInitials(title)}</span>
        )}
        {item.durationSeconds ? (
          <span className="poster-duration">{formatDuration(item.durationSeconds)}</span>
        ) : null}
      </div>
      <div className="poster-meta">
        <h3 className="poster-title" title={title}>
          {title}
        </h3>
        <p className="poster-sub muted">
          {badges.length > 0 ? badges.join(" · ") : (item.extension ?? "media").toUpperCase()}
        </p>
      </div>
    </button>
  );
}
