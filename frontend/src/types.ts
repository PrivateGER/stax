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
  progressRatio?: number | null;
  progressSpeed?: number | null;
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

export type MediaSummary = {
  id: string;
  rootPath: string;
  relativePath: string;
  fileName: string;
  extension: string | null;
  sizeBytes: number;
  indexedAt: string;
  durationSeconds: number | null;
  probeError: string | null;
  subtitleTrackCount: number;
  audioStreamCount: number;
  subtitleStreamCount: number;
  thumbnailGeneratedAt: string | null;
  thumbnailError: string | null;
  preparationState: PreparationState;
};

export type LibraryRoot = {
  path: string;
  lastScannedAt: string | null;
  lastScanError: string | null;
};

export type LibraryResponse = {
  revision: number;
  hasPendingBackgroundWork: boolean;
  roots: LibraryRoot[];
  items: MediaSummary[];
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

export type LibraryStatusResponse = {
  revision: number;
  hasPendingBackgroundWork: boolean;
};

export type Participant = {
  id: string;
  name: string;
  driftSeconds: number | null;
};

export type SnapshotEvent = {
  type: "snapshot";
  room: Room;
  connectionCount: number;
  participants: Participant[];
};

export type ParticipantsUpdatedEvent = {
  type: "participantsUpdated";
  roomId: string;
  participants: Participant[];
};

export type PlaybackUpdatedEvent = {
  type: "playbackUpdated";
  room: Room;
  actor: string;
  action: PlaybackAction;
};

export type MediaChangedEvent = {
  type: "mediaChanged";
  room: Room;
  actor: string;
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

export type PongEvent = { type: "pong"; clientSentAtMs: number };

export type SocketEvent =
  | SnapshotEvent
  | PlaybackUpdatedEvent
  | MediaChangedEvent
  | PresenceChangedEvent
  | ParticipantsUpdatedEvent
  | DriftCorrectionEvent
  | SocketErrorEvent
  | PongEvent;
