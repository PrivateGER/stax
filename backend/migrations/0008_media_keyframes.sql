-- Source-video keyframe PTS offsets for legacy segmented playback experiments.
-- Populated by the background probe pool immediately after the main
-- ffprobe; consumed by older playback preparation code to build a segment plan that
-- lands every segment boundary on a source keyframe (otherwise we
-- can't start a new fMP4 chunk without re-encoding).
--
-- Stored in its own table rather than a BLOB column on media_items
-- because the per-title row count (~one entry per GOP, i.e. hundreds
-- to thousands for a feature-length film) makes row-level access
-- noticeably cheaper than pulling and re-parsing a JSON blob on every
-- session spawn. WITHOUT ROWID keeps the clustered index tight and
-- avoids a second lookup through an implicit rowid index.
CREATE TABLE IF NOT EXISTS media_keyframes (
    media_id    TEXT    NOT NULL,
    kf_index    INTEGER NOT NULL,
    pts_seconds REAL    NOT NULL,
    PRIMARY KEY (media_id, kf_index),
    FOREIGN KEY (media_id) REFERENCES media_items(id) ON DELETE CASCADE
) WITHOUT ROWID;

-- Companion stamp on media_items lets the rescan short-circuit (same
-- size_bytes + modified_at + keyframes_probed_at present ⇒ skip) and
-- gives operators a quick way to audit which titles are missing an
-- index. NULL = keyframes have never been probed for this row.
ALTER TABLE media_items ADD COLUMN keyframes_probed_at TEXT;
