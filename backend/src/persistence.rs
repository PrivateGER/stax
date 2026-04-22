use std::{collections::HashMap, env, error::Error, fmt, path::Path};

use sqlx::{
    Pool, Row, Sqlite,
    sqlite::{SqliteConnectOptions, SqlitePoolOptions},
};
use time::{OffsetDateTime, format_description::well_known::Rfc3339};
use uuid::Uuid;

use crate::{
    RoomRecord,
    clock::{AuthoritativePlaybackClock, PlaybackClockCheckpoint, format_timestamp},
    protocol::{
        AudioStream, LibraryRoot, MediaItem, PlaybackMode, PlaybackStatus, SubtitleStream,
        SubtitleTrack,
    },
};

static MIGRATOR: sqlx::migrate::Migrator = sqlx::migrate!("./migrations");

#[derive(Clone, Debug)]
pub struct Persistence {
    pool: Pool<Sqlite>,
}

#[derive(Debug, Clone)]
pub(crate) struct LibrarySnapshot {
    pub roots: Vec<LibraryRoot>,
    pub items: Vec<MediaItem>,
}

/// Minimal media-item snapshot the thumbnail worker needs to schedule a job.
/// Kept narrow on purpose so the worker pool doesn't pull the entire
/// `MediaItem` shape (with its serialized stream/subtitle blobs) into memory
/// for every queued file.
#[derive(Debug, Clone)]
pub struct PendingThumbnail {
    pub media_id: Uuid,
    pub root_path: String,
    pub relative_path: String,
    pub video_codec: Option<String>,
    pub duration_seconds: Option<f64>,
}

/// All probe-derived state the scanner needs to short-circuit re-probing
/// when a file hasn't changed on disk. The scanner compares
/// `(size_bytes, modified_at)` against fresh filesystem metadata; if both
/// match and `probe_error` is `None`, every other field can be reused
/// verbatim and ffprobe is skipped entirely.
#[derive(Debug, Clone)]
pub struct CachedMediaRecord {
    pub id: Uuid,
    pub size_bytes: u64,
    pub modified_at: String,
    pub duration_seconds: Option<f64>,
    pub container_name: Option<String>,
    pub video_codec: Option<String>,
    pub audio_codec: Option<String>,
    pub width: Option<u32>,
    pub height: Option<u32>,
    pub probed_at: Option<String>,
    pub probe_error: Option<String>,
    pub playback_mode: PlaybackMode,
    pub video_profile: Option<String>,
    pub video_level: Option<u32>,
    pub video_pix_fmt: Option<String>,
    pub video_bit_depth: Option<u8>,
    pub audio_streams: Vec<AudioStream>,
    pub subtitle_streams: Vec<SubtitleStream>,
}

/// One probed file's worth of metadata, ready to be written back to the
/// `media_items` row by the background probe pool. Mirrors the columns
/// populated during the probe phase of a scan.
#[derive(Debug, Clone)]
pub struct ProbeOutcome {
    pub probed_at: String,
    pub probe_error: Option<String>,
    pub duration_seconds: Option<f64>,
    pub container_name: Option<String>,
    pub video_codec: Option<String>,
    pub audio_codec: Option<String>,
    pub width: Option<u32>,
    pub height: Option<u32>,
    pub playback_mode: PlaybackMode,
    pub video_profile: Option<String>,
    pub video_level: Option<u32>,
    pub video_pix_fmt: Option<String>,
    pub video_bit_depth: Option<u8>,
    pub audio_streams: Vec<AudioStream>,
    pub subtitle_streams: Vec<SubtitleStream>,
}

/// One row's worth of fields the walker writes per file, plus an optional
/// probe carry-over for cache-hit rows. Cache-miss rows pass `cached_probe
/// = None` and the upsert writes NULL probe columns + reset thumbnail
/// state, queueing the row for the background probe pool.
#[derive(Debug, Clone)]
pub struct WalkRecord {
    pub id: Uuid,
    pub root_path: String,
    pub relative_path: String,
    pub file_name: String,
    pub extension: Option<String>,
    pub size_bytes: u64,
    pub modified_at: String,
    pub indexed_at: String,
    pub content_type: Option<String>,
    pub subtitle_tracks: Vec<SubtitleTrack>,
    /// `Some` iff `(size, mtime)` matches a previously-probed row that
    /// did not error — the cached probe fields are preserved verbatim.
    pub cached_probe: Option<CachedProbeFields>,
    /// Cached thumbnail outcome from the previous scan (only honored
    /// alongside `cached_probe`). `(generated_at, error)`.
    pub cached_thumbnail: Option<(Option<String>, Option<String>)>,
}

#[derive(Debug, Clone)]
pub struct CachedProbeFields {
    pub probed_at: Option<String>,
    pub probe_error: Option<String>,
    pub duration_seconds: Option<f64>,
    pub container_name: Option<String>,
    pub video_codec: Option<String>,
    pub audio_codec: Option<String>,
    pub width: Option<u32>,
    pub height: Option<u32>,
    pub playback_mode: PlaybackMode,
    pub video_profile: Option<String>,
    pub video_level: Option<u32>,
    pub video_pix_fmt: Option<String>,
    pub video_bit_depth: Option<u8>,
    pub audio_streams: Vec<AudioStream>,
    pub subtitle_streams: Vec<SubtitleStream>,
}

