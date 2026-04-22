import { navigate } from "../router";
import { StreamCopyProgress } from "../streamCopyProgress";
import type { MediaItem, StreamCopySummary } from "../types";

type Props = {
  item: MediaItem;
  title: string;
  liveStreamCopy: StreamCopySummary | null;
  creatingStreamCopy: boolean;
  streamCopyError: string | null;
  onCreateStreamCopy: () => void;
};

export function UnplayableNotice({
  item,
  title,
  liveStreamCopy,
  creatingStreamCopy,
  streamCopyError,
  onCreateStreamCopy,
}: Props) {
  const liveActive =
    liveStreamCopy?.status === "queued" || liveStreamCopy?.status === "running";
  const isPreparing = liveActive || item.preparationState === "preparing";
  const liveFailed = liveStreamCopy?.status === "failed";

  const message = isPreparing
    ? "A stream copy is still being prepared for this title."
    : liveFailed || item.preparationState === "failed"
      ? liveStreamCopy?.error ??
        item.streamCopy?.error ??
        "The last stream copy attempt failed. Create a new one to try again."
      : item.preparationState === "unsupported"
        ? "This title is not supported for browser playback."
        : "This title needs a stream copy before it can be played.";

  const buttonLabel =
    liveStreamCopy?.status === "queued"
      ? "Queued…"
      : isPreparing
        ? "Preparing…"
        : creatingStreamCopy
          ? "Submitting…"
          : "Create stream copy";

  return (
    <section className="title-missing">
      <h1>{title}</h1>
      <p className="muted">{message}</p>
      <StreamCopyProgress summary={liveStreamCopy} />
      {item.playbackMode === "needsPreparation" ? (
        <button
          className="primary-button"
          disabled={creatingStreamCopy || isPreparing}
          onClick={onCreateStreamCopy}
          type="button"
        >
          {buttonLabel}
        </button>
      ) : null}
      {streamCopyError ? <p className="error">{streamCopyError}</p> : null}
      <button
        className="ghost-button"
        onClick={() => navigate({ name: "title", mediaId: item.id })}
        type="button"
      >
        Back to title
      </button>
    </section>
  );
}
