use uuid::Uuid;

use sqlx::Row;

use super::{
    PendingProbe, Persistence, PersistenceError, ProbeOutcome, bump_library_revision,
    rows::{serialize_audio_streams, serialize_subtitle_streams},
};

impl Persistence {
    /// Stage-2 (probe) write: persist the outcome of a single probe job.
    /// Successful probes also reset thumbnail state so the thumbnail pool
    /// regenerates the cached jpeg using the freshly-discovered video
    /// codec / duration. Failed probes leave thumbnail state alone - the
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
        bump_library_revision(&self.pool).await?;

        Ok(())
    }

    /// Returns every media item that hasn't been probed yet - i.e. the
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
}
