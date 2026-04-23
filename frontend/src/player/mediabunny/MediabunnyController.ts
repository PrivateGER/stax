import {
  ALL_FORMATS,
  AudioBufferSink,
  CanvasSink,
  Input,
  UrlSource,
  type InputAudioTrack,
  type InputVideoTrack,
  type WrappedAudioBuffer,
  type WrappedCanvas,
} from "mediabunny";

import { formatLanguageName } from "../../format";
import { ensureCustomAudioDecoders } from "./customAudioDecoders";
import type { MediabunnyEvents } from "./events";

type Listeners = {
  [K in keyof MediabunnyEvents]: Set<MediabunnyEvents[K]>;
};

export type MediabunnyTrackInfo = {
  id: string;
  label: string;
  language?: string;
};

export type MediabunnyState = {
  playing: boolean;
  currentTime: number;
  duration: number;
  hasVideo: boolean;
  hasAudio: boolean;
  audioTracks: MediabunnyTrackInfo[];
  selectedAudioTrackId: string | null;
  volume: number;
  muted: boolean;
  needsGesture: boolean;
  ready: boolean;
  warning: string | null;
};

export function isWebCodecsSupported(): boolean {
  return typeof VideoDecoder !== "undefined" && typeof AudioDecoder !== "undefined";
}

export class MediabunnyController {
  private readonly canvas: HTMLCanvasElement;
  private readonly context: CanvasRenderingContext2D;

  private input: Input | null = null;
  private audioContext: AudioContext | null = null;
  private gainNode: GainNode | null = null;
  private videoSink: CanvasSink | null = null;
  private audioSink: AudioBufferSink | null = null;
  private videoTrack: InputVideoTrack | null = null;
  private audioTracks: InputAudioTrack[] = [];
  private audioTrackInfos: MediabunnyTrackInfo[] = [];
  private audioTrack: InputAudioTrack | null = null;

  private duration = 0;
  private playing = false;
  private audioContextStartTime = 0;
  private playbackTimeAtStart = 0;

  private videoFrameIterator: AsyncGenerator<WrappedCanvas, void, unknown> | null = null;
  private audioBufferIterator: AsyncGenerator<WrappedAudioBuffer, void, unknown> | null = null;
  private nextFrame: WrappedCanvas | null = null;
  private queuedAudioNodes = new Set<AudioBufferSourceNode>();

  /** Incremented on seek; in-flight async work checks this to bail out after a new seek. */
  private asyncId = 0;
  private rafHandle: number | null = null;
  private backgroundTickHandle: number | null = null;
  private lastTimeUpdateEmitAt = 0;
  /** Emit `timeupdate` at most this often (ms). Keeps React renders bounded. */
  private static TIMEUPDATE_INTERVAL_MS = 250;

  private volumeValue = 0.7;
  private mutedValue = false;
  private warning: string | null = null;
  private ready = false;
  private disposed = false;

  private listeners: Listeners = {
    play: new Set(),
    pause: new Set(),
    seeked: new Set(),
    timeupdate: new Set(),
    audiochange: new Set(),
    ended: new Set(),
    error: new Set(),
    ready: new Set(),
  };

  constructor(opts: { url: string; canvas: HTMLCanvasElement }) {
    this.canvas = opts.canvas;
    const context = this.canvas.getContext("2d");
    if (!context) throw new Error("Could not acquire 2D canvas context.");
    this.context = context;
    void this.load(opts.url);
  }

  // ===== public API =====

  get state(): MediabunnyState {
    return {
      playing: this.playing,
      currentTime: this.getPlaybackTime(),
      duration: this.duration,
      hasVideo: this.videoTrack !== null,
      hasAudio: this.audioTrack !== null,
      audioTracks: this.audioTrackInfos,
      selectedAudioTrackId: this.audioTrack ? String(this.audioTrack.id) : null,
      volume: this.volumeValue,
      muted: this.mutedValue,
      needsGesture: this.audioContext?.state === "suspended",
      ready: this.ready,
      warning: this.warning,
    };
  }

  async play(): Promise<void> {
    if (!this.ready || this.disposed || !this.audioContext) return;

    if (this.audioContext.state === "suspended") {
      await this.audioContext.resume();
    }

    if (this.getPlaybackTime() >= this.duration) {
      this.playbackTimeAtStart = 0;
      await this.startVideoIterator();
    }

    this.audioContextStartTime = this.audioContext.currentTime;
    this.playing = true;

    if (this.audioSink) {
      void this.audioBufferIterator?.return();
      this.audioBufferIterator = this.audioSink.buffers(this.getPlaybackTime());
      void this.runAudioIterator();
    }

    this.emit("play");
  }