/// Minimal media-item snapshot the probe pool needs to schedule a job.
#[derive(Debug, Clone)]
pub struct PendingProbe {
    pub media_id: Uuid,
    pub root_path: String,
    pub relative_path: String,
    pub extension: Option<String>,
}

#[derive(Debug)]
pub enum PersistenceError {
    Sqlx(sqlx::Error),
    Migrate(sqlx::migrate::MigrateError),
    InvalidData(String),
}

impl Persistence {
    pub async fn open_from_env() -> Result<Self, PersistenceError> {
        let database_path =
            env::var("SYNCPLAY_DATABASE_PATH").unwrap_or_else(|_| "syncplay.db".to_string());

        Self::open_at(database_path).await
    }

    pub async fn open_at(path: impl AsRef<Path>) -> Result<Self, PersistenceError> {
        let options = SqliteConnectOptions::new()
            .filename(path)
            .create_if_missing(true)
            .foreign_keys(true);

        Self::open_with_options(options).await
    }

    pub async fn open_in_memory() -> Result<Self, PersistenceError> {
        let options = SqliteConnectOptions::new()
            .filename(":memory:")
            .create_if_missing(true)
            .foreign_keys(true);

        Self::open_with_options(options).await
    }

    async fn open_with_options(options: SqliteConnectOptions) -> Result<Self, PersistenceError> {
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect_with(options)
            .await?;

        MIGRATOR.run(&pool).await?;

        Ok(Self { pool })
    }

    pub(crate) async fn load_rooms(&self) -> Result<Vec<RoomRecord>, PersistenceError> {
        let rows = sqlx::query(
            r#"
            SELECT
                rooms.id,
                rooms.name,
                rooms.media_id,
                rooms.media_title,
                rooms.created_at,
                COALESCE(playback_sessions.status, 'paused') AS status,
                COALESCE(playback_sessions.anchor_position_seconds, 0.0) AS anchor_position_seconds,
                COALESCE(playback_sessions.clock_updated_at, rooms.created_at) AS clock_updated_at,
                COALESCE(playback_sessions.playback_rate, 1.0) AS playback_rate
            FROM rooms
            LEFT JOIN playback_sessions
                ON playback_sessions.room_id = rooms.id
            ORDER BY rooms.name ASC
            "#,
        )
        .fetch_all(&self.pool)
        .await?;

        rows.into_iter().map(map_row_to_room_record).collect()
    }

    pub(crate) async fn save_room(&self, room: &RoomRecord) -> Result<(), PersistenceError> {
        let checkpoint = room.clock.checkpoint();
        let now = OffsetDateTime::now_utc();
        let mut transaction = self.pool.begin().await?;

        sqlx::query(
            r#"
            INSERT INTO rooms (id, name, media_id, media_title, created_at)
            VALUES (?1, ?2, ?3, ?4, ?5)
            ON CONFLICT(id) DO UPDATE SET
                name = excluded.name,
                media_id = excluded.media_id,
                media_title = excluded.media_title
            "#,
        )
        .bind(room.id.to_string())
        .bind(&room.name)
        .bind(room.media_id.map(|id| id.to_string()))
        .bind(&room.media_title)
        .bind(&room.created_at)
        .execute(&mut *transaction)
        .await?;

        sqlx::query(
            r#"
            INSERT INTO playback_sessions (
                room_id,
                status,
                anchor_position_seconds,
                clock_updated_at,
                playback_rate,
                updated_at
            )
            VALUES (?1, ?2, ?3, ?4, ?5, ?6)
            ON CONFLICT(room_id) DO UPDATE SET
                status = excluded.status,
                anchor_position_seconds = excluded.anchor_position_seconds,
                clock_updated_at = excluded.clock_updated_at,
                playback_rate = excluded.playback_rate,
                updated_at = excluded.updated_at
            "#,
        )
        .bind(room.id.to_string())
        .bind(playback_status_to_str(checkpoint.status))
        .bind(checkpoint.anchor_position_seconds)
        .bind(format_timestamp(checkpoint.updated_at))
        .bind(checkpoint.playback_rate)
        .bind(format_timestamp(now))
        .execute(&mut *transaction)
        .await?;

        transaction.commit().await?;

        Ok(())
    }

    pub(crate) async fn load_library_snapshot(&self) -> Result<LibrarySnapshot, PersistenceError> {
        let root_rows = sqlx::query(
            r#"
            SELECT path, last_scanned_at, last_scan_error
            FROM library_roots
            ORDER BY path ASC
            "#,
        )
        .fetch_all(&self.pool)
        .await?;
        let item_rows = sqlx::query(
            r#"
            SELECT
                id,
                root_path,
                relative_path,
                file_name,
                extension,
                size_bytes,
                modified_at,
                indexed_at,
                content_type,
                duration_seconds,
                container_name,
                video_codec,
                audio_codec,
                width,
                height,
                probed_at,
                probe_error,
                subtitle_tracks_json,
                thumbnail_generated_at,
                thumbnail_error,
                playback_mode,
                video_profile,
                video_level,
                video_pix_fmt,
                video_bit_depth,
                audio_streams_json,
                subtitle_streams_json
            FROM media_items
            ORDER BY root_path ASC, relative_path ASC
            "#,
        )
        .fetch_all(&self.pool)
        .await?;

        Ok(LibrarySnapshot {
            roots: root_rows
                .into_iter()
                .map(map_row_to_library_root)
                .collect::<Result<Vec<_>, _>>()?,
            items: item_rows
                .into_iter()
                .map(map_row_to_media_item)
                .collect::<Result<Vec<_>, _>>()?,
        })
    }

