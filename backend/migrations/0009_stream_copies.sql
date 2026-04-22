UPDATE media_items
SET playback_mode = 'needsPreparation'
WHERE playback_mode IN ('hlsRemux', 'hlsAudioTranscode', 'hlsFullTranscode');

CREATE TABLE IF NOT EXISTS stream_copies (
    media_id            TEXT    NOT NULL PRIMARY KEY,
    source_size_bytes   INTEGER NOT NULL,
    source_modified_at  TEXT    NOT NULL,
    status              TEXT    NOT NULL,
    audio_stream_index  INTEGER,
    subtitle_mode       TEXT    NOT NULL,
    subtitle_kind       TEXT,
    subtitle_index      INTEGER,
    output_path         TEXT,
    output_content_type TEXT,
    subtitle_path       TEXT,
    error               TEXT,
    created_at          TEXT    NOT NULL,
    updated_at          TEXT    NOT NULL,
    FOREIGN KEY (media_id) REFERENCES media_items(id) ON DELETE CASCADE
);
