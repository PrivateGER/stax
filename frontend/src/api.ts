import type {
  CreateStreamCopyRequest,
  HealthResponse,
  LibraryResponse,
  LibraryScanResponse,
  LibraryStatusResponse,
  MediaItem,
  Room,
  RoomsResponse,
  StreamCopySummary,
} from "./types";

type RequestOptions = {
  method?: "GET" | "POST";
  body?: unknown;
  fallbackMessage?: string;
};

async function request<T>(url: string, options: RequestOptions = {}): Promise<T> {
  const init: RequestInit = { method: options.method ?? "GET" };
  if (options.body !== undefined) {
    init.headers = { "Content-Type": "application/json" };
    init.body = JSON.stringify(options.body);
  }

  const response = await fetch(url, init);
  if (!response.ok) {
    const body = await response.json().catch(() => ({}) as { error?: string });
    throw new Error(
      body.error ??
        options.fallbackMessage ??
        `Request failed with status ${response.status}`,
    );
  }
  return response.json() as Promise<T>;
}

export const api = {
  health: () => request<HealthResponse>("/api/health"),
  library: () => request<LibraryResponse>("/api/library"),
  media: (mediaId: string) => request<MediaItem>(`/api/media/${mediaId}`),
  libraryStatus: () => request<LibraryStatusResponse>("/api/library/status"),
  rooms: () => request<RoomsResponse>("/api/rooms"),

  scan: () =>
    request<LibraryScanResponse>("/api/library/scan", {
      method: "POST",
      fallbackMessage: "Failed to scan library.",
    }),

  createRoom: (input: {
    name: string;
    mediaId: string | null;
    mediaTitle?: string | null;
  }) =>
    request<Room>("/api/rooms", {
      method: "POST",
      body: {
        name: input.name,
        mediaId: input.mediaId,
        mediaTitle: input.mediaTitle ?? null,
      },
      fallbackMessage: "Failed to create room.",
    }),

  getStreamCopy: async (mediaId: string): Promise<StreamCopySummary | null> => {
    const response = await fetch(`/api/media/${mediaId}/stream-copy`);
    if (response.status === 404) return null;
    if (!response.ok) {
      const body = await response.json().catch(() => ({}) as { error?: string });
      throw new Error(body.error ?? `Request failed with status ${response.status}`);
    }
    return response.json() as Promise<StreamCopySummary>;
  },

  createStreamCopy: (mediaId: string, input: CreateStreamCopyRequest) =>
    request<StreamCopySummary>(`/api/media/${mediaId}/stream-copy`, {
      method: "POST",
      body: input,
      fallbackMessage: "Failed to create stream copy.",
    }),
};

export function streamUrl(mediaId: string) {
  return `/api/media/${mediaId}/stream`;
}

export function subtitleUrl(mediaId: string, trackIndex: number) {
  return `/api/media/${mediaId}/subtitles/${trackIndex}`;
}

export function embeddedSubtitleUrl(mediaId: string, streamIndex: number) {
  return `/api/media/${mediaId}/subtitles/embedded/${streamIndex}`;
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
