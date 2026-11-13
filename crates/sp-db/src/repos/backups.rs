//! `backups`, `backup_policies`, `backup_destinations`.

use super::codec;
use crate::db::Db;
use crate::error::{DbError, DbResult};
use crate::repos as r;
use sp_core::ids::{BackupId, ServerId};
use sp_core::model::*;
use sp_core::CoreError;
use sqlx::{Row, SqlitePool};

pub struct Backups<'a> {
    pub(crate) db: &'a Db,
}

const COLS: &str =
    "id, server_id, destination_id, name, kind, format, state, object_key, size_bytes, \
                    sha256, included_bytes, file_count, world_consistent, error, created_at_ms, \
                    completed_at_ms, restored_count, last_restored_at_ms";

pub(crate) fn map(row: &sqlx::sqlite::SqliteRow) -> DbResult<Backup> {
    Ok(Backup {
        id: BackupId::new(r::i64(row, "id")?),
        server_id: ServerId::new(r::i64(row, "server_id")?),
        destination_id: r::text(row, "destination_id")?,
        name: r::text(row, "name")?,
        kind: codec::backup_kind_from_str(&r::text(row, "kind")?)?,
        format: codec::backup_format_from_str(&r::text(row, "format")?)?,
        state: codec::backup_state_from_str(&r::text(row, "state")?)?,
        size_bytes: r::u64_opt(row, "size_bytes")?,
        sha256: r::text_opt(row, "sha256")?,
        object_key: r::text(row, "object_key")?,
        created_at_ms: r::i64(row, "created_at_ms")?,
        completed_at_ms: r::i64_opt(row, "completed_at_ms")?,
        error: r::text_opt(row, "error")?,
        world_consistent: r::boolean(row, "world_consistent")?,
        included_bytes: r::u64_opt(row, "included_bytes")?.unwrap_or(0),
        file_count: r::u32(row, "file_count")?,
        restored_count: r::u32(row, "restored_count")?,
        last_restored_at_ms: r::i64_opt(row, "last_restored_at_ms")?,
    })
}

impl<'a> Backups<'a> {
    pub(crate) fn new(db: &'a Db) -> Self {
        Self { db }
    }
    fn pool(&self) -> &SqlitePool {
        self.db.pool()
    }

    pub async fn create(
        &self,
        server_id: ServerId,
        name: &str,
        kind: BackupKind,
        format: BackupFormat,
        destination_id: &str,
        object_key: &str,
    ) -> DbResult<Backup> {
        let id = sqlx::query(
            "INSERT INTO backups (server_id, destination_id, name, kind, format, state, object_key, created_at_ms)
             VALUES (?, ?, ?, ?, ?, 'queued', ?, ?)",
        )
        .bind(server_id.get())
        .bind(destination_id)
        .bind(name)
        .bind(codec::backup_kind_to_str(kind))
        .bind(codec::backup_format_to_str(format))
        .bind(object_key)
        .bind(now_ms())
        .execute(self.pool())
        .await?
        .last_insert_rowid();
        self.get(BackupId::new(id)).await
    }

    pub async fn get(&self, id: BackupId) -> DbResult<Backup> {
        let row = sqlx::query(&format!("SELECT {COLS} FROM backups WHERE id = ?"))
            .bind(id.get())
            .fetch_optional(self.pool())
            .await?
            .ok_or_else(|| DbError::Core(CoreError::not_found(format!("backup {id}"))))?;
        map(&row)
    }

    pub async fn list_for_server(&self, server_id: ServerId, limit: u32) -> DbResult<Vec<Backup>> {
        let rows = sqlx::query(&format!(
            "SELECT {COLS} FROM backups WHERE server_id = ? ORDER BY created_at_ms DESC, id DESC LIMIT ?"
        ))
        .bind(server_id.get())
        .bind(i64::from(limit.max(1)))
        .fetch_all(self.pool())
        .await?;
        rows.iter().map(map).collect()
    }

    pub async fn list_all(&self, limit: u32) -> DbResult<Vec<Backup>> {
        let rows = sqlx::query(&format!(
            "SELECT {COLS} FROM backups ORDER BY created_at_ms DESC, id DESC LIMIT ?"
        ))
        .bind(i64::from(limit.max(1)))
        .fetch_all(self.pool())
        .await?;
        rows.iter().map(map).collect()
    }

    pub async fn set_state(
        &self,
        id: BackupId,
        state: BackupState,
        error: Option<&str>,
    ) -> DbResult<()> {
        let _ = sqlx::query("UPDATE backups SET state = ?, error = ? WHERE id = ?")
            .bind(codec::backup_state_to_str(state))
            .bind(error.map(str::to_string))
            .bind(id.get())
            .execute(self.pool())
            .await?;
        Ok(())
    }