  pause(): void {
    if (!this.playing) return;
    this.playbackTimeAtStart = this.getPlaybackTime();
    this.playing = false;
    this.stopAudioOutput();
    this.emit("pause");
  }

  async seek(seconds: number): Promise<void> {
    if (!this.ready || this.disposed) return;
    const clamped = Math.max(0, Math.min(seconds, this.duration));
    const wasPlaying = this.playing;
    if (wasPlaying) this.pause();
    this.playbackTimeAtStart = clamped;
    await this.startVideoIterator();
    this.emit("seeked", clamped);
    this.emit("timeupdate", clamped);
    if (wasPlaying && clamped < this.duration) {
      await this.play();
    }
  }

  setVolume(v: number): void {
    this.volumeValue = Math.max(0, Math.min(1, v));
    this.mutedValue = false;
    this.applyGain();
  }

  setMuted(m: boolean): void {
    this.mutedValue = m;
    this.applyGain();
  }

  toggleMute(): void {
    this.setMuted(!this.mutedValue);
  }

  getAudioTracks(): MediabunnyTrackInfo[] {
    return this.audioTrackInfos;
  }

  async selectAudioTrack(id: string): Promise<void> {
    if (!this.ready || this.disposed) return;

    const nextTrack =
      this.audioTracks.find((track) => String(track.id) === id) ?? null;
    if (!nextTrack || nextTrack === this.audioTrack) return;

    const wasPlaying = this.playing;
    if (wasPlaying) {
      this.playbackTimeAtStart = this.getPlaybackTime();
      this.playing = false;
      this.stopAudioOutput();
    }

    this.audioTrack = nextTrack;
    this.audioSink = new AudioBufferSink(nextTrack);
    this.emit("audiochange");

    if (wasPlaying && this.audioContext && this.playbackTimeAtStart < this.duration) {
      if (this.audioContext.state === "suspended") {
        await this.audioContext.resume();
      }

      this.audioContextStartTime = this.audioContext.currentTime;
      this.playing = true;
      this.audioBufferIterator = this.audioSink.buffers(this.playbackTimeAtStart);
      void this.runAudioIterator();
    }

    this.emit("timeupdate", this.getPlaybackTime());
  }

  on<K extends keyof MediabunnyEvents>(
    event: K,
    cb: MediabunnyEvents[K],
  ): () => void {
    this.listeners[event].add(cb as never);
    return () => {
      this.listeners[event].delete(cb as never);
    };
  }

  dispose(): void {
    if (this.disposed) return;
    this.disposed = true;
    if (this.rafHandle !== null) cancelAnimationFrame(this.rafHandle);
    if (this.backgroundTickHandle !== null) window.clearInterval(this.backgroundTickHandle);
    void this.videoFrameIterator?.return();
    this.stopAudioOutput();
    this.audioContext?.close().catch(() => {
      // Ignored — already closed or never opened.
    });
    this.input?.dispose();
    for (const set of Object.values(this.listeners)) {
      set.clear();
    }
  }

  // ===== private =====

  private stopAudioOutput(): void {
    void this.audioBufferIterator?.return();
    this.audioBufferIterator = null;
    for (const node of this.queuedAudioNodes) {
      try {
        node.stop();
      } catch {
        // Already stopped.
      }
    }
    this.queuedAudioNodes.clear();
  }

