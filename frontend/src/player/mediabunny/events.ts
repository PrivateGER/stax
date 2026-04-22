export type MediabunnyEvents = {
  play: () => void;
  pause: () => void;
  seeked: (positionSeconds: number) => void;
  timeupdate: (positionSeconds: number) => void;
  audiochange: () => void;
  ended: () => void;
  error: (message: string) => void;
  ready: () => void;
};
