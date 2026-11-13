//! Outbound notification queue (Discord webhook, SMTP, desktop).
//!
//! The queue exists so a notification failure never fails the action that caused
//! it (a backup that succeeded must not be reported as failed because Discord was
//! down), and so retries survive a restart.

use super::codec;
use crate::db::Db;
use crate::error::DbResult;
use crate::repos as r;
use sp_core::model::{now_ms, NotificationLevel};
use sqlx::{Row, SqlitePool};

pub struct Notifications<'a> {
    pub(crate) db: &'a Db,
}

#[derive(Debug, Clone)]
pub struct QueuedNotification {
    pub id: i64,
    pub channel: String,
    pub title: String,
    pub body: String,
    pub level: NotificationLevel,
    pub server_id: Option<i64>,
    pub attempts: u32,
}

impl<'a> Notifications<'a> {
    pub(crate) fn new(db: &'a Db) -> Self {
        Self { db }
    }
    fn pool(&self) -> &SqlitePool {
        self.db.pool()
    }

    pub async fn enqueue(
        &self,
        channel: &str,
        title: &str,
        body: &str,
        level: NotificationLevel,
        server_id: Option<i64>,
    ) -> DbResult<i64> {
        Ok(sqlx::query(
            "INSERT INTO notifications (created_at_ms, channel, title, body, level, server_id, attempts)
             VALUES (?, ?, ?, ?, ?, ?, 0)",
        )
        .bind(now_ms())
        .bind(channel)
        .bind(title)
        .bind(body)
        .bind(codec::notification_level_to_str(level))
        .bind(server_id)
        .execute(self.pool())
        .await?
        .last_insert_rowid())
    }

    pub async fn pending(&self, limit: u32, max_attempts: u32) -> DbResult<Vec<QueuedNotification>> {
        let rows = sqlx::query(
            "SELECT id, channel, title, body, level, server_id, attempts FROM notifications
             WHERE delivered_at_ms IS NULL AND attempts < ? ORDER BY id LIMIT ?",
        )
        .bind(i64::from(max_attempts))
        .bind(i64::from(limit.max(1)))
        .fetch_all(self.pool())
        .await?;
        let mut out = Vec::with_capacity(rows.len());
        for row in &rows {
            out.push(QueuedNotification {
                id: r::i64(row, "id")?,
                channel: r::text(row, "channel")?,
                title: r::text(row, "title")?,
                body: r::text(row, "body")?,
                level: codec::notification_level_from_str(&r::text(row, "level")?)?,
                server_id: r::i64_opt(row, "server_id")?,
                attempts: r::u32(row, "attempts")?,
            });
        }
        Ok(out)
    }

    pub async fn mark_sent(&self, id: i64) -> DbResult<()> {
        let _ = sqlx::query("UPDATE notifications SET delivered_at_ms = ? WHERE id = ?")
            .bind(now_ms())
            .bind(id)
            .execute(self.pool())
            .await?;
        Ok(())
    }

    pub async fn mark_failed(&self, id: i64, error: &str) -> DbResult<()> {
        let _ = sqlx::query("UPDATE notifications SET attempts = attempts + 1, last_error = ? WHERE id = ?")
            .bind(error)
            .bind(id)
            .execute(self.pool())
            .await?;
        Ok(())
    }

    /// Drop delivered rows older than `keep_days` (keeps failures for the log page).
    pub async fn prune(&self, keep_days: u32) -> DbResult<u64> {
        let cutoff = now_ms() - i64::from(keep_days.max(1)) * 86_400_000;
        Ok(sqlx::query("DELETE FROM notifications WHERE delivered_at_ms IS NOT NULL AND delivered_at_ms < ?")
            .bind(cutoff)
            .execute(self.pool())
            .await?
            .rows_affected())
    }

    pub async fn recent(&self, limit: u32) -> DbResult<Vec<serde_json::Value>> {
        let rows = sqlx::query("SELECT id, created_at_ms, channel, title, body, level, delivered_at_ms, attempts, last_error FROM notifications ORDER BY id DESC LIMIT ?")
            .bind(i64::from(limit.max(1)))
            .fetch_all(self.pool())
            .await?;
        Ok(rows
            .iter()
            .map(|row| {
                serde_json::json!({
                    "id": row.try_get::<i64, _>("id").unwrap_or(0),
                    "createdAtMs": row.try_get::<i64, _>("created_at_ms").unwrap_or(0),
                    "channel": row.try_get::<String, _>("channel").unwrap_or_default(),
                    "title": row.try_get::<String, _>("title").unwrap_or_default(),
                    "body": row.try_get::<String, _>("body").unwrap_or_default(),
                    "level": row.try_get::<String, _>("level").unwrap_or("info"),
                    "deliveredAtMs": row.try_get::<Option<i64>, _>("delivered_at_ms").ok().flatten(),
                    "attempts": row.try_get::<i64, _>("attempts").unwrap_or(0),
                    "lastError": row.try_get::<Option<String>, _>("last_error").ok().flatten(),
                })
            })
            .collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::{Db, DbPath};

    #[tokio::test]
    async fn queue_retries_then_delivers() {
        let dir = tempfile::tempdir().unwrap();
        let db = Db::connect(&DbPath::file(dir.path().join("p.db"))).await.unwrap();
        let id = db.notifications().enqueue("discord", "Backup done", "world-1 tzst 40 MB", NotificationLevel::Success, Some(3)).await.unwrap();
        let pending = db.notifications().pending(10, 3).await.unwrap();
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].level, NotificationLevel::Success);
        assert_eq!(pending[0].attempts, 0);

        db.notifications().mark_failed(id, "429 slow down").await.unwrap();
        let again = db.notifications().pending(10, 3).await.unwrap();
        assert_eq!(again[0].attempts, 1);
        db.notifications().mark_failed(id, "429").await.unwrap();
        db.notifications().mark_failed(id, "429").await.unwrap();
        assert!(db.notifications().pending(10, 3).await.unwrap().is_empty(), "gives up after max_attempts");
        let recent = db.notifications().recent(5).await.unwrap();
        assert_eq!(recent[0]["lastError"], serde_json::json!("429"));

        db.notifications().mark_sent(id).await.unwrap();
        assert!(db.notifications().pending(10, 3).await.unwrap().is_empty());
        assert_eq!(db.notifications().prune(0).await.unwrap(), 1);
    }
}