    pub(crate) async fn find_media_item(
        &self,
        media_id: Uuid,
    ) -> Result<Option<MediaItem>, PersistenceError> {
        let row = sqlx::query(
            r#"
            SELECT
                id,
                root_path,
                relative_path,
                file_name,
                extension,
                size_bytes,
                modified_at,
                indexed_at,
                content_type,
                duration_seconds,
                container_name,
                video_codec,
                audio_codec,
                width,
                height,
                probed_at,
                probe_error,
                subtitle_tracks_json,
                thumbnail_generated_at,
                thumbnail_error,
                playback_mode,
                video_profile,
                video_level,
                video_pix_fmt,
                video_bit_depth,
                audio_streams_json,
                subtitle_streams_json
            FROM media_items
            WHERE id = ?1
            "#,
        )
        .bind(media_id.to_string())
        .fetch_optional(&self.pool)
        .await?;

        row.map(map_row_to_media_item).transpose()
    }

    pub(crate) async fn sync_library_roots(
        &self,
        configured_paths: &[String],
    ) -> Result<(), PersistenceError> {
        let now = format_timestamp(OffsetDateTime::now_utc());
        let existing_paths = sqlx::query(
            r#"
            SELECT path
            FROM library_roots
            "#,
        )
        .fetch_all(&self.pool)
        .await?
        .into_iter()
        .map(|row| row.try_get::<String, _>("path"))
        .collect::<Result<Vec<_>, _>>()?;

        let mut transaction = self.pool.begin().await?;

        for path in configured_paths {
            sqlx::query(
                r#"
                INSERT INTO library_roots (path, created_at)
                VALUES (?1, ?2)
                ON CONFLICT(path) DO NOTHING
                "#,
            )
            .bind(path)
            .bind(&now)
            .execute(&mut *transaction)
            .await?;
        }

        for path in existing_paths {
            if configured_paths.contains(&path) {
                continue;
            }

            sqlx::query(
                r#"
                DELETE FROM library_roots
                WHERE path = ?1
                "#,
            )
            .bind(path)
            .execute(&mut *transaction)
            .await?;
        }

        transaction.commit().await?;

        Ok(())
    }

    /// Returns every persisted media row under `root_path`, keyed by
    /// relative path, so the scanner can both reuse stable UUIDs and skip
    /// re-probing files whose `(size_bytes, modified_at)` haven't changed.
    pub(crate) async fn existing_media_records(
        &self,
        root_path: &str,
    ) -> Result<HashMap<String, CachedMediaRecord>, PersistenceError> {
        let rows = sqlx::query(
            r#"
            SELECT
                id,
                relative_path,
                size_bytes,
                modified_at,
                duration_seconds,
                container_name,
                video_codec,
                audio_codec,
                width,
                height,
                probed_at,
                probe_error,
                playback_mode,
                video_profile,
                video_level,
                video_pix_fmt,
                video_bit_depth,
                audio_streams_json,
                subtitle_streams_json
            FROM media_items
            WHERE root_path = ?1
            "#,
        )
        .bind(root_path)
        .fetch_all(&self.pool)
        .await?;

        let mut mapping = HashMap::with_capacity(rows.len());

        for row in rows {
            let raw_id = row.try_get::<String, _>("id")?;
            let media_id = Uuid::parse_str(&raw_id).map_err(|error| {
                PersistenceError::InvalidData(format!(
                    "invalid stored media id '{raw_id}': {error}"
                ))
            })?;
            let size_bytes = row.try_get::<i64, _>("size_bytes")?;
            if size_bytes < 0 {
                return Err(PersistenceError::InvalidData(format!(
                    "invalid stored media size '{size_bytes}'"
                )));
            }
            let playback_mode_raw = row.try_get::<String, _>("playback_mode")?;
            let playback_mode =
                PlaybackMode::from_str_opt(&playback_mode_raw).ok_or_else(|| {
                    PersistenceError::InvalidData(format!(
                        "invalid stored playback_mode '{playback_mode_raw}'"
                    ))
                })?;
            let audio_streams_json = row.try_get::<String, _>("audio_streams_json")?;
            let subtitle_streams_json = row.try_get::<String, _>("subtitle_streams_json")?;
            let relative_path = row.try_get::<String, _>("relative_path")?;

            mapping.insert(
                relative_path,
                CachedMediaRecord {
                    id: media_id,
                    size_bytes: size_bytes as u64,
                    modified_at: row.try_get("modified_at")?,
                    duration_seconds: row.try_get("duration_seconds")?,
                    container_name: row.try_get("container_name")?,
                    video_codec: row.try_get("video_codec")?,
                    audio_codec: row.try_get("audio_codec")?,
                    width: parse_optional_u32(&row, "width")?,
                    height: parse_optional_u32(&row, "height")?,
                    probed_at: row.try_get("probed_at")?,
                    probe_error: row.try_get("probe_error")?,
                    playback_mode,
                    video_profile: row.try_get("video_profile")?,
                    video_level: parse_optional_u32(&row, "video_level")?,
                    video_pix_fmt: row.try_get("video_pix_fmt")?,
                    video_bit_depth: parse_optional_u8(&row, "video_bit_depth")?,
                    audio_streams: deserialize_audio_streams(&audio_streams_json)?,
                    subtitle_streams: deserialize_subtitle_streams(&subtitle_streams_json)?,
                },
            );
        }

        Ok(mapping)
    }

