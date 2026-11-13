//! `scheduled_tasks`.

use super::codec;
use crate::db::Db;
use crate::error::{DbError, DbResult};
use crate::repos as r;
use sp_core::cron::Cron;
use sp_core::ids::{ServerId, TaskId};
use sp_core::model::*;
use sp_core::CoreError;
use sqlx::{Row, SqlitePool};

pub struct Tasks<'a> {
    pub(crate) db: &'a Db,
}

const COLS: &str = "id, server_id, name, action, cron, payload, enabled, only_if_running, \
                    warn_before_json, last_run_at_ms, last_result, fail_count, created_at_ms";

pub(crate) fn map(row: &sqlx::sqlite::SqliteRow) -> DbResult<ScheduledTask> {
    let cron = r::text(row, "cron")?;
    // `next_run_at_ms` is derived, never stored: a stored value goes stale the
    // moment the panel is off, and the scheduler recomputes on startup anyway.
    let next = Cron::parse(&cron).ok().and_then(|c| {
        c.next_after(chrono::Utc::now())
            .map(|t| t.timestamp_millis())
    });
    Ok(ScheduledTask {
        id: TaskId::new(r::i64(row, "id")?),
        server_id: r::i64_opt(row, "server_id")?.map(ServerId::new),
        name: r::text(row, "name")?,
        action: codec::task_action_from_str(&r::text(row, "action")?)?,
        cron,
        payload: r::text(row, "payload")?,
        enabled: r::boolean(row, "enabled")?,
        only_if_running: r::boolean(row, "only_if_running")?,
        warn_before_secs: r::json_or_default(row, "warn_before_json"),
        last_run_at_ms: r::i64_opt(row, "last_run_at_ms")?,
        last_result: r::text_opt(row, "last_result")?,
        fail_count: r::u32(row, "fail_count")?,
    })
}

impl<'a> Tasks<'a> {
    pub(crate) fn new(db: &'a Db) -> Self {
        Self { db }
    }
    fn pool(&self) -> &SqlitePool {
        self.db.pool()
    }

    pub async fn list(&self) -> DbResult<Vec<ScheduledTask>> {
        let rows = sqlx::query(&format!("SELECT {COLS} FROM scheduled_tasks ORDER BY id"))
            .fetch_all(self.pool())
            .await?;
        rows.iter().map(map).collect()
    }

    pub async fn list_for_server(&self, server_id: ServerId) -> DbResult<Vec<ScheduledTask>> {
        let rows = sqlx::query(&format!(
            "SELECT {COLS} FROM scheduled_tasks WHERE server_id = ? OR server_id IS NULL ORDER BY id"
        ))
        .bind(server_id.get())
        .fetch_all(self.pool())
        .await?;
        rows.iter().map(map).collect()
    }

    pub async fn get(&self, id: TaskId) -> DbResult<ScheduledTask> {
        let row = sqlx::query(&format!("SELECT {COLS} FROM scheduled_tasks WHERE id = ?"))
            .bind(id.get())
            .fetch_optional(self.pool())
            .await?
            .ok_or_else(|| DbError::Core(CoreError::not_found(format!("task {id}"))))?;
        map(&row)
    }

