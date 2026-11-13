//! Pool setup and the repository accessors.

use crate::error::{DbError, DbResult};
use crate::migrate;
use crate::repos;
use sqlx::sqlite::{SqliteConnectOptions, SqliteJournalMode, SqliteSynchronous};
// `Row` is the trait that carries `try_get`; without it the row accessors do not resolve.
use sqlx::{Row, SqlitePool};
use std::path::{Path, PathBuf};
use std::time::Duration;

/// Where the database lives.
#[derive(Debug, Clone)]
pub enum DbPath {
    File(PathBuf),
    /// Shared-cache in-memory database, used by tests.
    Memory,
}

impl DbPath {
    pub fn file(path: impl Into<PathBuf>) -> Self {
        Self::File(path.into())
    }

    pub fn as_path(&self) -> Option<&Path> {
        match self {
            Self::File(p) => Some(p),
            Self::Memory => None,
        }
    }

    /// Default location: `<data>/panel.db`.
    pub fn default_for(data_root: &Path) -> Self {
        Self::File(data_root.join("panel.db"))
    }
}

#[derive(Debug, Clone)]
pub struct Db {
    pool: SqlitePool,
    path: Option<PathBuf>,
}

impl Db {
    /// Open (creating if needed), apply pragmas, and run migrations.
    pub async fn connect(path: &DbPath) -> DbResult<Self> {
        let opts = Self::connect_options(path)?;
        let pool = SqlitePool::connect_with(opts).await?;
        let db = Self {
            pool,
            path: path.as_path().map(Path::to_path_buf),
        };
        db.after_open().await?;
        Ok(db)
    }

    /// In-memory database with migrations applied (tests, `--demo`).
    pub async fn in_memory() -> DbResult<Self> {
        Self::connect(&DbPath::Memory).await
    }

    fn connect_options(path: &DbPath) -> DbResult<SqliteConnectOptions> {
        let base = match path {
            DbPath::File(p) => {
                if let Some(parent) = p.parent() {
                    std::fs::create_dir_all(parent).map_err(|e| {
                        DbError::Core(sp_core::CoreError::Io(std::io::Error::new(
                            e.kind(),
                            format!("{}: {e}", parent.display()),
                        )))
                    })?;
                }
                // `filename` rather than a `sqlite://` URL: Windows paths contain
                // backslashes and drive colons, which URL parsing mangles.
                SqliteConnectOptions::new()
                    .filename(p)
                    .create_if_missing(true)
            }
            DbPath::Memory => SqliteConnectOptions::new()
                .in_memory(true)
                .shared_cache(true),
        };
        Ok(base
            // WAL: readers never block the writer, which matters because the
            // console/stats writers run every second while the UI reads.
            .journal_mode(if matches!(path, DbPath::Memory) {
                SqliteJournalMode::Memory
            } else {
                SqliteJournalMode::Wal
            })
            // NORMAL is durable enough for a panel whose contents are all
            // reconstructible (servers live on disk; the DB is the index).
            .synchronous(SqliteSynchronous::Normal)
            // sqlx 0.8 has no `temp_store` builder, and it has to be a connect-time pragma:
            // a `PRAGMA temp_store` run through the pool in `after_open` would configure the
            // one connection that happened to run it and leave the rest of the pool on disk.
            .pragma("temp_store", "2")
            .foreign_keys(true)
            .busy_timeout(Duration::from_secs(10))
            // Slow queries in a panel are a bug, but log them rather than hide them.
            .log_slow_statements(log::LevelFilter::Warn, Duration::from_millis(250)))
    }

    async fn after_open(&self) -> DbResult<()> {
        let applied = migrate::run(&self.pool).await?;
        if !applied.is_empty() {
            tracing::info!(migrations = ?applied, "database migrated");
        }
        Ok(())
    }

    pub fn pool(&self) -> &SqlitePool {
        &self.pool
    }

    pub fn path(&self) -> Option<&Path> {
        self.path.as_deref()
    }

    /// Flush the WAL into the main file (called on shutdown and before backup of
    /// the panel database itself).
    pub async fn checkpoint(&self) -> DbResult<()> {
        if self.path.is_some() {
            let _ = sqlx::query("PRAGMA wal_checkpoint(TRUNCATE)")
                .execute(&self.pool)
                .await;
        }
        Ok(())
    }

