ALTER TABLE media_items ADD COLUMN playback_mode TEXT NOT NULL DEFAULT 'direct';
ALTER TABLE media_items ADD COLUMN video_profile TEXT;
ALTER TABLE media_items ADD COLUMN video_level INTEGER;
ALTER TABLE media_items ADD COLUMN video_pix_fmt TEXT;
ALTER TABLE media_items ADD COLUMN video_bit_depth INTEGER;
ALTER TABLE media_items ADD COLUMN audio_streams_json TEXT NOT NULL DEFAULT '[]';
ALTER TABLE media_items ADD COLUMN subtitle_streams_json TEXT NOT NULL DEFAULT '[]';
