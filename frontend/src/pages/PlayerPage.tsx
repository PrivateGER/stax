import { useEffect, useState } from "react";

import { api } from "../api";
import { displayMediaTitle } from "../format";
import { PlayerSurface } from "../player/mediabunny/PlayerSurface";
import { useMediabunnyController } from "../player/mediabunny/useMediabunnyController";
import { SessionMenu } from "../player/SessionMenu";
import { TracksMenu } from "../player/TracksMenu";
import { deriveSubtitleSources } from "../player/subtitleSources";
import { UnplayableNotice } from "../player/UnplayableNotice";
import { useActiveSubtitleCue } from "../player/useActiveSubtitleCue";
import { navigate } from "../router";
import type { MediaItem, Room } from "../types";
import { useRoomSocket } from "../useRoomSocket";
import { useStreamCopyProgress } from "../useStreamCopyProgress";
import { useWatchTogether } from "../useWatchTogether";

type Props = {
  item: MediaItem | null;
  roomId: string | null;
  clientName: string;
  onClientNameChange: (name: string) => void;
  onRefresh: () => void;
  onRoomCreated: (room: Room) => void;
};

export function PlayerPage({
  item,
  roomId,
  clientName,
  onClientNameChange,
  onRefresh,
  onRoomCreated,
}: Props) {
  const [playerError, setPlayerError] = useState<string | null>(null);
  const [creatingStreamCopy, setCreatingStreamCopy] = useState(false);
  const [streamCopyError, setStreamCopyError] = useState<string | null>(null);
  const [selectedSubtitleIndex, setSelectedSubtitleIndex] = useState<number | null>(null);
  const [prevItemId, setPrevItemId] = useState<string | null>(item?.id ?? null);
  const subtitleSources = item ? deriveSubtitleSources(item) : [];

  const currentItemId = item?.id ?? null;
  if (currentItemId !== prevItemId) {
    setPrevItemId(currentItemId);
    setPlayerError(null);
    setStreamCopyError(null);
    setSelectedSubtitleIndex(null);
  }

  if (selectedSubtitleIndex !== null && !subtitleSources[selectedSubtitleIndex]) {
    setSelectedSubtitleIndex(null);
  }

  const { summary: liveStreamCopy, seedFromCreate: seedLiveStreamCopy } =
    useStreamCopyProgress({
      mediaId: item?.id ?? null,
      fallback: item?.streamCopy ?? null,
      onRefresh,
    });

  const socket = useRoomSocket(roomId, clientName);
  const watchTogether = useWatchTogether(item, onRoomCreated);

  const { controllerRef, canvasRef, state } = useMediabunnyController(
    item,
    setPlayerError,
  );
  const selectedSubtitleSource =
    selectedSubtitleIndex === null ? null : subtitleSources[selectedSubtitleIndex] ?? null;
  const { activeCues, error: subtitleError } = useActiveSubtitleCue(
    selectedSubtitleSource,
    state.currentTime,
  );

  // If the room is anchored to a different media item than what's in the URL,
  // follow the room to its canonical media. Keeps shared Watch Together links
  // robust even after the host switches titles.
  useEffect(() => {
    if (!roomId || !socket.room) return;
    const roomMediaId = socket.room.mediaId;
    if (!roomMediaId) return;
    if (item && roomMediaId === item.id) return;
    navigate({ name: "watch", mediaId: roomMediaId, roomId });
  }, [item, roomId, socket.room]);

  async function handleCreateStreamCopy() {
    if (!item) return;

    try {
      setCreatingStreamCopy(true);
      setStreamCopyError(null);
      const response = await api.createStreamCopy(item.id, {
        audioStreamIndex:
          item.audioStreams.find((stream) => stream.default)?.index ??
          item.audioStreams[0]?.index ??
          null,
        subtitleMode: "off",
        subtitle: null,
      });
      seedLiveStreamCopy(response);
      onRefresh();
    } catch (error) {
      setStreamCopyError(
        error instanceof Error ? error.message : "Could not create the stream copy.",
      );
    } finally {
      setCreatingStreamCopy(false);
    }
  }

  if (!item) {
    return (
      <section className="title-missing">
        <h1>No media selected</h1>
        <button
          className="primary-button"
          onClick={() => navigate({ name: "library", folder: null })}
          type="button"
        >
          Back to library
        </button>
      </section>
    );
  }

  const title = displayMediaTitle(item);
  const playable =
    item.preparationState === "direct" || item.preparationState === "prepared";

  if (!playable) {
    return (
      <UnplayableNotice
        creatingStreamCopy={creatingStreamCopy}
        item={item}
        liveStreamCopy={liveStreamCopy}
        onCreateStreamCopy={() => void handleCreateStreamCopy()}
        streamCopyError={streamCopyError}
        title={title}
      />
    );
  }

  return (
    <div className="player-page">
      <header className="player-bar">
        <button
          className="link-button"
          onClick={() => navigate({ name: "title", mediaId: item.id })}
          type="button"
        >
          ← {title}
        </button>

        <div className="player-bar-right">
          <TracksMenu
            audioTracks={state.audioTracks}
            onSelectAudio={(trackId) => {
              void controllerRef.current?.selectAudioTrack(trackId);
            }}
            onSelectSubtitle={setSelectedSubtitleIndex}
            selectedAudioId={state.selectedAudioTrackId}
            selectedSubtitleIndex={selectedSubtitleIndex}
            subtitleSources={subtitleSources}
          />

          {roomId ? (
            <SessionMenu
              clientName={clientName}
              onClientNameChange={onClientNameChange}
              onLeave={() => navigate({ name: "watch", mediaId: item.id, roomId: null })}
              socket={socket}
            />
          ) : (
            <button
              className="primary-button"
              disabled={watchTogether.creating}
              onClick={() => void watchTogether.start()}
              type="button"
            >
              {watchTogether.creating ? "Starting…" : "Watch Together"}
            </button>
          )}
        </div>
      </header>

      {watchTogether.error ? (
        <p className="error player-error">{watchTogether.error}</p>
      ) : null}
      {playerError ? <p className="error player-error">{playerError}</p> : null}
      {subtitleError ? <p className="error player-error">{subtitleError}</p> : null}
      {state.warning ? (
        <p className="error player-error">{state.warning}</p>
      ) : null}
      {socket.error ? (
        <p className="error player-error">{socket.error}</p>
      ) : null}

      <div className="player-stage" key={item.id}>
        <PlayerSurface
          canvasRef={canvasRef}
          controllerRef={controllerRef}
          state={state}
          subtitleCues={activeCues}
        />
      </div>
    </div>
  );
}
