use time::OffsetDateTime;

use sqlx::Row;

use crate::{clock::format_timestamp, protocol::StreamCopyStatus};

use super::{
    LibrarySnapshot, LibraryStatusSnapshot, Persistence, PersistenceError, bump_library_revision,
    rows::{map_row_to_library_root, map_row_to_media_summary},
};

impl Persistence {
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
                duration_seconds,
                probe_error,
                subtitle_tracks_json,
                thumbnail_generated_at,
                thumbnail_error,
                playback_mode,
                audio_streams_json,
                subtitle_streams_json,
                stream_copies.source_size_bytes AS stream_copy_source_size_bytes,
                stream_copies.source_modified_at AS stream_copy_source_modified_at,
                stream_copies.status AS stream_copy_status,
                stream_copies.audio_stream_index AS stream_copy_audio_stream_index,
                stream_copies.subtitle_mode AS stream_copy_subtitle_mode,
                stream_copies.subtitle_kind AS stream_copy_subtitle_kind,
                stream_copies.subtitle_index AS stream_copy_subtitle_index,
                stream_copies.output_path AS stream_copy_output_path,
                stream_copies.output_content_type AS stream_copy_output_content_type,
                stream_copies.subtitle_path AS stream_copy_subtitle_path,
                stream_copies.error AS stream_copy_error,
                stream_copies.created_at AS stream_copy_created_at,
                stream_copies.updated_at AS stream_copy_updated_at
            FROM media_items
            LEFT JOIN stream_copies ON stream_copies.media_id = media_items.id
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
                .map(map_row_to_media_summary)
                .collect::<Result<Vec<_>, _>>()?,
        })
    }

    pub(crate) async fn load_library_status(
        &self,
    ) -> Result<LibraryStatusSnapshot, PersistenceError> {
        let row = sqlx::query(
            r#"
            SELECT
                revision,
                EXISTS(
                    SELECT 1
                    FROM media_items
                    WHERE probed_at IS NULL
                      AND probe_error IS NULL
                ) AS has_pending_probes,
                EXISTS(
                    SELECT 1
                    FROM media_items
                    WHERE thumbnail_generated_at IS NULL
                      AND thumbnail_error IS NULL
                      AND probed_at IS NOT NULL
                      AND probe_error IS NULL
                ) AS has_pending_thumbnails,
                EXISTS(
                    SELECT 1
                    FROM stream_copies
                    INNER JOIN media_items ON media_items.id = stream_copies.media_id
                    WHERE stream_copies.source_size_bytes = media_items.size_bytes
                      AND stream_copies.source_modified_at = media_items.modified_at
                      AND stream_copies.status IN (?1, ?2)
                ) AS has_pending_stream_copies
            FROM library_state
            WHERE singleton = 1
            "#,
        )
        .bind(StreamCopyStatus::Queued.as_str())
        .bind(StreamCopyStatus::Running.as_str())
        .fetch_one(&self.pool)
        .await?;

        let revision = row.try_get::<i64, _>("revision")?;
        if revision < 0 {
            return Err(PersistenceError::InvalidData(format!(
                "invalid stored library revision '{revision}'"
            )));
        }

        let has_pending_probes = row.try_get::<i64, _>("has_pending_probes")? != 0;
        let has_pending_thumbnails = row.try_get::<i64, _>("has_pending_thumbnails")? != 0;
        let has_pending_stream_copies = row.try_get::<i64, _>("has_pending_stream_copies")? != 0;

        Ok(LibraryStatusSnapshot {
            revision: revision as u64,
            has_pending_background_work: has_pending_probes
                || has_pending_thumbnails
                || has_pending_stream_copies,
        })
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

        let mut changed = false;

        for path in configured_paths {
            let result = sqlx::query(
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
            changed |= result.rows_affected() > 0;
        }

        for path in existing_paths {
            if configured_paths.contains(&path) {
                continue;
            }

            let result = sqlx::query(
                r#"
                DELETE FROM library_roots
                WHERE path = ?1
                "#,
            )
            .bind(path)
            .execute(&mut *transaction)
            .await?;
            changed |= result.rows_affected() > 0;
        }

        if changed {
            bump_library_revision(&mut *transaction).await?;
        }

        transaction.commit().await?;

        Ok(())
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
        bump_library_revision(&self.pool).await?;

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

        bump_library_revision(&mut *transaction).await?;

        transaction.commit().await?;

        Ok(())
    }
}
