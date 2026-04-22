import { useEffect, useRef, useState } from "react";

import { api } from "../api";
import { displayMediaTitle } from "../format";
import { SessionPanel } from "../player/SessionPanel";
import { TracksMenu } from "../player/TracksMenu";
import { UnplayableNotice } from "../player/UnplayableNotice";
import { deriveSubtitleSources } from "../player/subtitleSources";
import { useAudioTracks } from "../player/useAudioTracks";
import { useRoomSync } from "../player/useRoomSync";
import { navigate } from "../router";
import type { MediaItem, Room } from "../types";
import { useRoomSocket } from "../useRoomSocket";
import { usePlayerSource } from "../usePlayerSource";
import { useStreamCopyProgress } from "../useStreamCopyProgress";
import { useWatchTogether } from "../useWatchTogether";

type Props = {
  item: MediaItem | null;
  roomId: string | null;
  rooms: Room[];
  clientName: string;
  onClientNameChange: (name: string) => void;
  onRefresh: () => void;
  onRoomCreated: (room: Room) => void;
};

export function PlayerPage({
  item,
  roomId,
  rooms,
  clientName,
  onClientNameChange,
  onRefresh,
  onRoomCreated,
}: Props) {
  const videoRef = useRef<HTMLVideoElement | null>(null);
  const [selectedSubtitleIndex, setSelectedSubtitleIndex] = useState<number | null>(null);
  const [playerError, setPlayerError] = useState<string | null>(null);
  const [creatingStreamCopy, setCreatingStreamCopy] = useState(false);
  const [streamCopyError, setStreamCopyError] = useState<string | null>(null);
  const [showSessionPanel, setShowSessionPanel] = useState<boolean>(Boolean(roomId));
  const [prevItemId, setPrevItemId] = useState<string | null>(item?.id ?? null);
  const [prevRoomId, setPrevRoomId] = useState<string | null>(roomId);

  const currentItemId = item?.id ?? null;
  if (currentItemId !== prevItemId) {
    setPrevItemId(currentItemId);
    setSelectedSubtitleIndex(null);
    setPlayerError(null);
    setStreamCopyError(null);
  }
  if (roomId !== prevRoomId) {
    setPrevRoomId(roomId);
    setShowSessionPanel(Boolean(roomId));
  }

  const { summary: liveStreamCopy, seedFromCreate: seedLiveStreamCopy } =
    useStreamCopyProgress({
      mediaId: item?.id ?? null,
      fallback: item?.streamCopy ?? null,
      onRefresh,
    });

  const socket = useRoomSocket(roomId, clientName);
  const watchTogether = useWatchTogether(item, onRoomCreated);
  const audio = useAudioTracks(videoRef, item);
  const { clockTickMs, catchUp } = useRoomSync({
    videoRef,
    socket,
    item,
    onAutoplayBlocked: setPlayerError,
  });

  usePlayerSource(videoRef, item);
  const playable =
    item?.preparationState === "direct" || item?.preparationState === "prepared";
  const subtitleSources = item ? deriveSubtitleSources(item) : [];

  // If the room is anchored to a different media item than what's in the URL,
  // follow the room to its canonical media. This is what makes shared
  // Watch Together links robust.
  useEffect(() => {
    if (!roomId || !socket.room) return;
    const roomMediaId = socket.room.mediaId;
    if (!roomMediaId) return;
    if (item && roomMediaId === item.id) return;
    navigate({ name: "watch", mediaId: roomMediaId, roomId });
  }, [item, roomId, socket.room]);

  useEffect(() => {
    const video = videoRef.current;
    if (!video) return;

    for (let index = 0; index < video.textTracks.length; index += 1) {
      const track = video.textTracks[index]!;
      track.mode = selectedSubtitleIndex === index ? "showing" : "disabled";
    }
  }, [item, selectedSubtitleIndex]);

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
            audioTracks={audio.tracks}
            onSelectAudio={audio.select}
            onSelectSubtitle={setSelectedSubtitleIndex}
            selectedAudioId={audio.selectedId}
            selectedSubtitleIndex={selectedSubtitleIndex}
            subtitleSources={subtitleSources}
          />

          {roomId ? (
            <button
              className="ghost-button"
              onClick={() => setShowSessionPanel((visible) => !visible)}
              type="button"
            >
              {showSessionPanel ? "Hide session" : "Show session"}
            </button>
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

      <div className={`player-layout ${roomId && showSessionPanel ? "with-session" : ""}`}>
        <div className="player-stage">
          <video
            autoPlay={!roomId}
            className="player-video"
            controls
            key={item.id}
            onError={() => setPlayerError("The browser could not load this file.")}
            playsInline
            preload="metadata"
            ref={videoRef}
          >
            {subtitleSources.map((track) => (
              <track
                key={track.key}
                kind="subtitles"
                label={track.label}
                src={track.src}
                srcLang={track.language}
              />
            ))}
          </video>
        </div>

        {roomId && showSessionPanel ? (
          <SessionPanel
            clientName={clientName}
            clockTickMs={clockTickMs}
            onCatchUp={catchUp}
            onClientNameChange={onClientNameChange}
            onLeave={() => navigate({ name: "watch", mediaId: item.id, roomId: null })}
            rooms={rooms}
            socket={socket}
          />
        ) : null}
      </div>
    </div>
  );
}
