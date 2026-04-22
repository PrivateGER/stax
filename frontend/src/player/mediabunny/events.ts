export type MediabunnyEvents = {
  play: () => void;
  pause: () => void;
  seeked: (positionSeconds: number) => void;
  timeupdate: (positionSeconds: number) => void;
  ended: () => void;
  error: (message: string) => void;
  ready: () => void;
};
