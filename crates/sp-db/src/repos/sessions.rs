//! `sessions` table: opaque bearer tokens, stored only as SHA-256.
//!
//! The cookie value is a 32-byte random token; the DB keeps `sha256(token)` so a
//! database copy cannot be replayed. Each session also carries the CSRF token used
//! by the double-submit pattern (see `sp_server::middleware::csrf`).

use crate::db::Db;
use crate::error::{DbError, DbResult};
use crate::repos as r;
use sp_core::ids::UserId;
use sp_core::model::now_ms;
use sp_core::CoreError;
use sqlx::SqlitePool;

#[derive(Debug, Clone)]
pub struct Session {
    pub token_hash: String,
    pub user_id: UserId,
    pub created_at_ms: i64,
    pub expires_at_ms: i64,
    pub last_seen_ms: i64,
    pub ip: Option<String>,
    pub user_agent: Option<String>,
    pub csrf_token: String,
}

pub struct Sessions<'a> {
    pub(crate) db: &'a Db,
}

fn map(row: &sqlx::sqlite::SqliteRow) -> DbResult<Session> {
    Ok(Session {
        token_hash: r::text(row, "token_hash")?,
        user_id: UserId::new(r::i64(row, "user_id")?),
        created_at_ms: r::i64(row, "created_at_ms")?,
        expires_at_ms: r::i64(row, "expires_at_ms")?,
        last_seen_ms: r::i64(row, "last_seen_ms")?,
        ip: r::text_opt(row, "ip")?,
        user_agent: r::text_opt(row, "user_agent")?,
        csrf_token: r::text(row, "csrf_token")?,
    })
}

impl<'a> Sessions<'a> {
    pub(crate) fn new(db: &'a Db) -> Self {
        Self { db }
    }
    fn pool(&self) -> &SqlitePool {
        self.db.pool()
    }