    /// Stage-1 (walk) write: upsert one row per discovered file. When the
    /// file's `(size, mtime)` matches an existing row whose probe didn't
    /// error, callers pass `cached_probe = Some(...)` and we preserve the
    /// stored probe fields verbatim — ffprobe never runs again. Otherwise
    /// callers pass `cached_probe = None` and we write NULL probe columns
    /// + reset thumbnail state, so the row is picked up by both the
    /// background probe pool and (after probe completes) the thumbnail
    /// pool.
    pub async fn upsert_walk_record(&self, record: &WalkRecord) -> Result<(), PersistenceError> {
        let (
            duration_seconds,
            container_name,
            video_codec,
            audio_codec,
            width,
            height,
            probed_at,
            probe_error,
            playback_mode,
            video_profile,
            video_level,
            video_pix_fmt,
            video_bit_depth,
            audio_streams_json,
            subtitle_streams_json,
        ) = match record.cached_probe.as_ref() {
            Some(cached) => (
                cached.duration_seconds,
                cached.container_name.clone(),
                cached.video_codec.clone(),
                cached.audio_codec.clone(),
                cached.width,
                cached.height,
                cached.probed_at.clone(),
                cached.probe_error.clone(),
                cached.playback_mode.as_str().to_string(),
                cached.video_profile.clone(),
                cached.video_level,
                cached.video_pix_fmt.clone(),
                cached.video_bit_depth,
                serialize_audio_streams(&cached.audio_streams)?,
                serialize_subtitle_streams(&cached.subtitle_streams)?,
            ),
            None => (
                None,
                None,
                None,
                None,
                None,
                None,
                None,
                None,
                // Sentinel for the NOT NULL column; will be overwritten
                // by the probe pool's `update_probe_metadata` call.
                PlaybackMode::Direct.as_str().to_string(),
                None,
                None,
                None,
                None,
                "[]".to_string(),
                "[]".to_string(),
            ),
        };

        let (thumbnail_generated_at, thumbnail_error) = match record.cached_thumbnail.as_ref() {
            Some((generated_at, error)) if record.cached_probe.is_some() => {
                (generated_at.clone(), error.clone())
            }
            // Cache miss (or only thumbnail data without probe data):
            // reset thumbnail state so the freshly-probed row gets a new
            // thumbnail pass that matches the new probe data.
            _ => (None, None),
        };

        sqlx::query(
            r#"
            INSERT INTO media_items (
                id,
                root_path,
                relative_path,
                file_name,
                extension,
                size_bytes,
                modified_at,
                indexed_at,
                content_type,
                duration_seconds,
                container_name,
                video_codec,
                audio_codec,
                width,
                height,
                probed_at,
                probe_error,
                subtitle_tracks_json,
                thumbnail_generated_at,
                thumbnail_error,
                playback_mode,
                video_profile,
                video_level,
                video_pix_fmt,
                video_bit_depth,
                audio_streams_json,
                subtitle_streams_json
            )
            VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18, ?19, ?20, ?21, ?22, ?23, ?24, ?25, ?26, ?27)
            ON CONFLICT(root_path, relative_path) DO UPDATE SET
                file_name = excluded.file_name,
                extension = excluded.extension,
                size_bytes = excluded.size_bytes,
                modified_at = excluded.modified_at,
                indexed_at = excluded.indexed_at,
                content_type = excluded.content_type,
                duration_seconds = excluded.duration_seconds,
                container_name = excluded.container_name,
                video_codec = excluded.video_codec,
                audio_codec = excluded.audio_codec,
                width = excluded.width,
                height = excluded.height,
                probed_at = excluded.probed_at,
                probe_error = excluded.probe_error,
                subtitle_tracks_json = excluded.subtitle_tracks_json,
                thumbnail_generated_at = excluded.thumbnail_generated_at,
                thumbnail_error = excluded.thumbnail_error,
                playback_mode = excluded.playback_mode,
                video_profile = excluded.video_profile,
                video_level = excluded.video_level,
                video_pix_fmt = excluded.video_pix_fmt,
                video_bit_depth = excluded.video_bit_depth,
                audio_streams_json = excluded.audio_streams_json,
                subtitle_streams_json = excluded.subtitle_streams_json
            "#,
        )
        .bind(record.id.to_string())
        .bind(&record.root_path)
        .bind(&record.relative_path)
        .bind(&record.file_name)
        .bind(&record.extension)
        .bind(record.size_bytes as i64)
        .bind(&record.modified_at)
        .bind(&record.indexed_at)
        .bind(&record.content_type)
        .bind(duration_seconds)
        .bind(&container_name)
        .bind(&video_codec)
        .bind(&audio_codec)
        .bind(width.map(i64::from))
        .bind(height.map(i64::from))
        .bind(&probed_at)
        .bind(&probe_error)
        .bind(serialize_subtitle_tracks(&record.subtitle_tracks)?)
        .bind(&thumbnail_generated_at)
        .bind(&thumbnail_error)
        .bind(playback_mode)
        .bind(&video_profile)
        .bind(video_level.map(i64::from))
        .bind(&video_pix_fmt)
        .bind(video_bit_depth.map(i64::from))
        .bind(audio_streams_json)
        .bind(subtitle_streams_json)
        .execute(&self.pool)
        .await?;

