CREATE INDEX IF NOT EXISTS idx_media_items_pending_probes
ON media_items(root_path, relative_path)
WHERE probed_at IS NULL
  AND probe_error IS NULL;

CREATE INDEX IF NOT EXISTS idx_media_items_pending_thumbnails
ON media_items(root_path, relative_path)
WHERE thumbnail_generated_at IS NULL
  AND thumbnail_error IS NULL
  AND probed_at IS NOT NULL
  AND probe_error IS NULL;

CREATE INDEX IF NOT EXISTS idx_stream_copies_pending_status
ON stream_copies(status, updated_at)
WHERE status IN ('queued', 'running');