  private async load(url: string): Promise<void> {
    try {
      await ensureCustomAudioDecoders();
      if (this.disposed) return;
      this.input = new Input({ source: new UrlSource(url), formats: ALL_FORMATS });
      if (this.disposed) {
        // dispose() ran between the `await` above and this assignment, so
        // dispose()'s own `this.input?.dispose()` was a no-op. Clean up now.
        this.input.dispose();
        return;
      }
      this.duration = await this.input.computeDuration();

      let videoTrack = await this.input.getPrimaryVideoTrack();
      const primaryAudioTrack = await this.input.getPrimaryAudioTrack();
      const inputAudioTracks = await this.input.getAudioTracks();
      const warnings: string[] = [];
      const availableAudioTracks: InputAudioTrack[] = [];
      let unavailableAudioTrackCount = 0;

      if (videoTrack) {
        if (videoTrack.codec === null) {
          warnings.push("Unsupported video codec.");
          videoTrack = null;
        } else if (!(await videoTrack.canDecode())) {
          warnings.push("The browser cannot decode this video track.");
          videoTrack = null;
        }
      }

      for (const track of inputAudioTracks) {
        if (track.codec === null) {
          unavailableAudioTrackCount += 1;
          continue;
        }

        if (!(await track.canDecode())) {
          unavailableAudioTrackCount += 1;
          continue;
        }

        availableAudioTracks.push(track);
      }

      if (unavailableAudioTrackCount === 1) {
        warnings.push("One audio track is unavailable in this browser.");
      } else if (unavailableAudioTrackCount > 1) {
        warnings.push(`${unavailableAudioTrackCount} audio tracks are unavailable in this browser.`);
      }

      const audioTrack =
        (primaryAudioTrack &&
          availableAudioTracks.find((track) => track.id === primaryAudioTrack.id)) ??
        availableAudioTracks[0] ??
        null;

      if (this.disposed) return;
      if (!videoTrack && !audioTrack) {
        throw new Error(
          warnings.join(" ") || "No decodable video or audio track in this file.",
        );
      }
      if (warnings.length > 0) this.warning = warnings.join(" ");

      this.videoTrack = videoTrack;
      this.audioTracks = availableAudioTracks;
      this.audioTrackInfos = availableAudioTracks.map((track) => describeAudioTrack(track));
      this.audioTrack = audioTrack;

      this.audioContext = new AudioContext({ sampleRate: audioTrack?.sampleRate });
      this.gainNode = this.audioContext.createGain();
      this.gainNode.connect(this.audioContext.destination);
      this.applyGain();

      const videoCanBeTransparent = videoTrack
        ? await videoTrack.canBeTransparent()
        : false;

      if (videoTrack) {
        this.canvas.width = videoTrack.displayWidth;
        this.canvas.height = videoTrack.displayHeight;
        this.videoSink = new CanvasSink(videoTrack, {
          poolSize: 2,
          fit: "contain",
          alpha: videoCanBeTransparent,
        });
      }
      if (audioTrack) {
        this.audioSink = new AudioBufferSink(audioTrack);
      }

      await this.startVideoIterator();
      this.ready = true;
      this.emit("ready");
      this.emit("timeupdate", 0);
      this.startRafLoop();

      if (this.audioContext.state === "running") {
        await this.play();
      }
    } catch (error) {
      if (this.disposed) return;
      this.emit("error", error instanceof Error ? error.message : String(error));
    }
  }

  private async startVideoIterator(): Promise<void> {
    if (!this.videoSink) return;
    this.asyncId++;
    await this.videoFrameIterator?.return();
    this.videoFrameIterator = this.videoSink.canvases(this.getPlaybackTime());
    const firstFrame = (await this.videoFrameIterator.next()).value ?? null;
    const secondFrame = (await this.videoFrameIterator.next()).value ?? null;
    this.nextFrame = secondFrame;
    if (firstFrame) {
      this.context.clearRect(0, 0, this.canvas.width, this.canvas.height);
      this.context.drawImage(firstFrame.canvas, 0, 0);
    }
  }

  private startRafLoop(): void {
    const render = () => {
      if (this.disposed) return;
      this.renderFrame();
      this.rafHandle = requestAnimationFrame(render);
    };
    this.rafHandle = requestAnimationFrame(render);
    // Keep rendering even when the tab is hidden so position-update listeners
    // (timeupdate) still fire — requestAnimationFrame is throttled to ~1 Hz
    // when backgrounded.
    this.backgroundTickHandle = window.setInterval(() => {
      if (!this.disposed) this.renderFrame();
    }, 500);
  }

  private renderFrame(): void {
    if (!this.ready) return;
    const playbackTime = this.getPlaybackTime();

    if (playbackTime >= this.duration && this.playing) {
      this.pause();
      this.playbackTimeAtStart = this.duration;
      this.emit("ended");
    }

    if (this.nextFrame && this.nextFrame.timestamp <= playbackTime) {
      this.context.clearRect(0, 0, this.canvas.width, this.canvas.height);
      this.context.drawImage(this.nextFrame.canvas, 0, 0);
      this.nextFrame = null;
      void this.advanceVideoFrame();
    }

    const now = performance.now();
    if (now - this.lastTimeUpdateEmitAt >= MediabunnyController.TIMEUPDATE_INTERVAL_MS) {
      this.lastTimeUpdateEmitAt = now;
      this.emit("timeupdate", playbackTime);
    }
  }

  private async advanceVideoFrame(): Promise<void> {
    const localAsyncId = this.asyncId;
    while (true) {
      const iterator = this.videoFrameIterator;
      if (!iterator) return;
      let candidate: WrappedCanvas | null;
      try {
        candidate = (await iterator.next()).value ?? null;
      } catch {
        // iterator.next() throws InputDisposedError after dispose(), and the
        // UrlSource fetch can reject mid-flight on abort. Either way, if we've
        // moved on (seek, disposal), there's nothing useful to do.
        return;
      }
      if (!candidate) return;
      if (localAsyncId !== this.asyncId || this.disposed) return;
      const playbackTime = this.getPlaybackTime();
      if (candidate.timestamp <= playbackTime) {
        this.context.clearRect(0, 0, this.canvas.width, this.canvas.height);
        this.context.drawImage(candidate.canvas, 0, 0);
      } else {
        this.nextFrame = candidate;
        return;
      }
    }
  }

