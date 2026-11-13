//! `audit_log`: append-only trail for everything that changes state.

use super::codec;
use crate::db::Db;
use crate::error::DbResult;
use crate::repos as r;
use sp_core::ids::UserId;
use sp_core::model::*;
use sqlx::{Row, SqlitePool};

pub struct Audit<'a> {
    pub(crate) db: &'a Db,
}

#[derive(Debug, Clone, Default)]
pub struct AuditQuery {
    pub limit: u32,
    pub offset: u32,
    pub user_id: Option<UserId>,
    pub server_id: Option<i64>,
    pub action: Option<String>,
    pub only_failures: bool,
}

impl<'a> Audit<'a> {
    pub(crate) fn new(db: &'a Db) -> Self {
        Self { db }
    }
    fn pool(&self) -> &SqlitePool {
        self.db.pool()
    }

    #[allow(clippy::too_many_arguments)]
    pub async fn record(
        &self,
        user_id: Option<UserId>,
        username: Option<&str>,
        action: &str,
        target: Option<&str>,
        ip: Option<&str>,
        outcome: AuditOutcome,
        detail: Option<&str>,
    ) -> DbResult<()> {
        let _ = sqlx::query(
            "INSERT INTO audit_log (at_ms, user_id, username, action, target, ip, outcome, detail)
             VALUES (?, ?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(now_ms())
        .bind(user_id.map(|u| u.get()))
        .bind(username)
        .bind(action)
        .bind(target)
        .bind(ip)
        .bind(codec::audit_outcome_to_str(outcome))
        .bind(detail)
        .execute(self.pool())
        .await?;
        Ok(())
    }

    pub async fn list(&self, q: AuditQuery) -> DbResult<Vec<AuditEntry>> {
        let mut sql = String::from(
            "SELECT id, at_ms, user_id, username, action, target, ip, outcome, detail FROM audit_log WHERE 1=1",
        );
        if q.user_id.is_some() {
            sql.push_str(" AND user_id = ?");
        }
        if q.server_id.is_some() {
            sql.push_str(" AND target LIKE ?");
        }
        if q.action.is_some() {
            sql.push_str(" AND action = ?");
        }
        if q.only_failures {
            sql.push_str(" AND outcome = 'failure'");
        }
        sql.push_str(" ORDER BY id DESC LIMIT ? OFFSET ?");

        let mut query = sqlx::query(&sql);
        if let Some(u) = q.user_id {
            query = query.bind(u.get());
        }
        if let Some(sid) = q.server_id {
            query = query.bind(format!("server:{sid}%"));
        }
        if let Some(a) = &q.action {
            query = query.bind(a.clone());
        }
        query = query.bind(i64::from(q.limit.max(1))).bind(i64::from(q.offset));

        let rows = query.fetch_all(self.pool()).await?;
        let mut out = Vec::with_capacity(rows.len());
        for row in &rows {
            out.push(AuditEntry {
                id: r::i64(row, "id")?,
                at_ms: r::i64(row, "at_ms")?,
                user_id: r::i64_opt(row, "user_id")?.map(UserId::new),
                username: r::text_opt(row, "username")?,
                action: r::text(row, "action")?,
                target: r::text_opt(row, "target")?,
                ip: r::text_opt(row, "ip")?,
                outcome: codec::audit_outcome_from_str(&r::text(row, "outcome")?)?,
                detail: r::text_opt(row, "detail")?,
            });
        }
        Ok(out)
    }

    pub async fn count(&self) -> DbResult<u64> {
        Ok(sqlx::query("SELECT COUNT(*) AS n FROM audit_log")
            .fetch_one(self.pool())
            .await?
            .try_get::<i64, _>("n")? as u64)
    }

    /// Retention: drop everything older than `days` (called from the housekeeping
    /// task). Returns the number of rows removed.
    pub async fn prune(&self, days: u32, keep_failures: bool) -> DbResult<u64> {
        let cutoff = now_ms() - i64::from(days) * 86_400_000;
        let sql = if keep_failures {
            "DELETE FROM audit_log WHERE at_ms < ? AND outcome <> 'failure'"
        } else {
            "DELETE FROM audit_log WHERE at_ms < ?"
        };
        Ok(sqlx::query(sql).bind(cutoff).execute(self.pool()).await?.rows_affected())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::{Db, DbPath};

    async fn db() -> (tempfile::TempDir, Db) {
        let dir = tempfile::tempdir().unwrap();
        (dir, Db::connect(&DbPath::file(dir.path().join("p.db"))).await.unwrap())
    }

    #[tokio::test]
    async fn records_and_filters() {
        let (_t, db) = db().await;
        db.audit().record(None, Some("system"), "panel.start", None, None, AuditOutcome::Success, None).await.unwrap();
        db.audit().record(None, Some("admin"), "server.stop", Some("server:3"), Some("127.0.0.1"), AuditOutcome::Failure, Some("timeout")).await.unwrap();
        assert_eq!(db.audit().count().await.unwrap(), 2);

        let all = db.audit().list(AuditQuery { limit: 10, ..Default::default() }).await.unwrap();
        assert_eq!(all.len(), 2);
        assert_eq!(all[0].action, "server.stop", "newest first");
        assert_eq!(all[0].outcome, AuditOutcome::Failure);
        assert_eq!(all[0].detail.as_deref(), Some("timeout"));

        let fails = db.audit().list(AuditQuery { limit: 10, only_failures: true, ..Default::default() }).await.unwrap();
        assert_eq!(fails.len(), 1);
        let for_server = db.audit().list(AuditQuery { limit: 10, server_id: Some(3), ..Default::default() }).await.unwrap();
        assert_eq!(for_server.len(), 1);
        assert!(db.audit().list(AuditQuery { limit: 10, server_id: Some(99), ..Default::default() }).await.unwrap().is_empty());
        let page = db.audit().list(AuditQuery { limit: 1, offset: 1, ..Default::default() }).await.unwrap();
        assert_eq!(page.len(), 1);
        assert_eq!(page[0].action, "panel.start");
    }

    #[tokio::test]
    async fn prune_keeps_failures_when_asked() {
        let (_t, db) = db().await;
        db.audit().record(None, None, "old.success", None, None, AuditOutcome::Success, None).await.unwrap();
        // Force one row into the past.
        let _ = sqlx::query("UPDATE audit_log SET at_ms = ? WHERE action = 'old.success'")
            .bind(now_ms() - 400 * 86_400_000)
            .execute(db.pool())
            .await
            .unwrap();
        assert_eq!(db.audit().prune(30, false).await.unwrap(), 1);
        assert_eq!(db.audit().count().await.unwrap(), 1);

        db.audit().record(None, None, "old.fail", None, None, AuditOutcome::Failure, None).await.unwrap();
        let _ = sqlx::query("UPDATE audit_log SET at_ms = ? WHERE action = 'old.fail'")
            .bind(now_ms() - 400 * 86_400_000)
            .execute(db.pool())
            .await
            .unwrap();
        assert_eq!(db.audit().prune(30, true).await.unwrap(), 0, "failures survive");
        assert_eq!(db.audit().prune(30, false).await.unwrap(), 2);
    }
}
