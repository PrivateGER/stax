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
    library::ScannedMediaItem,
    protocol::{LibraryRoot, MediaItem, PlaybackStatus, SubtitleTrack},
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
                thumbnail_error
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
                thumbnail_error
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

    pub(crate) async fn existing_media_ids(
        &self,
        root_path: &str,
    ) -> Result<HashMap<String, Uuid>, PersistenceError> {
        let rows = sqlx::query(
            r#"
            SELECT id, relative_path
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
            let relative_path = row.try_get::<String, _>("relative_path")?;
            let media_id = Uuid::parse_str(&raw_id).map_err(|error| {
                PersistenceError::InvalidData(format!(
                    "invalid stored media id '{raw_id}': {error}"
                ))
            })?;

            mapping.insert(relative_path, media_id);
        }

        Ok(mapping)
    }

    pub(crate) async fn replace_library_scan(
        &self,
        root_path: &str,
        items: &[ScannedMediaItem],
    ) -> Result<(), PersistenceError> {
        let existing_relative_paths = sqlx::query(
            r#"
            SELECT relative_path
            FROM media_items
            WHERE root_path = ?1
            "#,
        )
        .bind(root_path)
        .fetch_all(&self.pool)
        .await?
        .into_iter()
        .map(|row| row.try_get::<String, _>("relative_path"))
        .collect::<Result<Vec<_>, _>>()?;
        let mut transaction = self.pool.begin().await?;

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
        .execute(&mut *transaction)
        .await?;

        for item in items {
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
                    thumbnail_error
                )
                VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18, ?19, ?20)
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
                    thumbnail_error = excluded.thumbnail_error
                "#,
            )
            .bind(item.id.to_string())
            .bind(&item.root_path)
            .bind(&item.relative_path)
            .bind(&item.file_name)
            .bind(&item.extension)
            .bind(item.size_bytes as i64)
            .bind(&item.modified_at)
            .bind(&item.indexed_at)
            .bind(&item.content_type)
            .bind(item.duration_seconds)
            .bind(&item.container_name)
            .bind(&item.video_codec)
            .bind(&item.audio_codec)
            .bind(item.width.map(i64::from))
            .bind(item.height.map(i64::from))
            .bind(&item.probed_at)
            .bind(&item.probe_error)
            .bind(serialize_subtitle_tracks(&item.subtitle_tracks)?)
            .bind(&item.thumbnail_generated_at)
            .bind(&item.thumbnail_error)
            .execute(&mut *transaction)
            .await?;
        }

        for relative_path in existing_relative_paths {
            if items.iter().any(|item| item.relative_path == relative_path) {
                continue;
            }

            sqlx::query(
                r#"
                DELETE FROM media_items
                WHERE root_path = ?1 AND relative_path = ?2
                "#,
            )
            .bind(root_path)
            .bind(relative_path)
            .execute(&mut *transaction)
            .await?;
        }

        transaction.commit().await?;

        Ok(())
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

    if size_bytes < 0 {
        return Err(PersistenceError::InvalidData(format!(
            "invalid stored media size '{size_bytes}'"
        )));
    }

    Ok(MediaItem {
        id: Uuid::parse_str(&id).map_err(|error| {
            PersistenceError::InvalidData(format!("invalid stored media id '{id}': {error}"))
        })?,
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
    })
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
