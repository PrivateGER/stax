import { useEffect, useRef, useState, type RefObject } from "react";

import { formatTimeCode } from "../../format";
import type { MediabunnyController, MediabunnyState } from "./MediabunnyController";
import { mapKey, type PlayerIntent } from "./keyboardShortcuts";

type Props = {
  controllerRef: RefObject<MediabunnyController | null>;
  state: MediabunnyState;
  containerRef: RefObject<HTMLDivElement | null>;
  /** Kept alive by the parent — pointer move / leave to show/hide. */
  controlsVisible: boolean;
  onInteract: () => void;
};

export function ControlsBar({
  controllerRef,
  state,
  containerRef,
  controlsVisible,
  onInteract,
}: Props) {
  const progressRef = useRef<HTMLDivElement | null>(null);
  const volumeRef = useRef<HTMLDivElement | null>(null);
  const [dragSeekFraction, setDragSeekFraction] = useState<number | null>(null);

  // Fullscreen state mirrored for the button icon.
  const [isFullscreen, setIsFullscreen] = useState(false);
  useEffect(() => {
    const handler = () => setIsFullscreen(document.fullscreenElement !== null);
    document.addEventListener("fullscreenchange", handler);
    return () => document.removeEventListener("fullscreenchange", handler);
  }, []);

  const toggleFullscreen = () => {
    const container = containerRef.current;
    if (!container) return;
    if (document.fullscreenElement) {
      void document.exitFullscreen();
    } else {
      container.requestFullscreen().catch((error) => {
        console.error("Failed to enter fullscreen mode:", error);
      });
    }
  };

  // Keyboard shortcuts while the surface is in the document.
  useEffect(() => {
    const applyIntent = (intent: PlayerIntent) => {
      const controller = controllerRef.current;
      if (!controller) return;
      onInteract();
      switch (intent.type) {
        case "togglePlay":
          if (controller.state.playing) controller.pause();
          else void controller.play();
          break;
        case "toggleFullscreen":
          toggleFullscreen();
          break;
        case "toggleMute":
          controller.toggleMute();
          break;
        case "relativeSeek":
          void controller.seek(controller.state.currentTime + intent.deltaSeconds);
          break;
        case "seekToFraction":
          void controller.seek(intent.fraction * controller.state.duration);
          break;
      }
    };

    const handler = (event: KeyboardEvent) => {
      const target = event.target as HTMLElement | null;
      // Don't swallow typing.
      if (target && (target.tagName === "INPUT" || target.tagName === "TEXTAREA" || target.isContentEditable)) {
        return;
      }
      const intent = mapKey(event);
      if (!intent) return;
      event.preventDefault();
      applyIntent(intent);
    };
    window.addEventListener("keydown", handler);
    return () => window.removeEventListener("keydown", handler);
  }, [controllerRef, containerRef, onInteract]);

  const handleTogglePlay = () => {
    const controller = controllerRef.current;
    if (!controller) return;
    if (state.playing) controller.pause();
    else void controller.play();
  };

  const handleProgressDown = (event: React.PointerEvent<HTMLDivElement>) => {
    if (!progressRef.current || !state.duration) return;
    const rect = progressRef.current.getBoundingClientRect();
    const fraction = clamp01((event.clientX - rect.left) / rect.width);
    setDragSeekFraction(fraction);
    progressRef.current.setPointerCapture(event.pointerId);
  };

  const handleProgressMove = (event: React.PointerEvent<HTMLDivElement>) => {
    if (dragSeekFraction === null || !progressRef.current) return;
    const rect = progressRef.current.getBoundingClientRect();
    const fraction = clamp01((event.clientX - rect.left) / rect.width);
    setDragSeekFraction(fraction);
  };

  const handleProgressUp = (event: React.PointerEvent<HTMLDivElement>) => {
    if (dragSeekFraction === null || !progressRef.current) return;
    progressRef.current.releasePointerCapture(event.pointerId);
    const controller = controllerRef.current;
    if (controller) {
      void controller.seek(dragSeekFraction * state.duration);
    }
    setDragSeekFraction(null);
  };

  const displayFraction =
    dragSeekFraction ??
    (state.duration > 0 ? clamp01(state.currentTime / state.duration) : 0);

  const handleVolumeDown = (event: React.PointerEvent<HTMLDivElement>) => {
    if (!volumeRef.current) return;
    volumeRef.current.setPointerCapture(event.pointerId);
    updateVolumeFromPointer(event);
  };

  const handleVolumeMove = (event: React.PointerEvent<HTMLDivElement>) => {
    if (!volumeRef.current?.hasPointerCapture(event.pointerId)) return;
    updateVolumeFromPointer(event);
  };

  const handleVolumeUp = (event: React.PointerEvent<HTMLDivElement>) => {
    if (!volumeRef.current) return;
    volumeRef.current.releasePointerCapture(event.pointerId);
  };

  const updateVolumeFromPointer = (event: React.PointerEvent<HTMLDivElement>) => {
    if (!volumeRef.current) return;
    const rect = volumeRef.current.getBoundingClientRect();
    const fraction = clamp01((event.clientX - rect.left) / rect.width);
    controllerRef.current?.setVolume(fraction);
  };

  const handleMute = () => controllerRef.current?.toggleMute();

  return (
    <div
      className={`mb-controls ${controlsVisible ? "visible" : ""}`}
      onClick={(event) => event.stopPropagation()}
      onPointerMove={onInteract}
    >
      <button
        aria-label={state.playing ? "Pause" : "Play"}
        className="mb-controls-button"
        onClick={handleTogglePlay}
        type="button"
      >
        {state.playing ? <PauseGlyph /> : <PlayGlyph />}
      </button>

      {state.hasAudio ? (
        <div className="mb-volume">
          <button
            aria-label={state.muted ? "Unmute" : "Mute"}
            className="mb-controls-button"
            onClick={handleMute}
            type="button"
          >
            {state.muted || state.volume === 0 ? <VolumeMutedGlyph /> : <VolumeGlyph />}
          </button>
          <div
            className="mb-volume-track"
            onPointerDown={handleVolumeDown}
            onPointerMove={handleVolumeMove}
            onPointerUp={handleVolumeUp}
            ref={volumeRef}
          >
            <div
              className="mb-volume-fill"
              style={{ width: `${(state.muted ? 0 : state.volume) * 100}%` }}
            />
          </div>
        </div>
      ) : null}

      <span className="mb-time">{formatTimeCode(state.currentTime)}</span>

      <div
        className="mb-progress"
        onPointerDown={handleProgressDown}
        onPointerMove={handleProgressMove}
        onPointerUp={handleProgressUp}
        ref={progressRef}
      >
        <div
          className="mb-progress-buffered"
          style={{ width: `${clamp01(state.bufferedFraction) * 100}%` }}
        />
        <div
          className="mb-progress-fill"
          style={{ width: `${displayFraction * 100}%` }}
        >
          <span className="mb-progress-handle" />
        </div>
      </div>

      <span className="mb-time">{formatTimeCode(state.duration)}</span>

      <button
        aria-label={isFullscreen ? "Exit fullscreen" : "Enter fullscreen"}
        className="mb-controls-button"
        onClick={toggleFullscreen}
        type="button"
      >
        {isFullscreen ? <FullscreenExitGlyph /> : <FullscreenGlyph />}
      </button>
    </div>
  );
}

