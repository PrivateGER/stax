use uuid::Uuid;

use sqlx::Row;

use crate::protocol::{
    StreamCopyStatus, StreamCopySubtitleSelection, StreamCopySummary, SubtitleSourceKind,
};

use super::{
    PendingStreamCopy, Persistence, PersistenceError, StreamCopyRecord, StreamCopyRequestRecord,
    bump_library_revision, rows::map_row_to_stream_copy_record,
};

impl Persistence {
    pub async fn find_stream_copy(
        &self,
        media_id: Uuid,
    ) -> Result<Option<StreamCopyRecord>, PersistenceError> {
        let row = sqlx::query(
            r#"
            SELECT
                media_id,
                source_size_bytes,
                source_modified_at,
                status,
                audio_stream_index,
                subtitle_mode,
                subtitle_kind,
                subtitle_index,
                output_path,
                output_content_type,
                subtitle_path,
                error,
                created_at,
                updated_at
            FROM stream_copies
            WHERE media_id = ?1
            "#,
        )
        .bind(media_id.to_string())
        .fetch_optional(&self.pool)
        .await?;

        row.map(map_row_to_stream_copy_record).transpose()
    }

    pub async fn upsert_stream_copy_request(
        &self,
        request: &StreamCopyRequestRecord,
    ) -> Result<(), PersistenceError> {
        sqlx::query(
            r#"
            INSERT INTO stream_copies (
                media_id,
                source_size_bytes,
                source_modified_at,
                status,
                audio_stream_index,
                subtitle_mode,
                subtitle_kind,
                subtitle_index,
                output_path,
                output_content_type,
                subtitle_path,
                error,
                created_at,
                updated_at
            )
            VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, NULL, NULL, NULL, NULL, ?9, ?10)
            ON CONFLICT(media_id) DO UPDATE SET
                source_size_bytes = excluded.source_size_bytes,
                source_modified_at = excluded.source_modified_at,
                status = excluded.status,
                audio_stream_index = excluded.audio_stream_index,
                subtitle_mode = excluded.subtitle_mode,
                subtitle_kind = excluded.subtitle_kind,
                subtitle_index = excluded.subtitle_index,
                output_path = NULL,
                output_content_type = NULL,
                subtitle_path = NULL,
                error = NULL,
                updated_at = excluded.updated_at
            "#,
        )
        .bind(request.media_id.to_string())
        .bind(request.source_size_bytes as i64)
        .bind(&request.source_modified_at)
        .bind(StreamCopyStatus::Queued.as_str())
        .bind(request.audio_stream_index.map(i64::from))
        .bind(request.subtitle_mode.as_str())
        .bind(request.subtitle_kind.map(SubtitleSourceKind::as_str))
        .bind(request.subtitle_index.map(i64::from))
        .bind(&request.updated_at)
        .bind(&request.updated_at)
        .execute(&self.pool)
        .await?;
        bump_library_revision(&self.pool).await?;

        Ok(())
    }

    pub async fn mark_stream_copy_running(
        &self,
        media_id: Uuid,
        updated_at: &str,
    ) -> Result<(), PersistenceError> {
        sqlx::query(
            r#"
            UPDATE stream_copies
            SET status = ?1,
                error = NULL,
                updated_at = ?2
            WHERE media_id = ?3
            "#,
        )
        .bind(StreamCopyStatus::Running.as_str())
        .bind(updated_at)
        .bind(media_id.to_string())
        .execute(&self.pool)
        .await?;
        bump_library_revision(&self.pool).await?;

        Ok(())
    }

    pub async fn mark_stream_copy_ready(
        &self,
        media_id: Uuid,
        output_path: &str,
        output_content_type: &str,
        subtitle_path: Option<&str>,
        updated_at: &str,
    ) -> Result<(), PersistenceError> {
        sqlx::query(
            r#"
            UPDATE stream_copies
            SET status = ?1,
                output_path = ?2,
                output_content_type = ?3,
                subtitle_path = ?4,
                error = NULL,
                updated_at = ?5
            WHERE media_id = ?6
            "#,
        )
        .bind(StreamCopyStatus::Ready.as_str())
        .bind(output_path)
        .bind(output_content_type)
        .bind(subtitle_path)
        .bind(updated_at)
        .bind(media_id.to_string())
        .execute(&self.pool)
        .await?;
        bump_library_revision(&self.pool).await?;

        Ok(())
    }

    pub async fn mark_stream_copy_failed(
        &self,
        media_id: Uuid,
        error: &str,
        updated_at: &str,
    ) -> Result<(), PersistenceError> {
        sqlx::query(
            r#"
            UPDATE stream_copies
            SET status = ?1,
                output_path = NULL,
                output_content_type = NULL,
                subtitle_path = NULL,
                error = ?2,
                updated_at = ?3
            WHERE media_id = ?4
            "#,
        )
        .bind(StreamCopyStatus::Failed.as_str())
        .bind(error)
        .bind(updated_at)
        .bind(media_id.to_string())
        .execute(&self.pool)
        .await?;
        bump_library_revision(&self.pool).await?;

        Ok(())
    }

    pub async fn list_pending_stream_copies(
        &self,
    ) -> Result<Vec<PendingStreamCopy>, PersistenceError> {
        let rows = sqlx::query(
            r#"
            SELECT media_id
            FROM stream_copies
            WHERE status IN (?1, ?2)
            ORDER BY updated_at ASC
            "#,
        )
        .bind(StreamCopyStatus::Queued.as_str())
        .bind(StreamCopyStatus::Running.as_str())
        .fetch_all(&self.pool)
        .await?;

        rows.into_iter()
            .map(|row| {
                let raw_id = row.try_get::<String, _>("media_id")?;
                let media_id = Uuid::parse_str(&raw_id).map_err(|error| {
                    PersistenceError::InvalidData(format!(
                        "invalid stored media id '{raw_id}': {error}"
                    ))
                })?;

                Ok(PendingStreamCopy { media_id })
            })
            .collect()
    }
}

pub(crate) fn stream_copy_summary_for(
    media_id: Uuid,
    record: &StreamCopyRecord,
) -> StreamCopySummary {
    let subtitle = match (record.subtitle_kind, record.subtitle_index) {
        (Some(kind), Some(index)) => Some(StreamCopySubtitleSelection { kind, index }),
        _ => None,
    };
    let subtitle_url = if record.status == StreamCopyStatus::Ready && record.subtitle_path.is_some()
    {
        Some(format!("/api/media/{media_id}/stream-copy/subtitle"))
    } else {
        None
    };

    StreamCopySummary {
        status: record.status,
        audio_stream_index: record.audio_stream_index,
        subtitle_mode: record.subtitle_mode,
        subtitle,
        subtitle_url,
        error: record.error.clone(),
        progress_ratio: None,
        progress_speed: None,
        updated_at: record.updated_at.clone(),
    }
}
