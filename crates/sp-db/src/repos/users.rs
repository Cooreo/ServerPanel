//! `users` table: credentials are Argon2id PHC strings; nothing here hashes.

use super::codec;
use crate::db::Db;
use crate::error::{DbError, DbResult};
use crate::repos as r;
use sp_core::ids::UserId;
use sp_core::model::*;
use sp_core::CoreError;
use sqlx::{Row, SqlitePool};

pub struct Users<'a> {
    pub(crate) db: &'a Db,
}

const COLS: &str = "id, username, role, totp_enabled, created_at_ms, last_login_at_ms, \
                    failed_attempts, locked_until_ms, theme_pref, density_pref, accent_pref";

fn map(row: &sqlx::sqlite::SqliteRow) -> DbResult<User> {
    Ok(User {
        id: UserId::new(r::i64(row, "id")?),
        username: r::text(row, "username")?,
        role: codec::role_from_str(&r::text(row, "role")?)?,
        totp_enabled: r::boolean(row, "totp_enabled")?,
        created_at_ms: r::i64(row, "created_at_ms")?,
        last_login_at_ms: r::i64_opt(row, "last_login_at_ms")?,
        failed_attempts: r::u32(row, "failed_attempts")?,
        locked_until_ms: r::i64_opt(row, "locked_until_ms")?,
        // Extra grants are panel-level settings, kept in `settings` so this row
        // stays a 1:1 mirror of the login form.
        extra_permissions: Vec::new(),
        server_grants: Vec::new(),
        theme_pref: r::text_opt(row, "theme_pref")?,
    })
}

impl<'a> Users<'a> {
    pub(crate) fn new(db: &'a Db) -> Self {
        Self { db }
    }
    fn pool(&self) -> &SqlitePool {
        self.db.pool()
    }

    pub async fn count(&self) -> DbResult<u32> {
        Ok(sqlx::query("SELECT COUNT(*) AS n FROM users")
            .fetch_one(self.pool())
            .await?
            .try_get::<i64, _>("n")? as u32)
    }

    pub async fn list(&self) -> DbResult<Vec<User>> {
        let rows = sqlx::query(&format!(
            "SELECT {COLS} FROM users ORDER BY username COLLATE NOCASE"
        ))
        .fetch_all(self.pool())
        .await?;
        rows.iter().map(map).collect()
    }

    pub async fn get(&self, id: UserId) -> DbResult<Option<User>> {
        let row = sqlx::query(&format!("SELECT {COLS} FROM users WHERE id = ?"))
            .bind(id.get())
            .fetch_optional(self.pool())
            .await?;
        row.as_ref().map(map).transpose()
    }

    pub async fn by_username(&self, username: &str) -> DbResult<Option<(User, String)>> {
        let row = sqlx::query(&format!(
            "SELECT {COLS}, password_hash FROM users WHERE username COLLATE NOCASE = ?"
        ))
        .bind(username)
        .fetch_optional(self.pool())
        .await?;
        let Some(row) = row else { return Ok(None) };
        let hash = r::text(&row, "password_hash")?;
        Ok(Some((map(&row)?, hash)))
    }

    pub async fn create(&self, username: &str, password_hash: &str, role: Role) -> DbResult<User> {
        let id = sqlx::query(
            "INSERT INTO users (username, password_hash, role, created_at_ms) VALUES (?, ?, ?, ?)",
        )
        .bind(username)
        .bind(password_hash)
        .bind(codec::role_to_str(role))
        .bind(now_ms())
        .execute(self.pool())
        .await?
        .last_insert_rowid();
        self.get(UserId::new(id))
            .await?
            .ok_or_else(|| DbError::Core(CoreError::other("insert did not produce a row")))
    }

    pub async fn set_password(&self, id: UserId, password_hash: &str) -> DbResult<()> {
        let _ = sqlx::query("UPDATE users SET password_hash = ?, failed_attempts = 0, locked_until_ms = NULL WHERE id = ?")
            .bind(password_hash)
            .bind(id.get())
            .execute(self.pool())
            .await?;
        Ok(())
    }

