import {
  AudioSample,
  CustomAudioDecoder,
  registerDecoder,
  type AudioCodec,
  type EncodedPacket,
} from "mediabunny";
import type {
  MPEGDecodedAudio,
  MPEGDecoderWebWorker as MPEGDecoderWebWorkerType,
} from "mpg123-decoder";

const MP3_CODEC_STRINGS = ["mp3", "mp4a.69", "mp4a.6B", "mp4a.6b"] as const;

let registrationPromise: Promise<void> | null = null;

export function ensureCustomAudioDecoders(): Promise<void> {
  registrationPromise ??= registerMp3DecoderIfNeeded();
  return registrationPromise;
}

async function registerMp3DecoderIfNeeded(): Promise<void> {
  const codecStringsNeedingPolyfill = await getUnsupportedMp3CodecStrings();
  if (codecStringsNeedingPolyfill.size === 0) return;

  class WasmMp3AudioDecoder extends CustomAudioDecoder {
    private decoder: MPEGDecoderWebWorkerType | null = null;

    static override supports(codec: AudioCodec, config: AudioDecoderConfig): boolean {
      return codec === "mp3" && codecStringsNeedingPolyfill.has(config.codec);
    }

    async init(): Promise<void> {
      const { MPEGDecoderWebWorker } = await import("mpg123-decoder");
      this.decoder = new MPEGDecoderWebWorker({ enableGapless: false });
      await this.decoder.ready;
    }

    async decode(packet: EncodedPacket): Promise<void> {
      if (!this.decoder) throw new Error("MP3 decoder has not been initialized.");
      const decoded = await this.decoder.decodeFrame(packet.data);
      emitDecodedAudioSample(decoded, packet.timestamp, this.onSample);
    }

    flush(): void {
      // mpg123-decoder does not expose an explicit stream flush. MP3 frames are
      // emitted as packets arrive, so there is no additional tail sample here.
    }

    async close(): Promise<void> {
      const decoder = this.decoder;
      this.decoder = null;
      await decoder?.free();
    }
  }

  registerDecoder(WasmMp3AudioDecoder);
}

async function getUnsupportedMp3CodecStrings(): Promise<Set<string>> {
  const unsupported = new Set<string>();

  if (typeof AudioDecoder === "undefined") {
    for (const codec of MP3_CODEC_STRINGS) unsupported.add(codec);
    return unsupported;
  }

  for (const codec of MP3_CODEC_STRINGS) {
    try {
      const support = await AudioDecoder.isConfigSupported({
        codec,
        numberOfChannels: 2,
        sampleRate: 44100,
      });
      if (support.supported !== true) unsupported.add(codec);
    } catch {
      unsupported.add(codec);
    }
  }

  return unsupported;
}

function emitDecodedAudioSample(
  decoded: MPEGDecodedAudio,
  timestamp: number,
  onSample: (sample: AudioSample) => unknown,
): void {
  if (decoded.samplesDecoded === 0) return;

  const channels = decoded.channelData
    .map((channel) => channel.subarray(0, decoded.samplesDecoded))
    .filter((channel) => channel.length === decoded.samplesDecoded);
  if (channels.length === 0) return;

  const planarData = new Float32Array(decoded.samplesDecoded * channels.length);
  channels.forEach((channel, index) => {
    planarData.set(channel, index * decoded.samplesDecoded);
  });

  onSample(
    new AudioSample({
      data: planarData,
      format: "f32-planar",
      numberOfChannels: channels.length,
      sampleRate: decoded.sampleRate,
      timestamp,
    }),
  );
}
