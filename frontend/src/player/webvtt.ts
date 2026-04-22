export type WebVttCue = {
  start: number;
  end: number;
  text: string;
};

export function parseWebVtt(source: string): WebVttCue[] {
  const normalized = source.replace(/^\uFEFF/, "").replace(/\r\n?/g, "\n");
  const blocks = normalized.split(/\n{2,}/);
  const cues: WebVttCue[] = [];

  for (const block of blocks) {
    const trimmed = block.trim();
    if (!trimmed || trimmed.startsWith("WEBVTT")) continue;
    if (trimmed.startsWith("NOTE") || trimmed.startsWith("STYLE") || trimmed.startsWith("REGION")) {
      continue;
    }

    const lines = block.split("\n");
    const timingLineIndex = lines.findIndex((line) => line.includes("-->"));
    if (timingLineIndex === -1) continue;

    const timingLine = lines[timingLineIndex]?.trim();
    if (!timingLine) continue;

    const [startRaw, endAndSettings] = timingLine.split(/\s+-->\s+/);
    if (!startRaw || !endAndSettings) continue;

    const [endRaw] = endAndSettings.trim().split(/\s+/, 1);
    if (!endRaw) continue;

    const start = parseTimestamp(startRaw);
    const end = parseTimestamp(endRaw);
    if (start === null || end === null || end <= start) continue;

    const payload = lines
      .slice(timingLineIndex + 1)
      .join("\n")
      .trim();
    const text = normalizeCueText(payload);
    if (!text) continue;

    cues.push({ start, end, text });
  }

  return cues.sort((left, right) => left.start - right.start || left.end - right.end);
}

function parseTimestamp(value: string): number | null {
  const match = value.trim().match(/^(?:(\d+):)?(\d{2}):(\d{2})[.,](\d{3})$/);
  if (!match) return null;

  const [, hoursRaw, minutesRaw, secondsRaw, millisRaw] = match;
  const hours = Number(hoursRaw ?? 0);
  const minutes = Number(minutesRaw);
  const seconds = Number(secondsRaw);
  const millis = Number(millisRaw);

  if ([hours, minutes, seconds, millis].some((part) => Number.isNaN(part))) {
    return null;
  }

  return hours * 3600 + minutes * 60 + seconds + millis / 1000;
}

function normalizeCueText(value: string) {
  if (!value) return "";

  const withoutTags = value
    .replace(/<br\s*\/?>/gi, "\n")
    .replace(/<[^>]+>/g, "");
  const decoded = decodeEntities(withoutTags)
    .replace(/\n{3,}/g, "\n\n")
    .trim();

  return decoded;
}

function decodeEntities(value: string) {
  if (typeof document === "undefined") {
    return value
      .replaceAll("&amp;", "&")
      .replaceAll("&lt;", "<")
      .replaceAll("&gt;", ">")
      .replaceAll("&nbsp;", " ")
      .replaceAll("&#39;", "'")
      .replaceAll("&quot;", '"');
  }

  const textarea = document.createElement("textarea");
  textarea.innerHTML = value;
  return textarea.value;
}