    pub async fn set_role(&self, id: UserId, role: Role) -> DbResult<()> {
        let _ = sqlx::query("UPDATE users SET role = ? WHERE id = ?")
            .bind(codec::role_to_str(role))
            .bind(id.get())
            .execute(self.pool())
            .await?;
        Ok(())
    }

    pub async fn set_totp(&self, id: UserId, secret: Option<&str>, enabled: bool) -> DbResult<()> {
        let _ = sqlx::query("UPDATE users SET totp_secret = ?, totp_enabled = ? WHERE id = ?")
            .bind(secret)
            .bind(i64::from(enabled))
            .bind(id.get())
            .execute(self.pool())
            .await?;
        Ok(())
    }

    pub async fn totp_secret(&self, id: UserId) -> DbResult<Option<String>> {
        let row = sqlx::query("SELECT totp_secret FROM users WHERE id = ?")
            .bind(id.get())
            .fetch_optional(self.pool())
            .await?;
        match row {
            Some(r) => Ok(r.try_get::<Option<String>, _>("totp_secret")?),
            None => Ok(None),
        }
    }

    pub async fn touch_login(&self, id: UserId) -> DbResult<()> {
        let _ = sqlx::query("UPDATE users SET last_login_at_ms = ?, failed_attempts = 0, locked_until_ms = NULL WHERE id = ?")
            .bind(now_ms())
            .bind(id.get())
            .execute(self.pool())
            .await?;
        Ok(())
    }

    /// Increment the failure counter and lock the account when it crosses `max`.
    pub async fn record_failure(&self, id: UserId, max: u32, lock_ms: i64) -> DbResult<u32> {
        let row = sqlx::query("SELECT failed_attempts FROM users WHERE id = ?")
            .bind(id.get())
            .fetch_optional(self.pool())
            .await?;
        let Some(row) = row else { return Ok(0) };
        let attempts: u32 = r::u32(&row, "failed_attempts")?.saturating_add(1);
        let locked: Option<i64> = (attempts >= max).then(|| now_ms() + lock_ms);
        let _ =
            sqlx::query("UPDATE users SET failed_attempts = ?, locked_until_ms = ? WHERE id = ?")
                .bind(i64::from(attempts))
                .bind(locked)
                .bind(id.get())
                .execute(self.pool())
                .await?;
        Ok(attempts)
    }

    pub async fn set_prefs(
        &self,
        id: UserId,
        theme: Option<&str>,
        density: Option<&str>,
        accent: Option<&str>,
    ) -> DbResult<()> {
        let _ = sqlx::query(
            "UPDATE users SET theme_pref = ?, density_pref = ?, accent_pref = ? WHERE id = ?",
        )
        .bind(theme)
        .bind(density)
        .bind(accent)
        .bind(id.get())
        .execute(self.pool())
        .await?;
        Ok(())
    }

    /// Refuses to remove the last admin — a panel nobody can log into is a
    /// support ticket, not a feature.
    pub async fn delete(&self, id: UserId) -> DbResult<()> {
        let admins: i64 = sqlx::query("SELECT COUNT(*) AS n FROM users WHERE role = 'admin'")
            .fetch_one(self.pool())
            .await?
            .try_get("n")?;
        let target = sqlx::query("SELECT role FROM users WHERE id = ?")
            .bind(id.get())
            .fetch_optional(self.pool())
            .await?;
        let role = match target {
            Some(row) => r::text(&row, "role")?,
            None => return Err(DbError::Core(CoreError::not_found(format!("user {id}")))),
        };
        if role == "admin" && admins <= 1 {
            return Err(DbError::Core(CoreError::other(
                "cannot delete the only admin account — promote another user first",
            )));
        }
        let _ = sqlx::query("DELETE FROM users WHERE id = ?")
            .bind(id.get())
            .execute(self.pool())
            .await?;
        Ok(())
    }