    /// `cron` is validated before insert: a bad expression must not be stored.
    pub async fn create(
        &self,
        name: &str,
        server_id: Option<ServerId>,
        action: TaskAction,
        cron: &str,
        payload: &str,
        only_if_running: bool,
        warn_before_secs: &[u32],
    ) -> DbResult<ScheduledTask> {
        Cron::parse(cron).map_err(DbError::Core)?;
        let warns = r::to_json(warn_before_secs)?;
        let id = sqlx::query(
            "INSERT INTO scheduled_tasks (server_id, name, action, cron, payload, enabled, only_if_running,
                                          warn_before_json, created_at_ms)
             VALUES (?, ?, ?, ?, ?, 1, ?, ?, ?)",
        )
        .bind(server_id.map(|s| s.get()))
        .bind(name)
        .bind(codec::task_action_to_str(action))
        .bind(cron)
        .bind(payload)
        .bind(i64::from(only_if_running))
        .bind(warns)
        .bind(now_ms())
        .execute(self.pool())
        .await?
        .last_insert_rowid();
        self.get(TaskId::new(id)).await
    }

    #[allow(clippy::too_many_arguments)]
    pub async fn update(
        &self,
        id: TaskId,
        name: &str,
        action: TaskAction,
        cron: &str,
        payload: &str,
        only_if_running: bool,
        warn_before_secs: &[u32],
    ) -> DbResult<ScheduledTask> {
        Cron::parse(cron).map_err(DbError::Core)?;
        let _ = sqlx::query(
            "UPDATE scheduled_tasks SET name = ?, action = ?, cron = ?, payload = ?, only_if_running = ?,
                    warn_before_json = ?, fail_count = 0 WHERE id = ?",
        )
        .bind(name)
        .bind(codec::task_action_to_str(action))
        .bind(cron)
        .bind(payload)
        .bind(i64::from(only_if_running))
        .bind(r::to_json(warn_before_secs)?)
        .bind(id.get())
        .execute(self.pool())
        .await?;
        self.get(id).await
    }

    pub async fn set_enabled(&self, id: TaskId, enabled: bool) -> DbResult<()> {
        let _ = sqlx::query("UPDATE scheduled_tasks SET enabled = ? WHERE id = ?")
            .bind(i64::from(enabled))
            .bind(id.get())
            .execute(self.pool())
            .await?;
        Ok(())
    }

    pub async fn record_run(&self, id: TaskId, ok: bool, result: Option<&str>) -> DbResult<()> {
        if ok {
            let _ = sqlx::query("UPDATE scheduled_tasks SET last_run_at_ms = ?, last_result = ?, fail_count = 0 WHERE id = ?")
                .bind(now_ms())
                .bind(result)
                .bind(id.get())
                .execute(self.pool())
                .await?;
        } else {
            let _ = sqlx::query("UPDATE scheduled_tasks SET last_run_at_ms = ?, last_result = ?, fail_count = fail_count + 1 WHERE id = ?")
                .bind(now_ms())
                .bind(result)
                .bind(id.get())
                .execute(self.pool())
                .await?;
        }
        Ok(())
    }

    /// Disable a task that has failed `threshold` times in a row, so a broken
    /// cron job cannot spam the console forever. Returns whether it was disabled.
    pub async fn auto_disable(&self, id: TaskId, threshold: u32) -> DbResult<bool> {
        Ok(
            sqlx::query("UPDATE scheduled_tasks SET enabled = 0 WHERE id = ? AND fail_count >= ?")
                .bind(id.get())
                .bind(i64::from(threshold))
                .execute(self.pool())
                .await?
                .rows_affected()
                > 0,
        )
    }

    pub async fn delete(&self, id: TaskId) -> DbResult<()> {
        let n = sqlx::query("DELETE FROM scheduled_tasks WHERE id = ?")
            .bind(id.get())
            .execute(self.pool())
            .await?
            .rows_affected();
        if n == 0 {
            return Err(DbError::Core(CoreError::not_found(format!("task {id}"))));
        }
        Ok(())
    }

    pub async fn delete_for_server(&self, server_id: ServerId) -> DbResult<u64> {
        Ok(
            sqlx::query("DELETE FROM scheduled_tasks WHERE server_id = ?")
                .bind(server_id.get())
                .execute(self.pool())
                .await?
                .rows_affected(),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::{Db, DbPath};

    async fn db() -> (tempfile::TempDir, Db) {
        let dir = tempfile::tempdir().unwrap();
        (
            dir,
            Db::connect(&DbPath::file(dir.path().join("p.db")))
                .await
                .unwrap(),
        )
    }

    #[tokio::test]
    async fn create_update_run_delete() {
        let (_t, db) = db().await;
        let t = db
            .tasks()
            .create(
                "nightly restart",
                None,
                TaskAction::Restart,
                "0 4 * * *",
                "",
                false,
                &[300, 60],
            )
            .await
            .unwrap();
        assert_eq!(t.action, TaskAction::Restart);
        assert!(t.enabled);
        assert_eq!(t.warn_before_secs, vec![300, 60]);
        assert!(t.next_run_at_ms.is_some(), "next run is derived from cron");

        let u = db
            .tasks()
            .update(
                t.id,
                "renamed",
                TaskAction::Broadcast,
                "*/10 * * * *",
                "hello",
                true,
                &[60],
            )
            .await
            .unwrap();
        assert_eq!(u.name, "renamed");
        assert_eq!(u.action, TaskAction::Broadcast);
        assert_eq!(u.payload, "hello");
        assert!(u.only_if_running);

        db.tasks().record_run(t.id, true, Some("ok")).await.unwrap();
        let after = db.tasks().get(t.id).await.unwrap();
        assert_eq!(after.fail_count, 0);
        assert_eq!(after.last_result.as_deref(), Some("ok"));

        db.tasks().delete(t.id).await.unwrap();
        assert!(db.tasks().get(t.id).await.is_err());
    }

    #[tokio::test]
    async fn invalid_cron_is_rejected_at_the_door() {
        let (_t, db) = db().await;
        let err = db
            .tasks()
            .create(
                "bad",
                None,
                TaskAction::Command,
                "not a cron",
                "",
                false,
                &[],
            )
            .await
            .unwrap_err();
        let core: CoreError = err.into();
        assert_eq!(core.code(), "validation", "{core}");
        assert!(
            core.to_string().contains("minutes") || core.to_string().contains("fields"),
            "{core}"
        );
        // and it must not have been inserted
        assert!(db.tasks().list().await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn auto_disable_after_repeated_failures() {
        let (_t, db) = db().await;
        let t = db
            .tasks()
            .create(
                "flaky",
                None,
                TaskAction::Backup,
                "0 * * * *",
                "",
                false,
                &[],
            )
            .await
            .unwrap();
        for _ in 0..2 {
            db.tasks()
                .record_run(t.id, false, Some("boom"))
                .await
                .unwrap();
        }
        assert_eq!(db.tasks().get(t.id).await.unwrap().fail_count, 2);
        assert!(
            !db.tasks().auto_disable(t.id, 3).await.unwrap(),
            "below threshold"
        );
        db.tasks()
            .record_run(t.id, false, Some("boom"))
            .await
            .unwrap();
        assert!(db.tasks().auto_disable(t.id, 3).await.unwrap());
        assert!(
            !db.tasks().get(t.id).await.unwrap().enabled,
            "disabled so it stops hammering"
        );
        // a success resets the counter
        db.tasks().set_enabled(t.id, true).await.unwrap();
        db.tasks().record_run(t.id, true, None).await.unwrap();
        assert_eq!(db.tasks().get(t.id).await.unwrap().fail_count, 0);
    }

    #[tokio::test]
    async fn panel_wide_tasks_have_no_server() {
        let (_t, db) = db().await;
        db.tasks()
            .create(
                "prune",
                None,
                TaskAction::PruneBackups,
                "0 5 * * 0",
                "",
                false,
                &[],
            )
            .await
            .unwrap();
        let all = db.tasks().list().await.unwrap();
        assert_eq!(all.len(), 1);
        assert_eq!(all[0].server_id, None);
    }
}
