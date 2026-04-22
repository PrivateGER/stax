export type PlayerIntent =
  | { type: "togglePlay" }
  | { type: "toggleFullscreen" }
  | { type: "toggleMute" }
  | { type: "relativeSeek"; deltaSeconds: number }
  | { type: "seekToFraction"; fraction: number };

export function mapKey(event: KeyboardEvent): PlayerIntent | null {
  if (event.ctrlKey || event.metaKey || event.altKey) return null;
  switch (event.code) {
    case "Space":
    case "KeyK":
      return { type: "togglePlay" };
    case "KeyF":
      return { type: "toggleFullscreen" };
    case "KeyM":
      return { type: "toggleMute" };
    case "ArrowLeft":
      return { type: "relativeSeek", deltaSeconds: event.shiftKey ? -10 : -5 };
    case "ArrowRight":
      return { type: "relativeSeek", deltaSeconds: event.shiftKey ? 10 : 5 };
    case "Digit0":
    case "Numpad0":
      return { type: "seekToFraction", fraction: 0 };
    case "Digit1":
    case "Numpad1":
      return { type: "seekToFraction", fraction: 0.1 };
    case "Digit2":
    case "Numpad2":
      return { type: "seekToFraction", fraction: 0.2 };
    case "Digit3":
    case "Numpad3":
      return { type: "seekToFraction", fraction: 0.3 };
    case "Digit4":
    case "Numpad4":
      return { type: "seekToFraction", fraction: 0.4 };
    case "Digit5":
    case "Numpad5":
      return { type: "seekToFraction", fraction: 0.5 };
    case "Digit6":
    case "Numpad6":
      return { type: "seekToFraction", fraction: 0.6 };
    case "Digit7":
    case "Numpad7":
      return { type: "seekToFraction", fraction: 0.7 };
    case "Digit8":
    case "Numpad8":
      return { type: "seekToFraction", fraction: 0.8 };
    case "Digit9":
    case "Numpad9":
      return { type: "seekToFraction", fraction: 0.9 };
    default:
      return null;
  }
}
