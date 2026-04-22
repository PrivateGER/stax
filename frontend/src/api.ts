import type {
  CreateStreamCopyRequest,
  HealthResponse,
  LibraryResponse,
  LibraryScanResponse,
  Room,
  RoomsResponse,
  StreamCopySummary,
} from "./types";

async function getJson<T>(url: string): Promise<T> {
  const response = await fetch(url);

  if (!response.ok) {
    const body = await response.json().catch(() => ({}) as { error?: string });
    throw new Error(body.error ?? `Request failed with status ${response.status}`);
  }

  return response.json() as Promise<T>;
}

export const api = {
  health: () => getJson<HealthResponse>("/api/health"),
  library: () => getJson<LibraryResponse>("/api/library"),
  rooms: () => getJson<RoomsResponse>("/api/rooms"),

  scan: async (): Promise<LibraryScanResponse> => {
    const response = await fetch("/api/library/scan", { method: "POST" });

    if (!response.ok) {
      const body = await response.json().catch(() => ({}) as { error?: string });
      throw new Error(body.error ?? "Failed to scan library.");
    }

    return response.json();
  },

  createRoom: async (input: {
    name: string;
    mediaId: string | null;
    mediaTitle?: string | null;
  }): Promise<Room> => {
    const response = await fetch("/api/rooms", {
      method: "POST",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify({
        name: input.name,
        mediaId: input.mediaId,
        mediaTitle: input.mediaTitle ?? null,
      }),
    });

    if (!response.ok) {
      const body = await response.json().catch(() => ({}) as { error?: string });
      throw new Error(body.error ?? "Failed to create room.");
    }

    return response.json();
  },

  getStreamCopy: async (mediaId: string): Promise<StreamCopySummary | null> => {
    const response = await fetch(`/api/media/${mediaId}/stream-copy`);
    if (response.status === 404) return null;
    if (!response.ok) {
      const body = await response.json().catch(() => ({}) as { error?: string });
      throw new Error(body.error ?? `Request failed with status ${response.status}`);
    }
    return response.json() as Promise<StreamCopySummary>;
  },

  createStreamCopy: async (
    mediaId: string,
    input: CreateStreamCopyRequest,
  ): Promise<StreamCopySummary> => {
    const response = await fetch(`/api/media/${mediaId}/stream-copy`, {
      method: "POST",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify({
        audioStreamIndex: input.audioStreamIndex,
        subtitleMode: input.subtitleMode,
        subtitle: input.subtitle,
      }),
    });

    if (!response.ok) {
      const body = await response.json().catch(() => ({}) as { error?: string });
      throw new Error(body.error ?? "Failed to create stream copy.");
    }

    return response.json();
  },
};

export function streamUrl(mediaId: string) {
  return `/api/media/${mediaId}/stream`;
}

export function subtitleUrl(mediaId: string, trackIndex: number) {
  return `/api/media/${mediaId}/subtitles/${trackIndex}`;
}

export function thumbnailUrl(mediaId: string) {
  return `/api/media/${mediaId}/thumbnail`;
}

export function socketUrl(roomId: string, clientName: string) {
  const protocol = window.location.protocol === "https:" ? "wss" : "ws";
  const params = new URLSearchParams({
    clientName: clientName.trim() || "Browser Viewer",
  });

  return `${protocol}://${window.location.host}/api/rooms/${roomId}/ws?${params.toString()}`;
}
