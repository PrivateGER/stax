import { useMemo, useState } from "react";

import { thumbnailUrl } from "../api";
import { buildFolderTree, findFolder, folderPathSegments, type FolderNode } from "../folderTree";
import {
  displayMediaTitle,
  formatDuration,
  mediaAspectRatio,
  mediaBadges,
  posterInitials,
} from "../format";
import { navigate } from "../router";
import type { LibraryRoot, MediaItem } from "../types";

type Props = {
  items: MediaItem[];
  roots: LibraryRoot[];
  folder: string | null;
  loading: boolean;
  scanning: boolean;
  error: string | null;
  onRescan: () => void;
};

export function LibraryPage({
  items,
  roots,
  folder,
  loading,
  scanning,
  error,
  onRescan,
}: Props) {
  const [query, setQuery] = useState("");

  const tree = useMemo(() => buildFolderTree(items, roots), [items, roots]);
  const currentNode = useMemo(() => findFolder(tree, folder), [tree, folder]);

  const recent = useMemo(
    () =>
      [...items]
        .sort(
          (left, right) =>
            right.indexedAt.localeCompare(left.indexedAt) ||
            left.fileName.localeCompare(right.fileName),
        )
        .slice(0, 6),
    [items],
  );

  const normalizedQuery = query.trim().toLowerCase();
  const searching = normalizedQuery.length > 0;

  const searchResults = useMemo(() => {
    if (!searching) return [];

    return items
      .filter((item) =>
        [
          item.fileName,
          item.relativePath,
          item.containerName ?? "",
          item.videoCodec ?? "",
          item.audioCodec ?? "",
        ]
          .join(" ")
          .toLowerCase()
          .includes(normalizedQuery),
      )
      .sort((left, right) => left.fileName.localeCompare(right.fileName));
  }, [items, normalizedQuery, searching]);

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

      {folder !== null ? <Breadcrumb folder={folder} /> : null}

      {error ? <p className="error">{error}</p> : null}

      {searching ? (
        <SearchResults items={searchResults} totalItems={items.length} loading={loading} />
      ) : (
        <FolderView
          currentNode={currentNode}
          folder={folder}
          loading={loading}
          recent={folder === null ? recent : []}
        />
      )}
    </div>
  );
}

function Breadcrumb({ folder }: { folder: string }) {
  const segments = folderPathSegments(folder);

  return (
    <nav aria-label="Library location" className="breadcrumb">
      <button
        className="breadcrumb-link"
        onClick={() => navigate({ name: "library", folder: null })}
        type="button"
      >
        Library
      </button>
      {segments.map((segment, index) => {
        const path = segments.slice(0, index + 1).join("/");
        const isLast = index === segments.length - 1;
        return (
          <span className="breadcrumb-segment" key={path}>
            <span className="breadcrumb-separator" aria-hidden="true">
              /
            </span>
            {isLast ? (
              <span className="breadcrumb-current">{segment}</span>
            ) : (
              <button
                className="breadcrumb-link"
                onClick={() => navigate({ name: "library", folder: path })}
                type="button"
              >
                {segment}
              </button>
            )}
          </span>
        );
      })}
    </nav>
  );
}

function FolderView({
  currentNode,
  folder,
  loading,
  recent,
}: {
  currentNode: FolderNode | null;
  folder: string | null;
  loading: boolean;
  recent: MediaItem[];
}) {
  if (loading && !currentNode) {
    return <p className="muted">Loading library…</p>;
  }

  if (!currentNode) {
    return (
      <section className="library-section">
        <p className="muted">
          That folder no longer exists in the library. It may have been removed in the last
          rescan.
        </p>
      </section>
    );
  }

  const isRoot = folder === null;
  const hasChildren = currentNode.children.length > 0;
  const hasItems = currentNode.items.length > 0;
  const isEmpty = !hasChildren && !hasItems;

  return (
    <>
      {isRoot && recent.length > 0 ? (
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

      {hasChildren ? (
        <section className="library-section">
          <div className="library-section-head">
            <h2>{isRoot ? "Folders" : "Subfolders"}</h2>
            <span className="muted">
              {currentNode.children.length} folder{currentNode.children.length === 1 ? "" : "s"}
            </span>
          </div>
          <div className="poster-grid">
            {currentNode.children.map((child) => (
              <FolderCard key={child.path} node={child} />
            ))}
          </div>
        </section>
      ) : null}

      {hasItems ? (
        <section className="library-section">
          <div className="library-section-head">
            <h2>Titles</h2>
            <span className="muted">
              {currentNode.items.length} title{currentNode.items.length === 1 ? "" : "s"}
            </span>
          </div>
          <div className="poster-grid">
            {currentNode.items.map((item) => (
              <PosterCard key={item.id} item={item} />
            ))}
          </div>
        </section>
      ) : null}

      {isEmpty ? (
        <section className="library-section">
          <p className="muted">
            {isRoot
              ? "No media indexed yet. Add files to a library root and rescan."
              : "This folder is empty."}
          </p>
        </section>
      ) : null}
    </>
  );
}

function SearchResults({
  items,
  totalItems,
  loading,
}: {
  items: MediaItem[];
  totalItems: number;
  loading: boolean;
}) {
  return (
    <section className="library-section">
      <div className="library-section-head">
        <h2>Results</h2>
        <span className="muted">
          {items.length} of {totalItems}
        </span>
      </div>

      {loading ? (
        <p className="muted">Loading library…</p>
      ) : items.length === 0 ? (
        <p className="muted">No titles match the current search.</p>
      ) : (
        <div className="poster-grid">
          {items.map((item) => (
            <PosterCard key={item.id} item={item} />
          ))}
        </div>
      )}
    </section>
  );
}

function FolderCard({ node }: { node: FolderNode }) {
  return (
    <button
      className="poster-card poster-card-folder"
      onClick={() => navigate({ name: "library", folder: node.path })}
      type="button"
    >
      <div className="poster-art poster-art-folder">
        <FolderGlyph />
      </div>
      <div className="poster-meta">
        <h3 className="poster-title" title={node.name}>
          {node.name}
        </h3>
        <p className="poster-sub muted">
          {node.descendantCount} item{node.descendantCount === 1 ? "" : "s"}
        </p>
      </div>
    </button>
  );
}

function PosterCard({ item }: { item: MediaItem }) {
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
      className="poster-card"
      onClick={() => navigate({ name: "title", mediaId: item.id })}
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

function FolderGlyph() {
  return (
    <svg
      aria-hidden="true"
      className="folder-glyph"
      fill="none"
      stroke="currentColor"
      strokeLinecap="round"
      strokeLinejoin="round"
      strokeWidth={1.5}
      viewBox="0 0 24 24"
    >
      <path d="M3 7.2c0-1.12 0-1.68.218-2.108a2 2 0 0 1 .874-.874C4.52 4 5.08 4 6.2 4h2.379c.489 0 .733 0 .963.055.204.05.4.13.578.24.202.124.375.297.72.642L11.6 5.7c.345.345.518.518.72.642.179.11.374.19.578.24.23.055.474.055.963.055H17.8c1.12 0 1.68 0 2.108.218a2 2 0 0 1 .874.874C21 7.92 21 8.48 21 9.6v7.2c0 1.12 0 1.68-.218 2.108a2 2 0 0 1-.874.874C19.48 20 18.92 20 17.8 20H6.2c-1.12 0-1.68 0-2.108-.218a2 2 0 0 1-.874-.874C3 18.48 3 17.92 3 16.8V7.2Z" />
    </svg>
  );
}
