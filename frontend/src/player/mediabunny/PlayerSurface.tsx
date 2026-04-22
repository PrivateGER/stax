import { useCallback, useEffect, useRef, useState, type RefObject } from "react";

import { ControlsBar } from "./ControlsBar";
import type { MediabunnyController, MediabunnyState } from "./MediabunnyController";

type Props = {
  canvasRef: RefObject<HTMLCanvasElement | null>;
  controllerRef: RefObject<MediabunnyController | null>;
  state: MediabunnyState;
};

const HIDE_CONTROLS_AFTER_MS = 2000;

export function PlayerSurface({ canvasRef, controllerRef, state }: Props) {
  const containerRef = useRef<HTMLDivElement | null>(null);
  const [controlsVisible, setControlsVisible] = useState(true);
  const hideTimerRef = useRef<number | null>(null);

  const showControls = useCallback(() => {
    setControlsVisible(true);
    if (hideTimerRef.current !== null) window.clearTimeout(hideTimerRef.current);
    if (!state.hasVideo) return; // audio-only: keep controls pinned.
    hideTimerRef.current = window.setTimeout(() => {
      setControlsVisible(false);
    }, HIDE_CONTROLS_AFTER_MS);
  }, [state.hasVideo]);

  useEffect(() => {
    showControls();
    return () => {
      if (hideTimerRef.current !== null) window.clearTimeout(hideTimerRef.current);
    };
  }, [showControls]);

  const handleSurfaceClick = () => {
    const controller = controllerRef.current;
    if (!controller || !state.ready) return;
    if (state.needsGesture) {
      void controller.play();
      return;
    }
    if (state.playing) controller.pause();
    else void controller.play();
  };

  return (
    <div
      className="mb-surface"
      onPointerLeave={() => state.hasVideo && setControlsVisible(false)}
      onPointerMove={showControls}
      ref={containerRef}
    >
      <canvas
        className="mb-canvas"
        onClick={handleSurfaceClick}
        ref={canvasRef}
        style={{ display: state.hasVideo ? "block" : "none" }}
      />
      {!state.hasVideo && state.ready ? (
        <div className="mb-audio-only">
          <p className="muted">Audio only</p>
        </div>
      ) : null}
      {!state.ready && !state.warning ? (
        <div className="mb-loading">
          <p className="muted">Loading…</p>
        </div>
      ) : null}
      {state.ready && state.needsGesture ? (
        <button
          aria-label="Play"
          className="mb-gesture-overlay"
          onClick={(event) => {
            event.stopPropagation();
            void controllerRef.current?.play();
          }}
          type="button"
        >
          <PlayGlyphLarge />
        </button>
      ) : null}
      <ControlsBar
        containerRef={containerRef}
        controllerRef={controllerRef}
        controlsVisible={controlsVisible}
        onInteract={showControls}
        state={state}
      />
    </div>
  );
}

function PlayGlyphLarge() {
  return (
    <svg aria-hidden="true" viewBox="0 0 96 96" fill="currentColor">
      <circle cx="48" cy="48" r="44" fill="rgba(0,0,0,0.55)" />
      <path d="M38 30v36l28-18z" fill="currentColor" />
    </svg>
  );
}