        Ok(())
    }

    /// Delete every `media_items` row under `root_path` whose UUID is not
    /// in `kept_ids`. Called after a walk completes so files removed from
    /// disk drop out of the index in one round trip.
    pub async fn prune_missing_paths(
        &self,
        root_path: &str,
        kept_ids: &[Uuid],
    ) -> Result<usize, PersistenceError> {
        if kept_ids.is_empty() {
            // Wipe the root entirely — no kept rows means everything was
            // removed (or the root is empty). The bound-parameter path
            // can't express `IN ()` so handle this case directly.
            let result = sqlx::query(
                r#"
                DELETE FROM media_items
                WHERE root_path = ?1
                "#,
            )
            .bind(root_path)
            .execute(&self.pool)
            .await?;
            return Ok(result.rows_affected() as usize);
        }

        // SQLite imposes a default limit of 999 bound parameters per
        // statement, which a single `IN (...)` over thousands of ids would
        // exceed. Cheapest correct approach: SELECT all current ids, diff
        // in Rust, DELETE the missing set in chunks.
        let mut total_removed = 0usize;
        let existing_ids: Vec<String> = sqlx::query(
            r#"
            SELECT id FROM media_items WHERE root_path = ?1
            "#,
        )
        .bind(root_path)
        .fetch_all(&self.pool)
        .await?
        .into_iter()
        .map(|row| row.try_get::<String, _>("id"))
        .collect::<Result<Vec<_>, _>>()?;

        let kept_set: std::collections::HashSet<String> =
            kept_ids.iter().map(|id| id.to_string()).collect();
        let to_delete: Vec<String> = existing_ids
            .into_iter()
            .filter(|id| !kept_set.contains(id))
            .collect();

        for chunk in to_delete.chunks(500) {
            let placeholders = (0..chunk.len())
                .map(|index| format!("?{}", index + 1))
                .collect::<Vec<_>>()
                .join(", ");
            let sql = format!("DELETE FROM media_items WHERE id IN ({})", placeholders);
            let mut query = sqlx::query(&sql);
            for id in chunk {
                query = query.bind(id);
            }
            let result = query.execute(&self.pool).await?;
            total_removed += result.rows_affected() as usize;
        }

        Ok(total_removed)
    }

    /// Mark a library root as freshly scanned (success). Clears any
    /// previously-recorded scan error.
    pub async fn mark_root_scanned(&self, root_path: &str) -> Result<(), PersistenceError> {
        sqlx::query(
            r#"
            UPDATE library_roots
            SET
                last_scanned_at = ?1,
                last_scan_error = NULL
            WHERE path = ?2
            "#,
        )
        .bind(format_timestamp(OffsetDateTime::now_utc()))
        .bind(root_path)
        .execute(&self.pool)
        .await?;

        Ok(())
    }

    /// Stage-2 (probe) write: persist the outcome of a single probe job.
    /// Successful probes also reset thumbnail state so the thumbnail pool
    /// regenerates the cached jpeg using the freshly-discovered video
    /// codec / duration. Failed probes leave thumbnail state alone — the
    /// thumbnail pool will skip the row anyway since `probed_at IS NOT
    /// NULL` won't pass the pending filter once `probe_error IS NOT
    /// NULL`.
    pub async fn update_probe_metadata(
        &self,
        media_id: Uuid,
        outcome: &ProbeOutcome,
    ) -> Result<(), PersistenceError> {
        sqlx::query(
            r#"
            UPDATE media_items
            SET
                duration_seconds = ?1,
                container_name = ?2,
                video_codec = ?3,
                audio_codec = ?4,
                width = ?5,
                height = ?6,
                probed_at = ?7,
                probe_error = ?8,
                playback_mode = ?9,
                video_profile = ?10,
                video_level = ?11,
                video_pix_fmt = ?12,
                video_bit_depth = ?13,
                audio_streams_json = ?14,
                subtitle_streams_json = ?15,
                thumbnail_generated_at = NULL,
                thumbnail_error = NULL
            WHERE id = ?16
            "#,
        )
        .bind(outcome.duration_seconds)
        .bind(&outcome.container_name)
        .bind(&outcome.video_codec)
        .bind(&outcome.audio_codec)
        .bind(outcome.width.map(i64::from))
        .bind(outcome.height.map(i64::from))
        .bind(&outcome.probed_at)
        .bind(&outcome.probe_error)
        .bind(outcome.playback_mode.as_str())
        .bind(&outcome.video_profile)
        .bind(outcome.video_level.map(i64::from))
        .bind(&outcome.video_pix_fmt)
        .bind(outcome.video_bit_depth.map(i64::from))
        .bind(serialize_audio_streams(&outcome.audio_streams)?)
        .bind(serialize_subtitle_streams(&outcome.subtitle_streams)?)
        .bind(media_id.to_string())
        .execute(&self.pool)
        .await?;

        Ok(())
    }

    /// Returns every media item that hasn't been probed yet — i.e. the
    /// probe pool's restart queue. Both `probed_at IS NULL` and
    /// `probe_error IS NULL` is the encoding for "still pending".
    pub async fn list_pending_probes(&self) -> Result<Vec<PendingProbe>, PersistenceError> {
        let rows = sqlx::query(
            r#"
            SELECT id, root_path, relative_path, extension
            FROM media_items
            WHERE probed_at IS NULL
              AND probe_error IS NULL
            ORDER BY root_path ASC, relative_path ASC
            "#,
        )
        .fetch_all(&self.pool)
        .await?;

        rows.into_iter()
            .map(|row| {
                let raw_id = row.try_get::<String, _>("id")?;
                let media_id = Uuid::parse_str(&raw_id).map_err(|error| {
                    PersistenceError::InvalidData(format!(
                        "invalid stored media id '{raw_id}': {error}"
                    ))
                })?;

                Ok(PendingProbe {
                    media_id,
                    root_path: row.try_get("root_path")?,
                    relative_path: row.try_get("relative_path")?,
                    extension: row.try_get("extension")?,
                })
            })
            .collect()
    }

    /// Persist the keyframe index for a media item. Replaces the previous
    /// set atomically: on a re-probe (file changed, or a manual refresh)
    /// we want the new list to be the sole source of truth, not merged
    /// with stale offsets from the prior run.
    ///
    /// Stamps `keyframes_probed_at` on the parent row using the timestamp
    /// the caller passes in — reuses the main probe's `probed_at` so the
    /// two timestamps line up (they come from the same scan attempt).
    pub async fn update_keyframes(
        &self,
        media_id: Uuid,
        offsets: &[f64],
        probed_at: &str,
    ) -> Result<(), PersistenceError> {
        let mut tx = self.pool.begin().await?;

        sqlx::query("DELETE FROM media_keyframes WHERE media_id = ?1")
            .bind(media_id.to_string())
            .execute(&mut *tx)
            .await?;

        // Batched inserts keep the transaction small even for films with
        // thousands of keyframes. We cap each INSERT at ~500 rows to stay
        // well under SQLite's default 999-parameter limit (2 params per
        // row = ~500 rows).
        const CHUNK: usize = 500;
        let media_id_str = media_id.to_string();
        for (chunk_idx, chunk) in offsets.chunks(CHUNK).enumerate() {
            let mut query = String::from(
                "INSERT INTO media_keyframes (media_id, kf_index, pts_seconds) VALUES ",
            );
            for i in 0..chunk.len() {
                if i > 0 {
                    query.push_str(", ");
                }
                query.push_str("(?, ?, ?)");
            }
            let mut q = sqlx::query(&query);
            for (i, offset) in chunk.iter().enumerate() {
                let global_index = (chunk_idx * CHUNK + i) as i64;
                q = q.bind(&media_id_str).bind(global_index).bind(*offset);
            }
            q.execute(&mut *tx).await?;
        }

        sqlx::query("UPDATE media_items SET keyframes_probed_at = ?1 WHERE id = ?2")
            .bind(probed_at)
            .bind(&media_id_str)
            .execute(&mut *tx)
            .await?;

        tx.commit().await?;
        Ok(())
    }

    /// Load the keyframe PTS offsets for a media item, ordered ascending.
    /// Returns an empty vec when no keyframes have been indexed yet (a
    /// caller reads that as "fall back to linear-only playback"), or when
    /// the media genuinely has no keyframes (audio-only).
    pub async fn load_keyframes(&self, media_id: Uuid) -> Result<Vec<f64>, PersistenceError> {
        let rows = sqlx::query(
            r#"
            SELECT pts_seconds
            FROM media_keyframes
            WHERE media_id = ?1
            ORDER BY kf_index ASC
            "#,
        )
        .bind(media_id.to_string())
        .fetch_all(&self.pool)
        .await?;

        rows.into_iter()
            .map(|row| row.try_get::<f64, _>("pts_seconds").map_err(Into::into))
            .collect()
    }

    /// Persist the outcome of a single thumbnail job. Either `generated_at`
    /// is set (success) or `error` is set (failure); both `None` reverts the
    /// row to "pending" which lets a future worker pass retry it.
    pub async fn update_thumbnail_state(
        &self,
        media_id: Uuid,
        generated_at: Option<&str>,
        error: Option<&str>,
    ) -> Result<(), PersistenceError> {
        sqlx::query(
            r#"
            UPDATE media_items
            SET thumbnail_generated_at = ?1,
                thumbnail_error = ?2
            WHERE id = ?3
            "#,
        )
        .bind(generated_at)
        .bind(error)
        .bind(media_id.to_string())
        .execute(&self.pool)
        .await?;

        Ok(())
    }

    /// Returns every media item that currently has neither a generated
    /// thumbnail nor a recorded error — i.e. the worker pool's restart
    /// queue.
    pub async fn list_pending_thumbnails(&self) -> Result<Vec<PendingThumbnail>, PersistenceError> {
        let rows = sqlx::query(
            r#"
            SELECT id, root_path, relative_path, video_codec, duration_seconds
            FROM media_items
            WHERE thumbnail_generated_at IS NULL
              AND thumbnail_error IS NULL
              AND probed_at IS NOT NULL
            ORDER BY root_path ASC, relative_path ASC
            "#,
        )
        .fetch_all(&self.pool)
        .await?;

        rows.into_iter()
            .map(|row| {
                let raw_id = row.try_get::<String, _>("id")?;
                let media_id = Uuid::parse_str(&raw_id).map_err(|error| {
                    PersistenceError::InvalidData(format!(
                        "invalid stored media id '{raw_id}': {error}"
                    ))
                })?;

                Ok(PendingThumbnail {
                    media_id,
                    root_path: row.try_get("root_path")?,
                    relative_path: row.try_get("relative_path")?,
                    video_codec: row.try_get("video_codec")?,
                    duration_seconds: row.try_get("duration_seconds")?,
                })
            })
            .collect()
    }

    pub(crate) async fn record_library_scan_failure(
        &self,
        root_path: &str,
        error_message: &str,
    ) -> Result<(), PersistenceError> {
        let mut transaction = self.pool.begin().await?;

        sqlx::query(
            r#"
            UPDATE library_roots
            SET
                last_scanned_at = ?1,
                last_scan_error = ?2
            WHERE path = ?3
            "#,
        )
        .bind(format_timestamp(OffsetDateTime::now_utc()))
        .bind(error_message)
        .bind(root_path)
        .execute(&mut *transaction)
        .await?;

        sqlx::query(
            r#"
            DELETE FROM media_items
            WHERE root_path = ?1
            "#,
        )
        .bind(root_path)
        .execute(&mut *transaction)
        .await?;

        transaction.commit().await?;

        Ok(())
    }
}

