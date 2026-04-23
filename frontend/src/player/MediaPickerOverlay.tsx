import { useEffect, useMemo, useRef, useState } from "react";

import { thumbnailUrl } from "../api";
import {
  displayMediaTitle,
  formatDuration,
  mediaAspectRatio,
  mediaBadges,
  posterInitials,
} from "../format";
import type { MediaItem } from "../types";

type Props = {
  items: MediaItem[];
  currentMediaId: string | null;
  onSelect: (mediaId: string) => void;
  onClose: () => void;
};

export function MediaPickerOverlay({
  items,
  currentMediaId,
  onSelect,
  onClose,
}: Props) {
  const [query, setQuery] = useState("");
  const searchRef = useRef<HTMLInputElement | null>(null);

  useEffect(() => {
    searchRef.current?.focus();

    const handleKey = (event: KeyboardEvent) => {
      if (event.key === "Escape") onClose();
    };
    document.addEventListener("keydown", handleKey);
    return () => document.removeEventListener("keydown", handleKey);
  }, [onClose]);

  const normalizedQuery = query.trim().toLowerCase();
  const filtered = useMemo(() => {
    const source = [...items].sort((left, right) =>
      left.fileName.localeCompare(right.fileName),
    );
    if (!normalizedQuery) return source;
    return source.filter((item) =>
      [
        item.fileName,
        item.relativePath,
        item.containerName ?? "",
        item.videoCodec ?? "",
      ]
        .join(" ")
        .toLowerCase()
        .includes(normalizedQuery),
    );
  }, [items, normalizedQuery]);

  return (
    <div
      className="media-picker-backdrop"
      onClick={(event) => {
        if (event.target === event.currentTarget) onClose();
      }}
      role="dialog"
      aria-modal="true"
      aria-label="Change video"
    >
      <div className="media-picker-panel">
        <header className="media-picker-header">
          <div>
            <p className="eyebrow">Watch Together</p>
            <h2>Change video for everyone</h2>
          </div>
          <button
            className="link-button"
            onClick={onClose}
            type="button"
          >
            Cancel
          </button>
        </header>

        <input
          className="search-input"
          onChange={(event) => setQuery(event.target.value)}
          placeholder="Search titles…"
          ref={searchRef}
          value={query}
        />

        {filtered.length === 0 ? (
          <p className="muted">No titles match.</p>
        ) : (
          <div className="poster-grid media-picker-grid">
            {filtered.map((item) => (
              <PickerCard
                item={item}
                key={item.id}
                isCurrent={item.id === currentMediaId}
                onSelect={() => onSelect(item.id)}
              />
            ))}
          </div>
        )}
      </div>
    </div>
  );
}

function PickerCard({
  item,
  isCurrent,
  onSelect,
}: {
  item: MediaItem;
  isCurrent: boolean;
  onSelect: () => void;
}) {
  const title = displayMediaTitle(item);
  const badges = mediaBadges(item);
  const hasThumbnail = item.thumbnailGeneratedAt !== null;
  const thumbnailAspectRatio = mediaAspectRatio(item.width, item.height);
  const posterArtStyle =
    hasThumbnail && thumbnailAspectRatio
      ? { aspectRatio: thumbnailAspectRatio }
      : undefined;

  return (
    <button
      className={`poster-card ${isCurrent ? "poster-card-current" : ""}`}
      disabled={isCurrent}
      onClick={onSelect}
      type="button"
    >
      <div className="poster-art" style={posterArtStyle}>
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
          <span className="poster-duration">
            {formatDuration(item.durationSeconds)}
          </span>
        ) : null}
      </div>
      <div className="poster-meta">
        <h3 className="poster-title" title={title}>
          {title}
        </h3>
        <p className="poster-sub muted">
          {isCurrent
            ? "Now playing"
            : badges.length > 0
              ? badges.join(" · ")
              : (item.extension ?? "media").toUpperCase()}
        </p>
      </div>
    </button>
  );
}