    #[allow(clippy::too_many_arguments)]
    pub async fn create(
        &self,
        user_id: UserId,
        token_hash: &str,
        csrf_token: &str,
        ttl_ms: i64,
        ip: Option<&str>,
        user_agent: Option<&str>,
    ) -> DbResult<Session> {
        let now = now_ms();
        let _ = sqlx::query(
            "INSERT INTO sessions (token_hash, user_id, created_at_ms, expires_at_ms, last_seen_ms, ip, user_agent, csrf_token)
             VALUES (?, ?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(token_hash)
        .bind(user_id.get())
        .bind(now)
        .bind(now + ttl_ms)
        .bind(now)
        .bind(ip)
        .bind(user_agent)
        .bind(csrf_token)
        .execute(self.pool())
        .await?;
        self.get_by_token(token_hash)
            .await?
            .ok_or_else(|| DbError::Core(CoreError::other("session insert failed")))
    }

    pub async fn get_by_token(&self, token_hash: &str) -> DbResult<Option<Session>> {
        let row = sqlx::query(
            "SELECT token_hash, user_id, created_at_ms, expires_at_ms, last_seen_ms, ip, user_agent, csrf_token
             FROM sessions WHERE token_hash = ? AND revoked_at_ms IS NULL AND expires_at_ms > ?",
        )
        .bind(token_hash)
        .bind(now_ms())
        .fetch_optional(self.pool())
        .await?;
        row.as_ref().map(map).transpose()
    }

    /// Sliding expiry, throttled by the caller (see `auth::SessionStore`).
    pub async fn touch(&self, token_hash: &str, new_expiry_ms: i64) -> DbResult<()> {
        let _ = sqlx::query(
            "UPDATE sessions SET last_seen_ms = ?, expires_at_ms = ? WHERE token_hash = ?",
        )
        .bind(now_ms())
        .bind(new_expiry_ms)
        .bind(token_hash)
        .execute(self.pool())
        .await?;
        Ok(())
    }

    pub async fn revoke(&self, token_hash: &str) -> DbResult<()> {
        let _ = sqlx::query(
            "UPDATE sessions SET revoked_at_ms = ? WHERE token_hash = ? AND revoked_at_ms IS NULL",
        )
        .bind(now_ms())
        .bind(token_hash)
        .execute(self.pool())
        .await?;
        Ok(())
    }

    /// Used by "log out everywhere" and by password changes / role revocation.
    pub async fn revoke_all_for_user(
        &self,
        user_id: UserId,
        except_token: Option<&str>,
    ) -> DbResult<u64> {
        let sql = match except_token {
            Some(_) => "UPDATE sessions SET revoked_at_ms = ? WHERE user_id = ? AND revoked_at_ms IS NULL AND token_hash <> ?",
            None => "UPDATE sessions SET revoked_at_ms = ? WHERE user_id = ? AND revoked_at_ms IS NULL",
        };
        let mut q = sqlx::query(sql).bind(now_ms()).bind(user_id.get());
        if let Some(token) = except_token {
            q = q.bind(token);
        }
        Ok(q.execute(self.pool()).await?.rows_affected())
    }

    pub async fn list_for_user(&self, user_id: UserId) -> DbResult<Vec<Session>> {
        let rows = sqlx::query(
            "SELECT token_hash, user_id, created_at_ms, expires_at_ms, last_seen_ms, ip, user_agent, csrf_token
             FROM sessions WHERE user_id = ? AND revoked_at_ms IS NULL AND expires_at_ms > ?
             ORDER BY last_seen_ms DESC",
        )
        .bind(user_id.get())
        .bind(now_ms())
        .fetch_all(self.pool())
        .await?;
        rows.iter().map(map).collect()
    }

    /// Delete anything expired or revoked; returns rows removed.
    pub async fn purge_expired(&self) -> DbResult<u64> {
        Ok(
            sqlx::query(
                "DELETE FROM sessions WHERE expires_at_ms < ? OR revoked_at_ms IS NOT NULL",
            )
            .bind(now_ms())
            .execute(self.pool())
            .await?
            .rows_affected(),
        )
    }
}

#[cfg(test)]
mod tests {
    use crate::db::{Db, DbPath};
    use sp_core::model::Role;

    async fn db() -> (tempfile::TempDir, Db) {
        let dir = tempfile::tempdir().unwrap();
        // Build the pool *before* moving `dir` into the tuple: tuple elements are evaluated
        // in order, so `dir.path()` ran on a TempDir that had already been moved out.
        let db = Db::connect(&DbPath::file(dir.path().join("p.db")))
            .await
            .unwrap();
        (dir, db)
    }

    #[tokio::test]
    async fn lifecycle() {
        let (_t, db) = db().await;
        let u = db.users().create("a", "h", Role::Admin).await.unwrap();
        let s = db
            .sessions()
            .create(
                u.id,
                "hash1",
                "csrf1",
                3_600_000,
                Some("127.0.0.1"),
                Some("Mozilla"),
            )
            .await
            .unwrap();
        assert_eq!(s.user_id, u.id);
        assert_eq!(s.csrf_token, "csrf1");
        assert_eq!(s.ip.as_deref(), Some("127.0.0.1"));

        let got = db.sessions().get_by_token("hash1").await.unwrap().unwrap();
        assert_eq!(got.token_hash, "hash1");
        assert!(db.sessions().get_by_token("nope").await.unwrap().is_none());

        db.sessions().revoke("hash1").await.unwrap();
        assert!(
            db.sessions().get_by_token("hash1").await.unwrap().is_none(),
            "revoked sessions must stop working"
        );
    }

    #[tokio::test]
    async fn expiry_is_enforced_in_sql() {
        let (_t, db) = db().await;
        let u = db.users().create("b", "h", Role::Viewer).await.unwrap();
        db.sessions()
            .create(u.id, "old", "c", -1, None, None)
            .await
            .unwrap();
        assert!(db.sessions().get_by_token("old").await.unwrap().is_none());
        assert!(db.sessions().purge_expired().await.unwrap() >= 1);
        assert!(db.sessions().list_for_user(u.id).await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn revoke_everywhere_keeps_current_session() {
        let (_t, db) = db().await;
        let u = db.users().create("c", "h", Role::Admin).await.unwrap();
        for i in 0..3 {
            db.sessions()
                .create(u.id, &format!("h{i}"), "c", 3_600_000, None, None)
                .await
                .unwrap();
        }
        assert_eq!(db.sessions().list_for_user(u.id).await.unwrap().len(), 3);
        let killed = db
            .sessions()
            .revoke_all_for_user(u.id, Some("h2"))
            .await
            .unwrap();
        assert_eq!(killed, 2);
        assert!(db.sessions().get_by_token("h0").await.unwrap().is_none());
        assert!(
            db.sessions().get_by_token("h2").await.unwrap().is_some(),
            "the caller's session survives"
        );
        assert_eq!(
            db.sessions().revoke_all_for_user(u.id, None).await.unwrap(),
            1
        );
    }

    #[tokio::test]
    async fn cascades_on_user_delete() {
        let (_t, db) = db().await;
        let admin = db.users().create("x", "h", Role::Admin).await.unwrap();
        let victim = db.users().create("y", "h", Role::Viewer).await.unwrap();
        db.sessions()
            .create(victim.id, "hy", "c", 3_600_000, None, None)
            .await
            .unwrap();
        db.users().delete(victim.id).await.unwrap();
        assert!(db.sessions().get_by_token("hy").await.unwrap().is_none());
        assert!(db.users().get(admin.id).await.unwrap().is_some());
    }
}
