//! `java_runtimes`: what JREs exist, where they came from.

use super::codec;
use crate::db::Db;
use crate::error::{DbError, DbResult};
use crate::repos as r;
use sp_core::ids::JavaId;
use sp_core::model::{now_ms, JavaRuntime, JavaSource};
use sp_core::CoreError;
use sqlx::SqlitePool;
use std::path::{Path, PathBuf};

pub struct Java<'a> {
    pub(crate) db: &'a Db,
}

fn map(row: &sqlx::sqlite::SqliteRow) -> DbResult<JavaRuntime> {
    Ok(JavaRuntime {
        id: JavaId::new(r::i64(row, "id")?),
        major: r::u32(row, "major")? as u16,
        version: r::text(row, "version")?,
        vendor: r::text(row, "vendor")?,
        path: PathBuf::from(r::text(row, "path")?),
        source: codec::java_source_from_str(&r::text(row, "source")?)?,
        arch: r::text(row, "arch")?,
        installed_at_ms: r::i64_opt(row, "installed_at_ms")?,
        size_bytes: r::u64_opt(row, "size_bytes")?,
    })
}

const COLS: &str = "id, major, version, vendor, path, source, arch, installed_at_ms, size_bytes";

impl<'a> Java<'a> {
    pub(crate) fn new(db: &'a Db) -> Self {
        Self { db }
    }
    fn pool(&self) -> &SqlitePool {
        self.db.pool()
    }

    pub async fn all(&self) -> DbResult<Vec<JavaRuntime>> {
        let rows = sqlx::query(&format!(
            "SELECT {COLS} FROM java_runtimes ORDER BY major DESC, source"
        ))
        .fetch_all(self.pool())
        .await?;
        rows.iter().map(map).collect()
    }

    pub async fn by_major(&self, major: u16) -> DbResult<Vec<JavaRuntime>> {
        let rows = sqlx::query(&format!("SELECT {COLS} FROM java_runtimes WHERE major = ? ORDER BY source, installed_at_ms DESC"))
            .bind(i64::from(major))
            .fetch_all(self.pool())
            .await?;
        rows.iter().map(map).collect()
    }

    /// Upsert by path: the same JRE discovered twice (once by scanning, once by
    /// the user pointing at it) must stay one row.
    pub async fn upsert(
        &self,
        major: u16,
        version: &str,
        vendor: &str,
        path: &Path,
        source: JavaSource,
        arch: &str,
        sha256: Option<&str>,
        size_bytes: Option<u64>,
    ) -> DbResult<JavaRuntime> {
        let _ = sqlx::query(
            "INSERT INTO java_runtimes (major, version, vendor, path, source, arch, sha256, size_bytes, installed_at_ms)
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)
             ON CONFLICT(path) DO UPDATE SET major = excluded.major, version = excluded.version,
                 vendor = excluded.vendor, source = excluded.source, arch = excluded.arch,
                 sha256 = COALESCE(excluded.sha256, java_runtimes.sha256),
                 size_bytes = COALESCE(excluded.size_bytes, java_runtimes.size_bytes)",
        )
        .bind(i64::from(major))
        .bind(version)
        .bind(vendor)
        .bind(path.to_string_lossy().to_string())
        .bind(codec::java_source_to_str(source))
        .bind(arch)
        .bind(sha256)
        .bind(size_bytes.map(|v| v as i64))
        .bind(matches!(source, JavaSource::Managed).then(now_ms))
        .execute(self.pool())
        .await?;
        let row = sqlx::query(&format!("SELECT {COLS} FROM java_runtimes WHERE path = ?"))
            .bind(path.to_string_lossy().to_string())
            .fetch_optional(self.pool())
            .await?
            .ok_or_else(|| DbError::Core(CoreError::not_found("java runtime")))?;
        map(&row)
    }

    pub async fn remove(&self, id: JavaId) -> DbResult<JavaRuntime> {
        let row = sqlx::query(&format!("SELECT {COLS} FROM java_runtimes WHERE id = ?"))
            .bind(id.get())
            .fetch_optional(self.pool())
            .await?
            .ok_or_else(|| DbError::Core(CoreError::not_found(format!("java runtime {id}"))))?;
        let runtime = map(&row)?;
        let _ = sqlx::query("DELETE FROM java_runtimes WHERE id = ?")
            .bind(id.get())
            .execute(self.pool())
            .await?;
        Ok(runtime)
    }

    /// The newest managed runtime at or above `major`, falling back to system ones.
    pub async fn best_for(&self, major: u16) -> DbResult<Option<JavaRuntime>> {
        let rows = sqlx::query(&format!(
            "SELECT {COLS} FROM java_runtimes WHERE major = ?
             ORDER BY CASE source WHEN 'managed' THEN 0 WHEN 'manual' THEN 1 ELSE 2 END, version DESC"
        ))
        .bind(i64::from(major))
        .fetch_all(self.pool())
        .await?;
        match rows.first() {
            Some(row) => Ok(Some(map(row)?)),
            None => Ok(None),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::{Db, DbPath};

    #[tokio::test]
    async fn upsert_prefers_managed_and_finds_by_major() {
        let dir = tempfile::tempdir().unwrap();
        let db = Db::connect(&DbPath::file(dir.path().join("p.db")))
            .await
            .unwrap();
        let sys = PathBuf::from("/usr/lib/jvm/java-21/bin/java");
        let managed = PathBuf::from("/data/java/temurin-21/bin/java");
        db.java()
            .upsert(
                21,
                "21.0.5+11",
                "Debian",
                &sys,
                JavaSource::System,
                "x86_64",
                None,
                None,
            )
            .await
            .unwrap();
        db.java()
            .upsert(
                21,
                "21.0.6+7",
                "Eclipse Temurin",
                &managed,
                JavaSource::Managed,
                "x86_64",
                Some("ff"),
                Some(123),
            )
            .await
            .unwrap();
        let best = db.java().best_for(21).await.unwrap().unwrap();
        assert_eq!(
            best.source,
            JavaSource::Managed,
            "the panel-managed JRE wins"
        );
        assert_eq!(best.size_bytes, Some(123));
        assert!(best.installed_at_ms.is_some());
        assert_eq!(db.java().by_major(21).await.unwrap().len(), 2);
        assert!(db.java().best_for(8).await.unwrap().is_none());

        // re-discovery of the system java keeps one row
        db.java()
            .upsert(
                21,
                "21.0.5+12",
                "Debian",
                &sys,
                JavaSource::System,
                "x86_64",
                None,
                None,
            )
            .await
            .unwrap();
        assert_eq!(db.java().all().await.unwrap().len(), 2);
        let removed = db.java().remove(best.id).await.unwrap();
        assert_eq!(removed.path, managed);
        assert_eq!(db.java().all().await.unwrap().len(), 1);
        assert!(db.java().remove(JavaId::new(999)).await.is_err());
    }
}