function clamp01(x: number): number {
  if (!Number.isFinite(x)) return 0;
  return Math.max(0, Math.min(1, x));
}

function PlayGlyph() {
  return (
    <svg aria-hidden="true" viewBox="0 0 24 24" fill="currentColor">
      <path d="M7 4.5v15l13-7.5z" />
    </svg>
  );
}

function PauseGlyph() {
  return (
    <svg aria-hidden="true" viewBox="0 0 24 24" fill="currentColor">
      <rect x="6" y="4.5" width="4" height="15" rx="0.5" />
      <rect x="14" y="4.5" width="4" height="15" rx="0.5" />
    </svg>
  );
}

function VolumeGlyph() {
  return (
    <svg aria-hidden="true" viewBox="0 0 24 24" fill="currentColor">
      <path d="M4 10v4h4l5 4V6L8 10H4z" />
      <path d="M16.5 12a4.5 4.5 0 0 0-2.5-4v8a4.5 4.5 0 0 0 2.5-4z" />
    </svg>
  );
}

function VolumeMutedGlyph() {
  return (
    <svg aria-hidden="true" viewBox="0 0 24 24" fill="currentColor">
      <path d="M4 10v4h4l5 4V6L8 10H4z" />
      <path d="m16 8 5 5m0-5-5 5" stroke="currentColor" strokeWidth="1.8" fill="none" strokeLinecap="round" />
    </svg>
  );
}

function FullscreenGlyph() {
  return (
    <svg aria-hidden="true" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="2" strokeLinecap="round" strokeLinejoin="round">
      <path d="M4 9V5h4M20 9V5h-4M4 15v4h4M20 15v4h-4" />
    </svg>
  );
}

function FullscreenExitGlyph() {
  return (
    <svg aria-hidden="true" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="2" strokeLinecap="round" strokeLinejoin="round">
      <path d="M9 5v4H5M15 5v4h4M9 19v-4H5M15 19v-4h4" />
    </svg>
  );
}
