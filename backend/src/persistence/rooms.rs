use time::OffsetDateTime;
use uuid::Uuid;

use crate::{clock::format_timestamp, rooms::RoomRecord};

use super::{
    Persistence, PersistenceError,
    rows::{map_row_to_room_record, playback_status_to_str},
};

impl Persistence {
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

    pub(crate) async fn delete_room(&self, room_id: Uuid) -> Result<bool, PersistenceError> {
        // FK cascade on playback_sessions.room_id drops the matching
        // checkpoint row in the same statement.
        let result = sqlx::query(
            r#"
            DELETE FROM rooms
            WHERE id = ?1
            "#,
        )
        .bind(room_id.to_string())
        .execute(&self.pool)
        .await?;

        Ok(result.rows_affected() > 0)
    }
}