    pub async fn complete(
        &self,
        id: BackupId,
        size_bytes: u64,
        sha256: Option<&str>,
        included_bytes: u64,
        file_count: u32,
        world_consistent: bool,
    ) -> DbResult<Backup> {
        let _ = sqlx::query(
            "UPDATE backups SET state = 'completed', size_bytes = ?, sha256 = ?, included_bytes = ?,
                    file_count = ?, world_consistent = ?, completed_at_ms = ?, error = NULL WHERE id = ?",
        )
        .bind(size_bytes as i64)
        .bind(sha256)
        .bind(included_bytes as i64)
        .bind(i64::from(file_count))
        .bind(i64::from(world_consistent))
        .bind(now_ms())
        .bind(id.get())
        .execute(self.pool())
        .await?;
        self.get(id).await
    }

    pub async fn mark_restored(&self, id: BackupId) -> DbResult<()> {
        let _ = sqlx::query(
            "UPDATE backups SET restored_count = restored_count + 1, last_restored_at_ms = ?, state = 'completed' WHERE id = ?",
        )
        .bind(now_ms())
        .bind(id.get())
        .execute(self.pool())
        .await?;
        Ok(())
    }

    pub async fn rename(&self, id: BackupId, name: &str) -> DbResult<()> {
        let _ = sqlx::query("UPDATE backups SET name = ? WHERE id = ?")
            .bind(name)
            .bind(id.get())
            .execute(self.pool())
            .await?;
        Ok(())
    }

    /// Removes the row only; deleting the archive on disk is the engine's job.
    pub async fn remove_row(&self, id: BackupId) -> DbResult<Backup> {
        let backup = self.get(id).await?;
        let _ = sqlx::query("DELETE FROM backups WHERE id = ?")
            .bind(id.get())
            .execute(self.pool())
            .await?;
        Ok(backup)
    }

    pub async fn count_for_server(&self, server_id: ServerId) -> DbResult<u32> {
        Ok(
            sqlx::query("SELECT COUNT(*) AS n FROM backups WHERE server_id = ?")
                .bind(server_id.get())
                .fetch_one(self.pool())
                .await?
                .try_get::<i64, _>("n")? as u32,
        )
    }

    pub async fn total_size_bytes(&self) -> DbResult<u64> {
        Ok(sqlx::query(
            "SELECT COALESCE(SUM(size_bytes), 0) AS s FROM backups WHERE state = 'completed'",
        )
        .fetch_one(self.pool())
        .await?
        .try_get::<i64, _>("s")?
        .max(0) as u64)
    }

    /// Rows left in a non-terminal state by a crash — the engine marks them failed
    /// at startup so the UI never shows a stuck progress bar.
    pub async fn reap_interrupted(&self) -> DbResult<u64> {
        Ok(sqlx::query("UPDATE backups SET state = 'failed', error = 'interrupted by panel restart' WHERE state IN ('queued','running','restoring')")
            .execute(self.pool())
            .await?
            .rows_affected())
    }

    /// Retention: ids that should be deleted, newest-first ranked by `keep_last`
    /// and `keep_days`. Only `completed` backups are ever returned.
    pub async fn retention_excess(
        &self,
        server_id: ServerId,
        keep_last: u32,
        keep_days: u32,
    ) -> DbResult<Vec<Backup>> {
        let cutoff = now_ms() - i64::from(keep_days.max(1)) * 86_400_000;
        let rows = sqlx::query(&format!(
            "SELECT {COLS} FROM backups WHERE server_id = ? AND state = 'completed' ORDER BY created_at_ms DESC, id DESC"
        ))
        .bind(server_id.get())
        .fetch_all(self.pool())
        .await?;
        let mut all = Vec::with_capacity(rows.len());
        for row in &rows {
            all.push(map(row)?);
        }
        let mut doomed = Vec::new();
        for (i, b) in all.iter().enumerate() {
            let too_old = b.created_at_ms < cutoff;
            let beyond_count = i >= keep_last as usize;
            // Pinning: a backup referenced by `pinned_ids` is never pruned; the
            // service passes that list in, so the SQL stays dumb.
            if too_old && beyond_count {
                doomed.push(b.clone());
            }
        }
        Ok(doomed)
    }

    // -- policies -----------------------------------------------------------
    pub async fn policy(&self, server_id: ServerId) -> DbResult<BackupPolicy> {
        let row = sqlx::query(
            "SELECT enabled, cron, format, keep_last, keep_days, exclude_json, quiesce,
                    include_worlds, include_logs, destination_id
             FROM backup_policies WHERE server_id = ?",
        )
        .bind(server_id.get())
        .fetch_optional(self.pool())
        .await?;
        let Some(row) = row else {
            return Ok(BackupPolicy::default());
        };
        Ok(BackupPolicy {
            enabled: r::boolean(&row, "enabled")?,
            cron: r::text(&row, "cron")?,
            format: codec::backup_format_from_str(&r::text(&row, "format")?)?,
            keep_last: r::u32(&row, "keep_last")?,
            keep_days: r::u32(&row, "keep_days")?,
            exclude_patterns: r::json_or_default(&row, "exclude_json"),
            quiesce: r::boolean(&row, "quiesce")?,
            include_worlds: r::boolean(&row, "include_worlds")?,
            include_logs: r::boolean(&row, "include_logs")?,
            destination_id: r::text(&row, "destination_id")?,
        })
    }

