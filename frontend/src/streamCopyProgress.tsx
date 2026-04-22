import type { StreamCopySummary } from "./types";

export function isActiveStreamCopy(summary: StreamCopySummary | null): boolean {
  return summary?.status === "queued" || summary?.status === "running";
}

type Props = {
  summary: StreamCopySummary | null;
};

export function StreamCopyProgress({ summary }: Props) {
  if (!summary || (summary.status !== "queued" && summary.status !== "running")) {
    return null;
  }

  if (summary.status === "queued") {
    return (
      <div className="stream-copy-progress">
        <p className="muted">Queued — waiting for a worker.</p>
      </div>
    );
  }

  const ratio = summary.progressRatio;
  if (ratio == null || !Number.isFinite(ratio)) {
    return (
      <div className="stream-copy-progress">
        <div
          aria-label="Preparing stream copy"
          className="stream-copy-progress-bar"
          role="progressbar"
        >
          <div className="stream-copy-progress-fill indeterminate" />
        </div>
      </div>
    );
  }

  const clamped = Math.max(0, Math.min(1, ratio));
  const percent = Math.round(clamped * 100);
  const speed = summary.progressSpeed;
  const speedLabel =
    speed != null && Number.isFinite(speed) && speed > 0
      ? `${speed.toFixed(speed >= 10 ? 0 : 1)}x`
      : null;

  return (
    <div className="stream-copy-progress">
      <div
        aria-valuemax={100}
        aria-valuemin={0}
        aria-valuenow={percent}
        className="stream-copy-progress-bar"
        role="progressbar"
      >
        <div
          className="stream-copy-progress-fill"
          style={{ width: `${percent}%` }}
        />
      </div>
      <div className="stream-copy-progress-meta">
        <span className="percent">{percent}%</span>
        {speedLabel ? <span>{speedLabel}</span> : null}
      </div>
    </div>
  );
}
