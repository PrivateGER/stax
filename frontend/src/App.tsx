import { useCallback, useEffect, useMemo, useState } from "react";

import shimakazeUrl from "../asset/shimakaze_sfw.png";
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
  MediaSummary,
  LibraryResponse,
  Room,
} from "./types";

const CLIENT_NAME_KEY = "stax.clientName";
const ACTIVE_SESSION_KEY = "stax.activeSessionRoomId";

export default function App() {
  const route = useRoute();
  const [health, setHealth] = useState<HealthResponse | null>(null);
  const [roots, setRoots] = useState<LibraryRoot[]>([]);
  const [items, setItems] = useState<MediaSummary[]>([]);
  const [itemDetails, setItemDetails] = useState<Map<string, MediaItem>>(() => new Map());
  const [rooms, setRooms] = useState<Room[]>([]);
  const [loading, setLoading] = useState(true);
  const [scanning, setScanning] = useState(false);
  const [libraryError, setLibraryError] = useState<string | null>(null);
  const [libraryRevision, setLibraryRevision] = useState(0);
  const [hasPendingBackgroundWork, setHasPendingBackgroundWork] = useState(false);
  const [clientName, setClientNameState] = useState<string>(() => {
    if (typeof window === "undefined") return randomName();
    const stored = window.localStorage.getItem(CLIENT_NAME_KEY);
    if (stored && stored.trim().length > 0) return stored;
    const fresh = randomName();
    window.localStorage.setItem(CLIENT_NAME_KEY, fresh);
    return fresh;
  });
  const [activeRoomId, setActiveRoomId] = useState<string | null>(() => {
    if (typeof window === "undefined") return null;
    return window.localStorage.getItem(ACTIVE_SESSION_KEY);
  });

  const setClientName = useCallback((name: string) => {
    setClientNameState(name);
    if (typeof window !== "undefined") {
      window.localStorage.setItem(CLIENT_NAME_KEY, name);
    }
  }, []);

  const clearActiveSession = useCallback(() => {
    setActiveRoomId(null);
    if (typeof window !== "undefined") {
      window.localStorage.removeItem(ACTIVE_SESSION_KEY);
    }
  }, []);

  const applyLibraryResponse = useCallback((libraryResponse: LibraryResponse) => {
    setLibraryRevision(libraryResponse.revision);
    setHasPendingBackgroundWork(libraryResponse.hasPendingBackgroundWork);
    setRoots(libraryResponse.roots);
    setItems(libraryResponse.items);
    setItemDetails(new Map());
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
      applyLibraryResponse(libraryResponse);
      setRooms(roomsResponse.rooms);
    } catch (error) {
      setLibraryError(
        error instanceof Error ? error.message : "Could not load the library.",
      );
    } finally {
      setLoading(false);
    }
  }, [applyLibraryResponse]);

  useEffect(() => {
    void refresh();
  }, [refresh]);

  useEffect(() => {
    if (route.name !== "watch" || !route.roomId) return;
    setActiveRoomId(route.roomId);
    if (typeof window !== "undefined") {
      window.localStorage.setItem(ACTIVE_SESSION_KEY, route.roomId);
    }
  }, [route]);

  const shouldPollRooms = route.name === "library" || route.name === "title";

  useEffect(() => {
    if (!shouldPollRooms) return;

    let active = true;
    const tick = async () => {
      if (typeof document !== "undefined" && document.visibilityState !== "visible") {
        return;
      }
      try {
        const response = await api.rooms();
        if (!active) return;
        setRooms(response.rooms);
      } catch {
        // Transient — try again next tick.
      }
    };

    const interval = window.setInterval(() => {
      void tick();
    }, 10_000);
    const handleVisibilityChange = () => {
      if (document.visibilityState === "visible") {
        void tick();
      }
    };
    document.addEventListener("visibilitychange", handleVisibilityChange);

    return () => {
      active = false;
      window.clearInterval(interval);
      document.removeEventListener("visibilitychange", handleVisibilityChange);
    };
  }, [shouldPollRooms]);

  const shouldPollLibraryStatus = route.name === "library" && hasPendingBackgroundWork;

  useEffect(() => {
    if (!shouldPollLibraryStatus) return;

    let active = true;

    const pollStatus = async () => {
      if (typeof document !== "undefined" && document.visibilityState !== "visible") {
        return;
      }

      try {
        const status = await api.libraryStatus();
        if (!active) return;

        if (status.revision !== libraryRevision) {
          const libraryResponse = await api.library();
          if (!active) return;
          applyLibraryResponse(libraryResponse);
          return;
        }

        if (!status.hasPendingBackgroundWork) {
          setHasPendingBackgroundWork(false);
        }
      } catch {
        // Ignore transient polling errors — the next tick will retry, and
        // a real outage is already surfaced by the initial refresh path.
      }
    };

    const interval = window.setInterval(() => {
      void pollStatus();
    }, 10_000);

    const handleVisibilityChange = () => {
      if (document.visibilityState === "visible") {
        void pollStatus();
      }
    };
    document.addEventListener("visibilitychange", handleVisibilityChange);

    return () => {
      active = false;
      window.clearInterval(interval);
      document.removeEventListener("visibilitychange", handleVisibilityChange);
    };
  }, [applyLibraryResponse, libraryRevision, shouldPollLibraryStatus]);

  const handleRescan = useCallback(async () => {
    try {
      setScanning(true);
      setLibraryError(null);
      const payload = await api.scan();
      applyLibraryResponse(payload);
    } catch (error) {
      setLibraryError(
        error instanceof Error ? error.message : "Could not scan the library.",
      );
    } finally {
      setScanning(false);
    }
  }, [applyLibraryResponse]);

  const handleRoomCreated = useCallback((room: Room) => {
    setRooms((existing) => {
      const filtered = existing.filter((entry) => entry.id !== room.id);
      return [room, ...filtered].sort((a, b) => a.name.localeCompare(b.name));
    });
  }, []);

  const itemsById = useMemo(() => {
    const map = new Map<string, MediaSummary>();
    for (const item of items) map.set(item.id, item);
    return map;
  }, [items]);
  const selectedMediaId =
    route.name === "title" || route.name === "watch" ? route.mediaId : null;
  const selectedItem = selectedMediaId
    ? (itemDetails.get(selectedMediaId) ?? null)
    : null;
  const selectedSummary = selectedMediaId
    ? (itemsById.get(selectedMediaId) ?? null)
    : null;

  useEffect(() => {
    if (!selectedMediaId) return;

    let active = true;
    const loadMedia = async () => {
      try {
        const media = await api.media(selectedMediaId);
        if (!active) return;
        setItemDetails((existing) => {
          const next = new Map(existing);
          next.set(media.id, media);
          return next;
        });
      } catch {
        if (!active) return;
        setItemDetails((existing) => {
          const next = new Map(existing);
          next.delete(selectedMediaId);
          return next;
        });
      }
    };

    void loadMedia();

    return () => {
      active = false;
    };
  }, [libraryRevision, selectedMediaId]);

  const activeSession = useMemo(() => {
    if (!activeRoomId) return null;
    const room = rooms.find((candidate) => candidate.id === activeRoomId);
    if (!room || !room.mediaId) return null;
    return { roomId: room.id, mediaId: room.mediaId, name: room.name };
  }, [activeRoomId, rooms]);
  const showSessionChip = activeSession !== null && route.name !== "watch";

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
          <img
            alt=""
            aria-hidden="true"
            className="top-nav-mascot"
            src={shimakazeUrl}
          />
          <span>Stax</span>
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
          {showSessionChip && activeSession ? (
            <a
              className="session-pill"
              href={toHash({
                name: "watch",
                mediaId: activeSession.mediaId,
                roomId: activeSession.roomId,
              })}
              onClick={(event) => {
                event.preventDefault();
                navigate({
                  name: "watch",
                  mediaId: activeSession.mediaId,
                  roomId: activeSession.roomId,
                });
              }}
              title={`Back to ${activeSession.name}`}
            >
              <span className="session-pill-dot live" aria-hidden="true" />
              <span>Back to {activeSession.name}</span>
            </a>
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
            rooms={rooms}
            roots={roots}
            scanning={scanning}
          />
        ) : null}

        {route.name === "title" ? (
          (loading || selectedSummary) && !selectedItem ? (
            <p className="muted">Loading title…</p>
          ) : (
            <TitlePage
              item={selectedItem}
              onRoomCreated={handleRoomCreated}
              onRefresh={() => void refresh()}
              rooms={rooms}
            />
          )
        ) : null}

        {route.name === "watch" ? (
          (loading || selectedSummary) && !selectedItem ? (
            <p className="muted">Loading player…</p>
          ) : (
            <PlayerPage
              clientName={clientName}
              item={selectedItem}
              items={items}
              onClientNameChange={setClientName}
              onLeaveSession={clearActiveSession}
              onRefresh={() => void refresh()}
              onRoomCreated={handleRoomCreated}
              roomId={route.roomId}
            />
          )
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