impl fmt::Display for PersistenceError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Sqlx(error) => write!(f, "{error}"),
            Self::Migrate(error) => write!(f, "{error}"),
            Self::InvalidData(message) => f.write_str(message),
        }
    }
}

impl Error for PersistenceError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Sqlx(error) => Some(error),
            Self::Migrate(error) => Some(error),
            Self::InvalidData(_) => None,
        }
    }
}

impl From<sqlx::Error> for PersistenceError {
    fn from(value: sqlx::Error) -> Self {
        Self::Sqlx(value)
    }
}

impl From<sqlx::migrate::MigrateError> for PersistenceError {
    fn from(value: sqlx::migrate::MigrateError) -> Self {
        Self::Migrate(value)
    }
}

fn map_row_to_room_record(row: sqlx::sqlite::SqliteRow) -> Result<RoomRecord, PersistenceError> {
    let room_id = row.try_get::<String, _>("id")?;
    let created_at = row.try_get::<String, _>("created_at")?;
    let clock_updated_at = row.try_get::<String, _>("clock_updated_at")?;
    let playback_status = row.try_get::<String, _>("status")?;
    let raw_media_id = row.try_get::<Option<String>, _>("media_id")?;

    let media_id = match raw_media_id {
        Some(value) => Some(Uuid::parse_str(&value).map_err(|error| {
            PersistenceError::InvalidData(format!(
                "invalid stored room media id '{value}': {error}"
            ))
        })?),
        None => None,
    };

    Ok(RoomRecord {
        id: Uuid::parse_str(&room_id).map_err(|error| {
            PersistenceError::InvalidData(format!("invalid stored room id '{room_id}': {error}"))
        })?,
        name: row.try_get("name")?,
        media_id,
        media_title: row.try_get("media_title")?,
        created_at,
        clock: AuthoritativePlaybackClock::restore(PlaybackClockCheckpoint {
            status: parse_playback_status(&playback_status)?,
            anchor_position_seconds: row.try_get("anchor_position_seconds")?,
            updated_at: parse_timestamp(&clock_updated_at)?,
            playback_rate: row.try_get("playback_rate")?,
        }),
    })
}

