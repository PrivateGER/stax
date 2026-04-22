import { useEffect, useState } from "react";

export type Route =
  | { name: "library"; folder: string | null }
  | { name: "title"; mediaId: string }
  | { name: "watch"; mediaId: string; roomId: string | null }
  | { name: "admin" };

export function parseHash(hash: string): Route {
  const path = hash.replace(/^#\/?/, "");

  if (!path || path === "library") {
    return { name: "library", folder: null };
  }

  if (path === "admin") {
    return { name: "admin" };
  }

  const folderMatch = path.match(/^library\/folder\/(.+)$/);
  if (folderMatch) {
    return { name: "library", folder: decodeFolderPath(folderMatch[1]!) };
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

  return { name: "library", folder: null };
}

export function toHash(route: Route): string {
  switch (route.name) {
    case "library":
      return route.folder ? `#/library/folder/${encodeFolderPath(route.folder)}` : "#/";
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

function encodeFolderPath(folder: string): string {
  // Encode each segment independently so a literal "/" inside a folder name
  // never collides with the path separator we emit.
  return folder
    .split("/")
    .filter(Boolean)
    .map(encodeURIComponent)
    .join("/");
}

function decodeFolderPath(folder: string): string {
  return folder
    .split("/")
    .filter(Boolean)
    .map(decodeURIComponent)
    .join("/");
}