    /// `PRAGMA integrity_check` — surfaced by `serverpanel doctor`.
    pub async fn integrity_check(&self) -> DbResult<String> {
        let row = sqlx::query("PRAGMA integrity_check")
            .fetch_one(&self.pool)
            .await?;
        Ok(row
            .try_get::<String, _>("integrity_check")
            .unwrap_or_else(|_| "ok".into()))
    }

    pub async fn vacuum(&self) -> DbResult<()> {
        let _ = sqlx::query("VACUUM").execute(&self.pool).await;
        Ok(())
    }

    /// Total rows across the tables the panel writes to, for the settings page.
    pub async fn row_counts(&self) -> DbResult<serde_json::Value> {
        let mut out = serde_json::Map::new();
        for table in [
            "servers",
            "users",
            "sessions",
            "backups",
            "scheduled_tasks",
            "server_content",
            "audit_log",
        ] {
            let row = sqlx::query(&format!("SELECT COUNT(*) AS n FROM {table}"))
                .fetch_one(&self.pool)
                .await?;
            let n: i64 = row.try_get("n").unwrap_or(0);
            out.insert(table.to_string(), serde_json::Value::from(n));
        }
        Ok(serde_json::Value::Object(out))
    }

    // -- repository accessors ------------------------------------------------
    pub fn servers(&self) -> repos::Servers<'_> {
        repos::Servers::new(self)
    }
    pub fn users(&self) -> repos::Users<'_> {
        repos::Users::new(self)
    }
    pub fn sessions(&self) -> repos::Sessions<'_> {
        repos::Sessions::new(self)
    }
    pub fn backups(&self) -> repos::Backups<'_> {
        repos::Backups::new(self)
    }
    pub fn tasks(&self) -> repos::Tasks<'_> {
        repos::Tasks::new(self)
    }
    pub fn content(&self) -> repos::Content<'_> {
        repos::Content::new(self)
    }
    pub fn audit(&self) -> repos::Audit<'_> {
        repos::Audit::new(self)
    }
    pub fn settings(&self) -> repos::Settings<'_> {
        repos::Settings::new(self)
    }
    pub fn java(&self) -> repos::Java<'_> {
        repos::Java::new(self)
    }
    pub fn metrics(&self) -> repos::Metrics<'_> {
        repos::Metrics::new(self)
    }
    pub fn notifications(&self) -> repos::Notifications<'_> {
        repos::Notifications::new(self)
    }
    pub fn updates(&self) -> repos::Updates<'_> {
        repos::Updates::new(self)
    }
    pub fn eula(&self) -> repos::Eula<'_> {
        repos::Eula::new(self)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn memory_db_opens_with_schema() {
        let db = Db::in_memory().await.unwrap();
        assert_eq!(db.integrity_check().await.unwrap(), "ok");
        let counts = db.row_counts().await.unwrap();
        assert_eq!(counts["servers"], 0);
        assert_eq!(counts["users"], 0);
    }

    #[tokio::test]
    async fn file_db_persists_and_reopens() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nested").join("panel.db");
        {
            let db = Db::connect(&DbPath::file(&path)).await.unwrap();
            db.settings()
                .set("boot_count", &serde_json::json!(1))
                .await
                .unwrap();
            db.checkpoint().await.unwrap();
        }
        assert!(path.exists(), "parent dir must be created automatically");
        let db = Db::connect(&DbPath::file(&path)).await.unwrap();
        let v: serde_json::Value = db.settings().get("boot_count").await.unwrap().unwrap();
        assert_eq!(v, serde_json::json!(1));
    }

    #[tokio::test]
    async fn foreign_keys_are_enforced() {
        let db = Db::in_memory().await.unwrap();
        let err = sqlx::query("INSERT INTO backups (server_id, name, kind, format, state, object_key, created_at_ms) VALUES (999,'x','manual','zip','queued','k',1)")
            .execute(db.pool())
            .await
            .unwrap_err();
        assert!(err.to_string().contains("FOREIGN KEY"), "{err}");
    }
}
