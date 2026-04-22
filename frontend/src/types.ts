export type PlaybackStatus = "playing" | "paused";
export type PlaybackAction = "play" | "pause" | "seek";
export type ConnectionState = "offline" | "connecting" | "live" | "error";
export type DriftCorrectionAction = "inSync" | "nudge" | "seek";

export type PlaybackState = {
  status: PlaybackStatus;
  positionSeconds: number;
  anchorPositionSeconds: number;
  clockUpdatedAt: string;
  emittedAt: string;
  playbackRate: number;
  driftToleranceSeconds: number;
};

export type Room = {
  id: string;
  name: string;
  mediaId: string | null;
  mediaTitle: string | null;
  createdAt: string;
  playbackState: PlaybackState;
};

export type SubtitleTrack = {
  fileName: string;
  relativePath: string;
  extension: string;
  label: string;
  language: string | null;
};

export type PlaybackMode =
  | "direct"
  | "needsPreparation"
  | "unsupported";

export type PreparationState =
  | "direct"
  | "needsPreparation"
  | "preparing"
  | "prepared"
  | "failed"
  | "unsupported";

export type StreamCopyStatus = "queued" | "running" | "ready" | "failed";
export type SubtitleMode = "off" | "sidecar" | "burned";
export type SubtitleSourceKind = "sidecar" | "embedded";

export type StreamCopySubtitleSelection = {
  kind: SubtitleSourceKind;
  index: number;
};

export type StreamCopySummary = {
  status: StreamCopyStatus;
  audioStreamIndex: number | null;
  subtitleMode: SubtitleMode;
  subtitle: StreamCopySubtitleSelection | null;
  subtitleUrl: string | null;
  error: string | null;
  updatedAt: string;
};

export type CreateStreamCopyRequest = {
  audioStreamIndex: number | null;
  subtitleMode: SubtitleMode;
  subtitle: StreamCopySubtitleSelection | null;
};

export type AudioStream = {
  index: number;
  codec: string | null;
  channels: number | null;
  channelLayout: string | null;
  language: string | null;
  title: string | null;
  default: boolean;
};

export type SubtitleStream = {
  index: number;
  codec: string | null;
  language: string | null;
  title: string | null;
  default: boolean;
  forced: boolean;
};

export type MediaItem = {
  id: string;
  rootPath: string;
  relativePath: string;
  fileName: string;
  extension: string | null;
  sizeBytes: number;
  modifiedAt: string;
  indexedAt: string;
  contentType: string | null;
  durationSeconds: number | null;
  containerName: string | null;
  videoCodec: string | null;
  audioCodec: string | null;
  width: number | null;
  height: number | null;
  probedAt: string | null;
  probeError: string | null;
  subtitleTracks: SubtitleTrack[];
  thumbnailGeneratedAt: string | null;
  thumbnailError: string | null;
  playbackMode: PlaybackMode;
  preparationState: PreparationState;
  videoProfile: string | null;
  videoLevel: number | null;
  videoPixFmt: string | null;
  videoBitDepth: number | null;
  audioStreams: AudioStream[];
  subtitleStreams: SubtitleStream[];
  streamCopy: StreamCopySummary | null;
};

export type LibraryRoot = {
  path: string;
  lastScannedAt: string | null;
  lastScanError: string | null;
};

export type LibraryResponse = {
  roots: LibraryRoot[];
  items: MediaItem[];
};

export type HealthResponse = {
  status: string;
  service: string;
  version: string;
};

export type RoomsResponse = { rooms: Room[] };

export type LibraryScanResponse = LibraryResponse & {
  scannedRootCount: number;
  indexedItemCount: number;
  scannedAt: string;
};

export type SnapshotEvent = {
  type: "snapshot";
  room: Room;
  connectionCount: number;
};

export type PlaybackUpdatedEvent = {
  type: "playbackUpdated";
  room: Room;
  actor: string;
  action: PlaybackAction;
};

export type PresenceChangedEvent = {
  type: "presenceChanged";
  roomId: string;
  connectionCount: number;
  actor: string;
  joined: boolean;
};

export type DriftCorrectionEvent = {
  type: "driftCorrection";
  roomId: string;
  actor: string;
  reportedPositionSeconds: number;
  expectedPositionSeconds: number;
  deltaSeconds: number;
  toleranceSeconds: number;
  suggestedAction: DriftCorrectionAction;
  measuredAt: string;
};

export type SocketErrorEvent = { type: "error"; message: string };

export type SocketEvent =
  | SnapshotEvent
  | PlaybackUpdatedEvent
  | PresenceChangedEvent
  | DriftCorrectionEvent
  | SocketErrorEvent;