  private async runAudioIterator(): Promise<void> {
    if (!this.audioSink || !this.audioContext || !this.gainNode) return;
    const iterator = this.audioBufferIterator;
    if (!iterator) return;
    try {
      await this.pumpAudioIterator(iterator);
    } catch {
      // Same rationale as advanceVideoFrame: InputDisposedError / aborted
      // fetches during tear-down are expected and should not surface as
      // unhandled rejections.
    }
  }

  private async pumpAudioIterator(
    iterator: AsyncGenerator<WrappedAudioBuffer, void, unknown>,
  ): Promise<void> {
    if (!this.audioContext || !this.gainNode) return;
    for await (const { buffer, timestamp } of iterator) {
      if (this.disposed) return;
      const node = this.audioContext.createBufferSource();
      node.buffer = buffer;
      node.connect(this.gainNode);
      const startTimestamp =
        this.audioContextStartTime + timestamp - this.playbackTimeAtStart;
      if (startTimestamp >= this.audioContext.currentTime) {
        node.start(startTimestamp);
      } else {
        node.start(
          this.audioContext.currentTime,
          this.audioContext.currentTime - startTimestamp,
        );
      }
      this.queuedAudioNodes.add(node);
      node.onended = () => {
        this.queuedAudioNodes.delete(node);
      };
      // Slow the loop if we're running well ahead of real-time.
      if (timestamp - this.getPlaybackTime() >= 1) {
        await new Promise<void>((resolve) => {
          const id = window.setInterval(() => {
            if (this.disposed || timestamp - this.getPlaybackTime() < 1) {
              window.clearInterval(id);
              resolve();
            }
          }, 100);
        });
      }
    }
  }

  private getPlaybackTime(): number {
    if (this.playing && this.audioContext) {
      return (
        this.audioContext.currentTime -
        this.audioContextStartTime +
        this.playbackTimeAtStart
      );
    }
    return this.playbackTimeAtStart;
  }

  private applyGain(): void {
    if (!this.gainNode) return;
    const actual = this.mutedValue ? 0 : this.volumeValue;
    // Quadratic for finer control at low volumes.
    this.gainNode.gain.value = actual * actual;
  }

  private emit<K extends keyof MediabunnyEvents>(
    event: K,
    ...args: Parameters<MediabunnyEvents[K]>
  ): void {
    for (const cb of this.listeners[event]) {
      try {
        (cb as (...a: unknown[]) => void)(...args);
      } catch (error) {
        console.error("[mediabunny]", event, error);
      }
    }
  }
}

function describeAudioTrack(track: InputAudioTrack): MediabunnyTrackInfo {
  const language =
    track.languageCode && track.languageCode !== "und"
      ? formatLanguageName(track.languageCode)
      : null;
  const label = firstNonEmpty(track.name, language, `Track ${track.number}`);
  const details = [
    detailUnlessDuplicate(language, label),
    formatCodec(track.codec),
    track.disposition.commentary ? "commentary" : null,
    track.disposition.visuallyImpaired ? "descriptive" : null,
    track.disposition.hearingImpaired ? "SDH" : null,
    track.disposition.original ? "original" : null,
    track.disposition.default ? "default" : null,
  ];

  return {
    id: String(track.id),
    label: joinTrackLabel(label, details),
    language: language ?? undefined,
  };
}

function firstNonEmpty(...values: Array<string | null | undefined>) {
  return values.find((value) => value && value.trim().length > 0)?.trim() ?? "Unknown";
}

function detailUnlessDuplicate(detail: string | null, label: string) {
  if (!detail) return null;
  return detail.localeCompare(label, undefined, { sensitivity: "accent" }) === 0
    ? null
    : detail;
}

function joinTrackLabel(label: string, metadata: Array<string | null>) {
  const details = metadata.filter((value): value is string => Boolean(value));
  return details.length > 0 ? `${label} · ${details.join(" · ")}` : label;
}

function formatCodec(codec: unknown) {
  const value = typeof codec === "string" ? codec.trim() : "";
  if (!value) return null;

  return value
    .replaceAll("_", " ")
    .replaceAll("-", " ")
    .split(/\s+/)
    .filter(Boolean)
    .map((part) => part.toUpperCase())
    .join(" ");
}