    pub async fn set_policy(&self, server_id: ServerId, p: &BackupPolicy) -> DbResult<()> {
        let excludes = r::to_json(&p.exclude_patterns)?;
        let _ = sqlx::query(
            "INSERT INTO backup_policies (server_id, enabled, cron, format, keep_last, keep_days, exclude_json,
                                         quiesce, include_worlds, include_logs, destination_id)
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
             ON CONFLICT(server_id) DO UPDATE SET enabled = excluded.enabled, cron = excluded.cron,
                 format = excluded.format, keep_last = excluded.keep_last, keep_days = excluded.keep_days,
                 exclude_json = excluded.exclude_json, quiesce = excluded.quiesce,
                 include_worlds = excluded.include_worlds, include_logs = excluded.include_logs,
                 destination_id = excluded.destination_id",
        )
        .bind(server_id.get())
        .bind(i64::from(p.enabled))
        .bind(&p.cron)
        .bind(codec::backup_format_to_str(p.format))
        .bind(i64::from(p.keep_last))
        .bind(i64::from(p.keep_days))
        .bind(excludes)
        .bind(i64::from(p.quiesce))
        .bind(i64::from(p.include_worlds))
        .bind(i64::from(p.include_logs))
        .bind(&p.destination_id)
        .execute(self.pool())
        .await?;
        Ok(())
    }

    pub async fn servers_with_enabled_policies(&self) -> DbResult<Vec<(ServerId, BackupPolicy)>> {
        let rows = sqlx::query(
            "SELECT server_id, enabled, cron, format, keep_last, keep_days, exclude_json, quiesce,
                    include_worlds, include_logs, destination_id
             FROM backup_policies WHERE enabled = 1",
        )
        .fetch_all(self.pool())
        .await?;
        let mut out = Vec::with_capacity(rows.len());
        for row in &rows {
            let sid = ServerId::new(r::i64(row, "server_id")?);
            out.push((
                sid,
                BackupPolicy {
                    enabled: true,
                    cron: r::text(row, "cron")?,
                    format: codec::backup_format_from_str(&r::text(row, "format")?)?,
                    keep_last: r::u32(row, "keep_last")?,
                    keep_days: r::u32(row, "keep_days")?,
                    exclude_patterns: r::json_or_default(row, "exclude_json"),
                    quiesce: r::boolean(row, "quiesce")?,
                    include_worlds: r::boolean(row, "include_worlds")?,
                    include_logs: r::boolean(row, "include_logs")?,
                    destination_id: r::text(row, "destination_id")?,
                },
            ));
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::{Db, DbPath};
    use crate::repos::ServerDraft;
    use sp_core::model::{InstallState, ServerKind, ServerRuntimeConfig};
    use std::path::PathBuf;

    async fn setup() -> (tempfile::TempDir, Db, ServerId) {
        let dir = tempfile::tempdir().unwrap();
        let db = Db::connect(&DbPath::file(dir.path().join("p.db")))
            .await
            .unwrap();
        let mut config = ServerRuntimeConfig::default();
        config.server_dir = PathBuf::from("/tmp/sp-backup-test");
        let s = db
            .servers()
            .create(ServerDraft {
                name: "b".into(),
                kind: ServerKind::Paper,
                mc_version: "1.21.4".into(),
                build: String::new(),
                server_dir: PathBuf::from("/tmp/sp-backup-test"),
                config,
                eula_accepted: true,
                install_state: InstallState::Ready,
                template_of: None,
                tags: vec![],
                position: None,
            })
            .await
            .unwrap();
        (dir, db, s.id)
    }

    #[tokio::test]
    async fn create_complete_and_list() {
        let (_t, db, sid) = setup().await;
        let b = db
            .backups()
            .create(
                sid,
                "pre-holiday",
                BackupKind::Manual,
                BackupFormat::TarZstd,
                "local",
                "s1/pre.tzst",
            )
            .await
            .unwrap();
        assert_eq!(b.state, BackupState::Queued);
        assert_eq!(b.format, BackupFormat::TarZstd);
        db.backups()
            .set_state(b.id, BackupState::Running, None)
            .await
            .unwrap();
        assert_eq!(
            db.backups().get(b.id).await.unwrap().state,
            BackupState::Running
        );
        let done = db
            .backups()
            .complete(b.id, 4096, Some("abc"), 9000, 42, true)
            .await
            .unwrap();
        assert_eq!(done.state, BackupState::Completed);
        assert_eq!(done.size_bytes, Some(4096));
        assert_eq!(done.file_count, 42);
        assert!(done.world_consistent);
        assert_eq!(done.sha256.as_deref(), Some("abc"));
        assert_eq!(db.backups().count_for_server(sid).await.unwrap(), 1);
        assert_eq!(db.backups().total_size_bytes().await.unwrap(), 4096);
        assert_eq!(
            db.backups().list_for_server(sid, 10).await.unwrap().len(),
            1
        );
        db.backups().mark_restored(b.id).await.unwrap();
        assert_eq!(db.backups().get(b.id).await.unwrap().restored_count, 1);
        db.backups().rename(b.id, "renamed").await.unwrap();
        assert_eq!(db.backups().get(b.id).await.unwrap().name, "renamed");
    }

    #[tokio::test]
    async fn failures_and_reaping() {
        let (_t, db, sid) = setup().await;
        let b = db
            .backups()
            .create(
                sid,
                "x",
                BackupKind::Scheduled,
                BackupFormat::Zip,
                "local",
                "k",
            )
            .await
            .unwrap();
        db.backups()
            .set_state(b.id, BackupState::Failed, Some("disk full"))
            .await
            .unwrap();
        let got = db.backups().get(b.id).await.unwrap();
        assert_eq!(got.error.as_deref(), Some("disk full"));
        assert_eq!(
            db.backups().total_size_bytes().await.unwrap(),
            0,
            "failed backups do not count"
        );
        assert_eq!(db.backups().reap_interrupted().await.unwrap(), 0);

        let stuck = db
            .backups()
            .create(
                sid,
                "y",
                BackupKind::Manual,
                BackupFormat::Zip,
                "local",
                "k2",
            )
            .await
            .unwrap();
        db.backups()
            .set_state(stuck.id, BackupState::Running, None)
            .await
            .unwrap();
        assert_eq!(db.backups().reap_interrupted().await.unwrap(), 1);
        assert_eq!(
            db.backups().get(stuck.id).await.unwrap().error.as_deref(),
            Some("interrupted by panel restart")
        );
    }

    #[tokio::test]
    async fn retention_needs_both_limits() {
        let (_t, db, sid) = setup().await;
        for i in 0..5 {
            let b = db
                .backups()
                .create(
                    sid,
                    &format!("b{i}"),
                    BackupKind::Scheduled,
                    BackupFormat::Zip,
                    "local",
                    "k",
                )
                .await
                .unwrap();
            db.backups()
                .complete(b.id, 10, None, 10, 1, true)
                .await
                .unwrap();
            let _ = sqlx::query("UPDATE backups SET created_at_ms = ? WHERE id = ?")
                .bind(now_ms() - i64::from(10 - i) * 86_400_000)
                .bind(b.id.get())
                .execute(db.pool())
                .await
                .unwrap();
        }
        // keep_last=3, keep_days=100 → nothing is both beyond count and too old.
        assert!(db
            .backups()
            .retention_excess(sid, 3, 100)
            .await
            .unwrap()
            .is_empty());
        // keep_last=3, keep_days=2 → the 3 oldest qualify.
        let doomed = db.backups().retention_excess(sid, 3, 2).await.unwrap();
        assert_eq!(doomed.len(), 3);
        assert!(doomed.iter().all(|b| b.name != "b4"));
        let row = db.backups().remove_row(doomed[0].id).await.unwrap();
        assert_eq!(db.backups().count_for_server(sid).await.unwrap(), 4);
        assert!(db.backups().get(row.id).await.is_err());
    }

    #[tokio::test]
    async fn policy_upsert_and_listing() {
        let (_t, db, sid) = setup().await;
        let default = db.backups().policy(sid).await.unwrap();
        assert!(!default.enabled);
        assert_eq!(default.cron, "0 4 * * *");
        let mut p = default.clone();
        p.enabled = true;
        p.cron = "*/30 * * * *".into();
        p.format = BackupFormat::Zip;
        p.keep_last = 2;
        p.exclude_patterns = vec!["logs".into()];
        db.backups().set_policy(sid, &p).await.unwrap();
        let got = db.backups().policy(sid).await.unwrap();
        assert_eq!(got, p);
        let list = db.backups().servers_with_enabled_policies().await.unwrap();
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].0, sid);
        p.enabled = false;
        db.backups().set_policy(sid, &p).await.unwrap();
        assert!(db
            .backups()
            .servers_with_enabled_policies()
            .await
            .unwrap()
            .is_empty());
        // policies cascade with the server
        db.servers().delete(sid).await.unwrap();
        assert_eq!(db.backups().policy(sid).await.unwrap(), default);
    }
}