fn map_row_to_library_root(row: sqlx::sqlite::SqliteRow) -> Result<LibraryRoot, PersistenceError> {
    Ok(LibraryRoot {
        path: row.try_get("path")?,
        last_scanned_at: row.try_get("last_scanned_at")?,
        last_scan_error: row.try_get("last_scan_error")?,
    })
}

fn map_row_to_media_item(row: sqlx::sqlite::SqliteRow) -> Result<MediaItem, PersistenceError> {
    let id = row.try_get::<String, _>("id")?;
    let size_bytes = row.try_get::<i64, _>("size_bytes")?;
    let subtitle_tracks_json = row.try_get::<String, _>("subtitle_tracks_json")?;
    let audio_streams_json = row.try_get::<String, _>("audio_streams_json")?;
    let subtitle_streams_json = row.try_get::<String, _>("subtitle_streams_json")?;
    let playback_mode_raw = row.try_get::<String, _>("playback_mode")?;
    let playback_mode = PlaybackMode::from_str_opt(&playback_mode_raw).ok_or_else(|| {
        PersistenceError::InvalidData(format!(
            "invalid stored playback_mode '{playback_mode_raw}'"
        ))
    })?;
    let media_uuid = Uuid::parse_str(&id).map_err(|error| {
        PersistenceError::InvalidData(format!("invalid stored media id '{id}': {error}"))
    })?;
    let hls_master_url = if playback_mode.is_hls() {
        Some(format!("/api/media/{media_uuid}/hls/master.m3u8"))
    } else {
        None
    };

    if size_bytes < 0 {
        return Err(PersistenceError::InvalidData(format!(
            "invalid stored media size '{size_bytes}'"
        )));
    }

    Ok(MediaItem {
        id: media_uuid,
        root_path: row.try_get("root_path")?,
        relative_path: row.try_get("relative_path")?,
        file_name: row.try_get("file_name")?,
        extension: row.try_get("extension")?,
        size_bytes: size_bytes as u64,
        modified_at: row.try_get("modified_at")?,
        indexed_at: row.try_get("indexed_at")?,
        content_type: row.try_get("content_type")?,
        duration_seconds: row.try_get("duration_seconds")?,
        container_name: row.try_get("container_name")?,
        video_codec: row.try_get("video_codec")?,
        audio_codec: row.try_get("audio_codec")?,
        width: parse_optional_u32(&row, "width")?,
        height: parse_optional_u32(&row, "height")?,
        probed_at: row.try_get("probed_at")?,
        probe_error: row.try_get("probe_error")?,
        subtitle_tracks: deserialize_subtitle_tracks(&subtitle_tracks_json)?,
        thumbnail_generated_at: row.try_get("thumbnail_generated_at")?,
        thumbnail_error: row.try_get("thumbnail_error")?,
        playback_mode,
        video_profile: row.try_get("video_profile")?,
        video_level: parse_optional_u32(&row, "video_level")?,
        video_pix_fmt: row.try_get("video_pix_fmt")?,
        video_bit_depth: parse_optional_u8(&row, "video_bit_depth")?,
        audio_streams: deserialize_audio_streams(&audio_streams_json)?,
        subtitle_streams: deserialize_subtitle_streams(&subtitle_streams_json)?,
        hls_master_url,
    })
}

