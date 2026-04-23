import { useEffect, useRef, useState } from "react";

import type { SubtitleSource } from "./subtitleSources";

type AudioTrackEntry = { id: string; label: string };

type Props = {
  subtitleSources: SubtitleSource[];
  selectedSubtitleIndex: number | null;
  onSelectSubtitle: (index: number | null) => void;
  audioTracks: AudioTrackEntry[];
  selectedAudioId: string | null;
  onSelectAudio: (id: string) => void;
};

export function TracksMenu({
  subtitleSources,
  selectedSubtitleIndex,
  onSelectSubtitle,
  audioTracks,
  selectedAudioId,
  onSelectAudio,
}: Props) {
  const [open, setOpen] = useState(false);
  const panelRef = useRef<HTMLDivElement | null>(null);
  const buttonRef = useRef<HTMLButtonElement | null>(null);

  useEffect(() => {
    if (!open) return;

    const handlePointer = (event: MouseEvent) => {
      const target = event.target as Node | null;
      if (!target) return;
      if (panelRef.current?.contains(target)) return;
      if (buttonRef.current?.contains(target)) return;
      setOpen(false);
    };
    const handleKey = (event: KeyboardEvent) => {
      if (event.key === "Escape") setOpen(false);
    };

    document.addEventListener("mousedown", handlePointer);
    document.addEventListener("keydown", handleKey);
    return () => {
      document.removeEventListener("mousedown", handlePointer);
      document.removeEventListener("keydown", handleKey);
    };
  }, [open]);

  const hasSubtitles = subtitleSources.length > 0;
  const hasAudioChoices = audioTracks.length > 1;
  if (!hasSubtitles && !hasAudioChoices) return null;

  return (
    <div className="tracks-menu">
      <button
        aria-expanded={open}
        aria-haspopup="menu"
        className="ghost-button tracks-menu-button"
        onClick={() => setOpen((value) => !value)}
        ref={buttonRef}
        type="button"
      >
        Tracks
      </button>
      {open ? (
        <div className="tracks-menu-panel" ref={panelRef} role="menu">
          {hasSubtitles ? (
            <div className="tracks-menu-section">
              <h3>Subtitles</h3>
              <Option
                active={selectedSubtitleIndex === null}
                label="Off"
                onClick={() => {
                  onSelectSubtitle(null);
                  setOpen(false);
                }}
              />
              {subtitleSources.map((track, index) => (
                <Option
                  active={selectedSubtitleIndex === index}
                  key={track.key}
                  label={track.label}
                  onClick={() => {
                    onSelectSubtitle(index);
                    setOpen(false);
                  }}
                />
              ))}
            </div>
          ) : null}

          {hasAudioChoices ? (
            <>
              {hasSubtitles ? <div className="tracks-menu-divider" /> : null}
              <div className="tracks-menu-section">
                <h3>Audio</h3>
                {audioTracks.map((track) => (
                  <Option
                    active={selectedAudioId === track.id}
                    key={track.id}
                    label={track.label}
                    onClick={() => {
                      onSelectAudio(track.id);
                      setOpen(false);
                    }}
                  />
                ))}
              </div>
            </>
          ) : null}
        </div>
      ) : null}
    </div>
  );
}

function Option({
  active,
  label,
  onClick,
}: {
  active: boolean;
  label: string;
  onClick: () => void;
}) {
  return (
    <button
      aria-checked={active}
      className={`tracks-menu-option ${active ? "active" : ""}`}
      onClick={onClick}
      role="menuitemradio"
      type="button"
    >
      <span>{label}</span>
      {active ? (
        <span aria-hidden="true" className="tracks-menu-option-marker">
          •
        </span>
      ) : null}
    </button>
  );
}
