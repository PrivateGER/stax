import { startTransition, useEffect, useRef, useState, type RefObject } from "react";

import { streamUrl } from "../../api";
import type { MediaItem } from "../../types";
import { MediabunnyController, type MediabunnyState } from "./MediabunnyController";

export type UseMediabunnyController = {
  controllerRef: RefObject<MediabunnyController | null>;
  canvasRef: RefObject<HTMLCanvasElement | null>;
  state: MediabunnyState;
};

const INITIAL_STATE: MediabunnyState = {
  playing: false,
  currentTime: 0,
  duration: 0,
  hasVideo: false,
  hasAudio: false,
  bufferedFraction: 0,
  audioTracks: [],
  selectedAudioTrackId: null,
  volume: 0.7,
  muted: false,
  needsGesture: false,
  ready: false,
  warning: null,
};

export function useMediabunnyController(
  item: MediaItem | null,
  onError: (message: string) => void,
): UseMediabunnyController {
  const controllerRef = useRef<MediabunnyController | null>(null);
  const canvasRef = useRef<HTMLCanvasElement | null>(null);
  const onErrorRef = useRef(onError);
  onErrorRef.current = onError;
  const [state, setState] = useState<MediabunnyState>(INITIAL_STATE);

  const playable =
    item?.preparationState === "direct" || item?.preparationState === "prepared";

  useEffect(() => {
    if (!playable || !item || !canvasRef.current) {
      return;
    }

    const controller = new MediabunnyController({
      url: streamUrl(item.id),
      canvas: canvasRef.current,
    });
    controllerRef.current = controller;
    setState(controller.state);

    const refresh = () => setState(controller.state);
    const refreshTimeupdate = () => {
      startTransition(() => {
        setState(controller.state);
      });
    };
    const offs = [
      controller.on("ready", refresh),
      controller.on("play", refresh),
      controller.on("pause", refresh),
      controller.on("seeked", refresh),
      controller.on("timeupdate", refreshTimeupdate),
      controller.on("audiochange", refresh),
      controller.on("ended", refresh),
      controller.on("error", (message) => {
        onErrorRef.current(message);
        refresh();
      }),
    ];

    return () => {
      for (const off of offs) off();
      controller.dispose();
      if (controllerRef.current === controller) {
        controllerRef.current = null;
      }
      setState(INITIAL_STATE);
    };
  }, [item?.id, playable]); // eslint-disable-line react-hooks/exhaustive-deps

  return { controllerRef, canvasRef, state };
}