fn serialize_audio_streams(streams: &[AudioStream]) -> Result<String, PersistenceError> {
    serde_json::to_string(streams).map_err(|error| {
        PersistenceError::InvalidData(format!("failed to serialize audio streams: {error}"))
    })
}

fn deserialize_audio_streams(value: &str) -> Result<Vec<AudioStream>, PersistenceError> {
    serde_json::from_str(value).map_err(|error| {
        PersistenceError::InvalidData(format!("invalid stored audio stream JSON: {error}"))
    })
}

fn serialize_subtitle_streams(streams: &[SubtitleStream]) -> Result<String, PersistenceError> {
    serde_json::to_string(streams).map_err(|error| {
        PersistenceError::InvalidData(format!("failed to serialize subtitle streams: {error}"))
    })
}

fn deserialize_subtitle_streams(value: &str) -> Result<Vec<SubtitleStream>, PersistenceError> {
    serde_json::from_str(value).map_err(|error| {
        PersistenceError::InvalidData(format!("invalid stored subtitle stream JSON: {error}"))
    })
}

fn parse_optional_u8(
    row: &sqlx::sqlite::SqliteRow,
    column: &str,
) -> Result<Option<u8>, PersistenceError> {
    let value = row.try_get::<Option<i64>, _>(column)?;

    match value {
        Some(value) if value < 0 => Err(PersistenceError::InvalidData(format!(
            "invalid stored {column} '{value}'"
        ))),
        Some(value) => u8::try_from(value).map(Some).map_err(|error| {
            PersistenceError::InvalidData(format!("invalid stored {column} '{value}': {error}"))
        }),
        None => Ok(None),
    }
}

fn serialize_subtitle_tracks(tracks: &[SubtitleTrack]) -> Result<String, PersistenceError> {
    serde_json::to_string(tracks).map_err(|error| {
        PersistenceError::InvalidData(format!("failed to serialize subtitle tracks: {error}"))
    })
}

fn deserialize_subtitle_tracks(value: &str) -> Result<Vec<SubtitleTrack>, PersistenceError> {
    serde_json::from_str(value).map_err(|error| {
        PersistenceError::InvalidData(format!("invalid stored subtitle track JSON: {error}"))
    })
}

fn parse_optional_u32(
    row: &sqlx::sqlite::SqliteRow,
    column: &str,
) -> Result<Option<u32>, PersistenceError> {
    let value = row.try_get::<Option<i64>, _>(column)?;

    match value {
        Some(value) if value < 0 => Err(PersistenceError::InvalidData(format!(
            "invalid stored {column} '{value}'"
        ))),
        Some(value) => u32::try_from(value).map(Some).map_err(|error| {
            PersistenceError::InvalidData(format!("invalid stored {column} '{value}': {error}"))
        }),
        None => Ok(None),
    }
}

fn playback_status_to_str(status: PlaybackStatus) -> &'static str {
    match status {
        PlaybackStatus::Playing => "playing",
        PlaybackStatus::Paused => "paused",
    }
}

fn parse_playback_status(value: &str) -> Result<PlaybackStatus, PersistenceError> {
    match value {
        "playing" => Ok(PlaybackStatus::Playing),
        "paused" => Ok(PlaybackStatus::Paused),
        other => Err(PersistenceError::InvalidData(format!(
            "invalid stored playback status '{other}'"
        ))),
    }
}

fn parse_timestamp(value: &str) -> Result<OffsetDateTime, PersistenceError> {
    OffsetDateTime::parse(value, &Rfc3339).map_err(|error| {
        PersistenceError::InvalidData(format!(
            "invalid stored RFC3339 timestamp '{value}': {error}"
        ))
    })
}
