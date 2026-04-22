import { useCallback, useEffect, useState } from "react";

import { api } from "./api";
import { AdminPage } from "./pages/AdminPage";
import { LibraryPage } from "./pages/LibraryPage";
import { PlayerPage } from "./pages/PlayerPage";
import { TitlePage } from "./pages/TitlePage";
import { randomName } from "./randomName";
import { navigate, toHash, useRoute } from "./router";
import type {
  HealthResponse,
  LibraryRoot,
  MediaItem,
  Room,
} from "./types";

const CLIENT_NAME_KEY = "syncplay.clientName";

export default function App() {
  const route = useRoute();
  const [health, setHealth] = useState<HealthResponse | null>(null);
  const [roots, setRoots] = useState<LibraryRoot[]>([]);
  const [items, setItems] = useState<MediaItem[]>([]);
  const [rooms, setRooms] = useState<Room[]>([]);
  const [loading, setLoading] = useState(true);
  const [scanning, setScanning] = useState(false);
  const [libraryError, setLibraryError] = useState<string | null>(null);
  const [clientName, setClientNameState] = useState<string>(() => {
    if (typeof window === "undefined") return randomName();
    const stored = window.localStorage.getItem(CLIENT_NAME_KEY);
    if (stored && stored.trim().length > 0) return stored;
    const fresh = randomName();
    window.localStorage.setItem(CLIENT_NAME_KEY, fresh);
    return fresh;
  });

  const setClientName = useCallback((name: string) => {
    setClientNameState(name);
    if (typeof window !== "undefined") {
      window.localStorage.setItem(CLIENT_NAME_KEY, name);
    }
  }, []);

  const refresh = useCallback(async () => {
    setLoading(true);
    setLibraryError(null);

    try {
      const [healthResponse, libraryResponse, roomsResponse] = await Promise.all([
        api.health(),
        api.library(),
        api.rooms(),
      ]);

      setHealth(healthResponse);
      setRoots(libraryResponse.roots);
      setItems(libraryResponse.items);
      setRooms(roomsResponse.rooms);
    } catch (error) {
      setLibraryError(
        error instanceof Error ? error.message : "Could not load the library.",
      );
    } finally {
      setLoading(false);
    }
  }, []);

  useEffect(() => {
    void refresh();
  }, [refresh]);

  // The backend now runs the library scan in three stages: walk → probe
  // → thumbnail. The walk returns synchronously with NULL probe and
  // thumbnail columns for new/changed files; background pools fill them
  // in afterwards. Poll until *both* stages have completed for every
  // item, so freshly-probed metadata and freshly-generated thumbnails
  // appear without a manual refresh. Stops itself once nothing is
  // pending.
  const hasPendingBackgroundWork = items.some(
    (item) =>
      (item.probedAt === null && item.probeError === null) ||
      (item.thumbnailGeneratedAt === null && item.thumbnailError === null) ||
      item.preparationState === "preparing",
  );

  useEffect(() => {
    if (!hasPendingBackgroundWork) return;

    const interval = window.setInterval(async () => {
      try {
        const libraryResponse = await api.library();
        setRoots(libraryResponse.roots);
        setItems(libraryResponse.items);
      } catch {
        // Ignore transient polling errors — the next tick will retry, and
        // a real outage is already surfaced by the initial refresh path.
      }
    }, 10_000);

    return () => window.clearInterval(interval);
  }, [hasPendingBackgroundWork]);

  const handleRescan = useCallback(async () => {
    try {
      setScanning(true);
      setLibraryError(null);
      const payload = await api.scan();
      setRoots(payload.roots);
      setItems(payload.items);
    } catch (error) {
      setLibraryError(
        error instanceof Error ? error.message : "Could not scan the library.",
      );
    } finally {
      setScanning(false);
    }
  }, []);

  const handleRoomCreated = useCallback((room: Room) => {
    setRooms((existing) => {
      const filtered = existing.filter((entry) => entry.id !== room.id);
      return [room, ...filtered].sort((a, b) => a.name.localeCompare(b.name));
    });
  }, []);

  const findItem = (mediaId: string) =>
    items.find((item) => item.id === mediaId) ?? null;

  return (
    <div className="app">
      <nav className="top-nav">
        <a
          className="top-nav-brand"
          href={toHash({ name: "library", folder: null })}
          onClick={(event) => {
            event.preventDefault();
            navigate({ name: "library", folder: null });
          }}
        >
          Syncplay
        </a>

        <div className="top-nav-links">
          <NavLink
            active={route.name === "library" || route.name === "title"}
            to={{ name: "library", folder: null }}
          >
            Library
          </NavLink>
          <NavLink active={route.name === "admin"} to={{ name: "admin" }}>
            Admin
          </NavLink>
        </div>

        <div className="top-nav-status">
          {rooms.length > 0 ? (
            <span className="session-count">
              {rooms.length} Watch Together session{rooms.length === 1 ? "" : "s"}
            </span>
          ) : null}
        </div>
      </nav>

      <main className="app-main">
        {route.name === "library" ? (
          <LibraryPage
            error={libraryError}
            folder={route.folder}
            items={items}
            loading={loading}
            onRescan={() => void handleRescan()}
            roots={roots}
            scanning={scanning}
          />
        ) : null}

        {route.name === "title" ? (
          <TitlePage
            item={findItem(route.mediaId)}
            onRoomCreated={handleRoomCreated}
            onRefresh={() => void refresh()}
            rooms={rooms}
          />
        ) : null}

        {route.name === "watch" ? (
          <PlayerPage
            clientName={clientName}
            item={findItem(route.mediaId)}
            onClientNameChange={setClientName}
            onRefresh={() => void refresh()}
            onRoomCreated={handleRoomCreated}
            roomId={route.roomId}
          />
        ) : null}

        {route.name === "admin" ? (
          <AdminPage
            health={health}
            items={items}
            onRescan={() => void handleRescan()}
            rooms={rooms}
            roots={roots}
            scanning={scanning}
          />
        ) : null}
      </main>
    </div>
  );
}

function NavLink({
  active,
  to,
  children,
}: {
  active: boolean;
  to: Parameters<typeof toHash>[0];
  children: React.ReactNode;
}) {
  return (
    <a
      className={`top-nav-link ${active ? "active" : ""}`}
      href={toHash(to)}
      onClick={(event) => {
        event.preventDefault();
        navigate(to);
      }}
    >
      {children}
    </a>
  );
}
