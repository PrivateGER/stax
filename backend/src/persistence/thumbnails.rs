use uuid::Uuid;

use sqlx::Row;

use super::{PendingThumbnail, Persistence, PersistenceError, bump_library_revision};

impl Persistence {
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
        bump_library_revision(&self.pool).await?;

        Ok(())
    }

    /// Returns every media item that currently has neither a generated
    /// thumbnail nor a recorded error - i.e. the worker pool's restart
    /// queue.
    pub async fn list_pending_thumbnails(&self) -> Result<Vec<PendingThumbnail>, PersistenceError> {
        let rows = sqlx::query(
            r#"
            SELECT id, root_path, relative_path, video_codec, duration_seconds
            FROM media_items
            WHERE thumbnail_generated_at IS NULL
              AND thumbnail_error IS NULL
              AND probed_at IS NOT NULL
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
}
