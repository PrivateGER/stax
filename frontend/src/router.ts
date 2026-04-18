import { useEffect, useState } from "react";

export type Route =
  | { name: "library" }
  | { name: "title"; mediaId: string }
  | { name: "watch"; mediaId: string; roomId: string | null }
  | { name: "admin" };

export function parseHash(hash: string): Route {
  const path = hash.replace(/^#\/?/, "");

  if (!path || path === "library") {
    return { name: "library" };
  }

  if (path === "admin") {
    return { name: "admin" };
  }

  const titleMatch = path.match(/^title\/([^/]+)$/);
  if (titleMatch) {
    return { name: "title", mediaId: decodeURIComponent(titleMatch[1]!) };
  }

  const watchMatch = path.match(/^watch\/([^/]+)(?:\/room\/([^/]+))?$/);
  if (watchMatch) {
    return {
      name: "watch",
      mediaId: decodeURIComponent(watchMatch[1]!),
      roomId: watchMatch[2] ? decodeURIComponent(watchMatch[2]) : null,
    };
  }

  return { name: "library" };
}

export function toHash(route: Route): string {
  switch (route.name) {
    case "library":
      return "#/";
    case "title":
      return `#/title/${encodeURIComponent(route.mediaId)}`;
    case "watch":
      return route.roomId
        ? `#/watch/${encodeURIComponent(route.mediaId)}/room/${encodeURIComponent(route.roomId)}`
        : `#/watch/${encodeURIComponent(route.mediaId)}`;
    case "admin":
      return "#/admin";
  }
}

export function navigate(route: Route) {
  const nextHash = toHash(route);

  if (window.location.hash !== nextHash) {
    window.location.hash = nextHash;
  }
}

export function useRoute(): Route {
  const [route, setRoute] = useState<Route>(() => parseHash(window.location.hash));

  useEffect(() => {
    const onChange = () => setRoute(parseHash(window.location.hash));
    window.addEventListener("hashchange", onChange);
    return () => window.removeEventListener("hashchange", onChange);
  }, []);

  return route;
}
