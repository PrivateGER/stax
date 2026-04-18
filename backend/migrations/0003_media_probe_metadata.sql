ALTER TABLE media_items ADD COLUMN duration_seconds REAL;
ALTER TABLE media_items ADD COLUMN container_name TEXT;
ALTER TABLE media_items ADD COLUMN video_codec TEXT;
ALTER TABLE media_items ADD COLUMN audio_codec TEXT;
ALTER TABLE media_items ADD COLUMN width INTEGER;
ALTER TABLE media_items ADD COLUMN height INTEGER;
ALTER TABLE media_items ADD COLUMN probed_at TEXT;
ALTER TABLE media_items ADD COLUMN probe_error TEXT;