    /// First-run flag: no users at all → show the setup wizard.
    pub async fn needs_setup(&self) -> DbResult<bool> {
        Ok(self.count().await? == 0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::{Db, DbPath};

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
    async fn create_lookup_and_role_changes() {
        let (_t, db) = db().await;
        assert!(db.users().needs_setup().await.unwrap());
        let u = db
            .users()
            .create("admin", "$argon2id$v=19$x$y", Role::Admin)
            .await
            .unwrap();
        assert_eq!(u.username, "admin");
        assert_eq!(u.role, Role::Admin);
        assert!(!u.totp_enabled);
        assert!(!db.users().needs_setup().await.unwrap());

        let (found, hash) = db.users().by_username("ADMIN").await.unwrap().unwrap();
        assert_eq!(found.id, u.id);
        assert_eq!(hash, "$argon2id$v=19$x$y");
        assert!(db.users().by_username("nobody").await.unwrap().is_none());

        db.users().set_role(u.id, Role::Viewer).await.unwrap();
        assert_eq!(
            db.users().get(u.id).await.unwrap().unwrap().role,
            Role::Viewer
        );
    }

    #[tokio::test]
    async fn duplicate_usernames_are_conflicts() {
        let (_t, db) = db().await;
        db.users().create("bob", "h", Role::Viewer).await.unwrap();
        let err: CoreError = db
            .users()
            .create("BOB", "h", Role::Viewer)
            .await
            .unwrap_err()
            .into();
        assert_eq!(err.code(), "conflict");
    }

    #[tokio::test]
    async fn lockout_after_repeated_failures() {
        let (_t, db) = db().await;
        let u = db.users().create("u", "h", Role::Viewer).await.unwrap();
        assert_eq!(db.users().record_failure(u.id, 3, 60_000).await.unwrap(), 1);
        assert_eq!(db.users().record_failure(u.id, 3, 60_000).await.unwrap(), 2);
        assert_eq!(db.users().record_failure(u.id, 3, 60_000).await.unwrap(), 3);
        let got = db.users().get(u.id).await.unwrap().unwrap();
        assert_eq!(got.failed_attempts, 3);
        assert!(
            got.locked_until_ms.is_some(),
            "must be locked at the threshold"
        );
        db.users().touch_login(u.id).await.unwrap();
        let got = db.users().get(u.id).await.unwrap().unwrap();
        assert_eq!(got.failed_attempts, 0);
        assert!(got.locked_until_ms.is_none());
        assert!(got.last_login_at_ms.is_some());
    }

    #[tokio::test]
    async fn totp_toggle_and_secret() {
        let (_t, db) = db().await;
        let u = db.users().create("mfa", "h", Role::Admin).await.unwrap();
        assert!(db.users().totp_secret(u.id).await.unwrap().is_none());
        db.users()
            .set_totp(u.id, Some("JBSWY3DPEHPK3PXP"), false)
            .await
            .unwrap();
        assert_eq!(
            db.users().totp_secret(u.id).await.unwrap().as_deref(),
            Some("JBSWY3DPEHPK3PXP")
        );
        db.users()
            .set_totp(u.id, Some("JBSWY3DPEHPK3PXP"), true)
            .await
            .unwrap();
        assert!(db.users().get(u.id).await.unwrap().unwrap().totp_enabled);
        db.users().set_totp(u.id, None, false).await.unwrap();
        assert!(db.users().totp_secret(u.id).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn last_admin_cannot_be_deleted() {
        let (_t, db) = db().await;
        let admin = db.users().create("admin", "h", Role::Admin).await.unwrap();
        let err: CoreError = db.users().delete(admin.id).await.unwrap_err().into();
        assert!(err.to_string().contains("only admin"), "{err}");

        let second = db.users().create("admin2", "h", Role::Admin).await.unwrap();
        db.users().delete(second.id).await.unwrap();
        assert!(db.users().get(second.id).await.unwrap().is_none());
        // With one admin left again, deletion is blocked once more.
        assert!(db.users().delete(admin.id).await.is_err());
        // Viewers are always removable, and cascading deletes their sessions.
        let v = db.users().create("v", "h", Role::Viewer).await.unwrap();
        db.users().delete(v.id).await.unwrap();
    }

    #[tokio::test]
    async fn prefs_are_independent() {
        let (_t, db) = db().await;
        let u = db.users().create("p", "h", Role::Viewer).await.unwrap();
        db.users()
            .set_prefs(u.id, Some("amethyst"), None, Some("#7c5cff"))
            .await
            .unwrap();
        let got = db.users().get(u.id).await.unwrap().unwrap();
        assert_eq!(got.theme_pref.as_deref(), Some("amethyst"));
    }
}
