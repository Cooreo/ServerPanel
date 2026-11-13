//! `server_content` + `modpacks`: what is installed where, and whether it is enabled.

use super::codec;
use crate::db::Db;
use crate::error::{DbError, DbResult};
use crate::repos as r;
use sp_core::ids::ServerId;
use sp_core::model::*;
use sp_core::CoreError;
use sqlx::{Row, SqlitePool};

pub struct Content<'a> {
    pub(crate) db: &'a Db,
}

const COLS: &str = "id, server_id, kind, project_id, project_slug, title, version_id, version_number, \
                    filename, path, enabled, source, sha1, sha512, size_bytes, mc_versions_json, \
                    loaders_json, install_json, installed_at_ms, updated_at_ms";

pub(crate) fn map(row: &sqlx::sqlite::SqliteRow) -> DbResult<InstalledContent> {
    Ok(InstalledContent {
        id: r::i64(row, "id")?,
        server_id: ServerId::new(r::i64(row, "server_id")?),
        kind: codec::content_kind_from_str(&r::text(row, "kind")?)?,
        project_id: r::text_opt(row, "project_id")?,
        project_slug: r::text_opt(row, "project_slug")?,
        title: r::text(row, "title")?,
        version_id: r::text_opt(row, "version_id")?,
        version_number: r::text(row, "version_number")?,
        filename: r::text(row, "filename")?,
        path: r::text(row, "path")?,
        enabled: r::boolean(row, "enabled")?,
        mc_versions: r::json_or_default(row, "mc_versions_json"),
        loaders: r::json_or_default(row, "loaders_json"),
        sha1: r::text_opt(row, "sha1")?,
        sha512: r::text_opt(row, "sha512")?,
        installed_at_ms: r::i64(row, "installed_at_ms")?,
        update_to: None,
        source: codec::content_source_from_str(&r::text(row, "source")?)?,
    })
}

/// Payload for an install record. `install_json` keeps whatever the source needs
/// (Modrinth version JSON, the original URL, the mrpack index entry) without a
/// migration per field.
#[derive(Debug, Clone)]
pub struct NewContent {
    pub server_id: ServerId,
    pub kind: ContentKind,
    pub project_id: Option<String>,
    pub project_slug: Option<String>,
    pub title: String,
    pub version_id: Option<String>,
    pub version_number: String,
    pub filename: String,
    pub path: String,
    pub sha1: Option<String>,
    pub sha512: Option<String>,
    pub size_bytes: Option<u64>,
    pub mc_versions: Vec<String>,
    pub loaders: Vec<String>,
    pub source: ContentSource,
    pub install_json: serde_json::Value,
}

impl<'a> Content<'a> {
    pub(crate) fn new(db: &'a Db) -> Self {
        Self { db }
    }
    fn pool(&self) -> &SqlitePool {
        self.db.pool()
    }

    pub async fn record(&self, c: &NewContent) -> DbResult<i64> {
        let now = now_ms();
        let upsert = sqlx::query(
            "INSERT INTO server_content (server_id, kind, project_id, project_slug, title, version_id,
                                         version_number, filename, path, enabled, source, sha1, sha512,
                                         size_bytes, mc_versions_json, loaders_json, install_json,
                                         installed_at_ms, updated_at_ms)
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, 1, ?, ?, ?, ?, ?, ?, ?, ?, ?)
             ON CONFLICT(server_id, path) DO UPDATE SET
                 kind = excluded.kind, project_id = excluded.project_id, project_slug = excluded.project_slug,
                 title = excluded.title, version_id = excluded.version_id, version_number = excluded.version_number,
                 filename = excluded.filename, enabled = 1, source = excluded.source, sha1 = excluded.sha1,
                 sha512 = excluded.sha512, size_bytes = excluded.size_bytes, mc_versions_json = excluded.mc_versions_json,
                 loaders_json = excluded.loaders_json, install_json = excluded.install_json, updated_at_ms = excluded.updated_at_ms",
        )
        .bind(c.server_id.get())
        .bind(codec::content_kind_to_str(c.kind))
        .bind(c.project_id.clone())
        .bind(c.project_slug.clone())
        .bind(&c.title)
        .bind(c.version_id.clone())
        .bind(&c.version_number)
        .bind(&c.filename)
        .bind(&c.path)
        .bind(codec::content_source_to_str(c.source.clone()))
        .bind(&c.sha1)
        .bind(&c.sha512)
        .bind(c.size_bytes.map(|v| v as i64))
        .bind(r::to_json(&c.mc_versions)?)
        .bind(r::to_json(&c.loaders)?)
        .bind(c.install_json.to_string())
        .bind(now)
        .bind(now)
        .execute(self.pool())
        .await?;
        // `last_insert_rowid` is 0 on the UPDATE branch, so look the row up.
        let row = sqlx::query("SELECT id FROM server_content WHERE server_id = ? AND path = ?")
            .bind(c.server_id.get())
            .bind(&c.path)
            .fetch_one(self.pool())
            .await?;
        let _ = upsert;
        Ok(row.try_get("id")?)
    }

    pub async fn list(&self, server_id: ServerId, kind: Option<ContentKind>) -> DbResult<Vec<InstalledContent>> {
        let rows = match kind {
            Some(k) => sqlx::query(&format!(
                "SELECT {COLS} FROM server_content WHERE server_id = ? AND kind = ? ORDER BY title COLLATE NOCASE"
            ))
            .bind(server_id.get())
            .bind(codec::content_kind_to_str(k))
            .fetch_all(self.pool())
            .await?,
            None => sqlx::query(&format!(
                "SELECT {COLS} FROM server_content WHERE server_id = ? ORDER BY kind, title COLLATE NOCASE"
            ))
            .bind(server_id.get())
            .fetch_all(self.pool())
            .await?,
        };
        rows.iter().map(map).collect()
    }

    pub async fn by_path(&self, server_id: ServerId, path: &str) -> DbResult<Option<InstalledContent>> {
        let row = sqlx::query(&format!("SELECT {COLS} FROM server_content WHERE server_id = ? AND path = ?"))
            .bind(server_id.get())
            .bind(path)
            .fetch_optional(self.pool())
            .await?;
        row.as_ref().map(map).transpose()
    }

    /// Files the panel knows about, keyed for the "orphaned jar" detector.
    pub async fn known_paths(&self, server_id: ServerId) -> DbResult<Vec<String>> {
        let rows = sqlx::query("SELECT path FROM server_content WHERE server_id = ?")
            .bind(server_id.get())
            .fetch_all(self.pool())
            .await?;
        Ok(rows
            .iter()
            .filter_map(|row| row.try_get::<String, _>("path").ok())
            .collect())
    }

    pub async fn set_enabled(&self, id: i64, enabled: bool) -> DbResult<()> {
        let _ = sqlx::query("UPDATE server_content SET enabled = ?, updated_at_ms = ? WHERE id = ?")
            .bind(i64::from(enabled))
            .bind(now_ms())
            .execute(self.pool())
            .await?;
        Ok(())
    }

    pub async fn set_version(&self, id: i64, version_id: &str, version_number: &str, filename: &str, path: &str, sha1: Option<&str>, sha512: Option<&str>, size_bytes: Option<u64>) -> DbResult<()> {
        let _ = sqlx::query(
            "UPDATE server_content SET version_id = ?, version_number = ?, filename = ?, path = ?,
                    sha1 = ?, sha512 = ?, size_bytes = ?, updated_at_ms = ? WHERE id = ?",
        )
        .bind(version_id)
        .bind(version_number)
        .bind(filename)
        .bind(path)
        .bind(sha1)
        .bind(sha512)
        .bind(size_bytes.map(|v| v as i64))
        .bind(now_ms())
        .bind(id)
        .execute(self.pool())
        .await?;
        Ok(())
    }

    pub async fn forget(&self, id: i64) -> DbResult<InstalledContent> {
        let row = sqlx::query(&format!("SELECT {COLS} FROM server_content WHERE id = ?"))
            .bind(id)
            .fetch_optional(self.pool())
            .await?
            .ok_or_else(|| DbError::Core(CoreError::not_found(format!("content {id}"))))?;
        let item = map(&row)?;
        let _ = sqlx::query("DELETE FROM server_content WHERE id = ?")
            .bind(id)
            .execute(self.pool())
            .await?;
        Ok(item)
    }

    pub async fn forget_path(&self, server_id: ServerId, path: &str) -> DbResult<u64> {
        Ok(sqlx::query("DELETE FROM server_content WHERE server_id = ? AND path = ?")
            .bind(server_id.get())
            .bind(path)
            .execute(self.pool())
            .await?
            .rows_affected())
    }

    /// Every Modrinth-backed row with a version id — the input for a bulk
    /// `version_files` update check.
    pub async fn modrinth_rows(&self) -> DbResult<Vec<InstalledContent>> {
        let rows = sqlx::query(&format!(
            "SELECT {COLS} FROM server_content WHERE source = 'modrinth' AND version_id IS NOT NULL"
        ))
        .fetch_all(self.pool())
        .await?;
        rows.iter().map(map).collect()
    }

    pub async fn record_modpack(&self, server_id: ServerId, slug: &str, name: &str, version: &str, mc: &str, loader: &str, loader_version: &str, file_count: u32, skipped: &[String]) -> DbResult<i64> {
        let _ = sqlx::query("DELETE FROM modpacks WHERE server_id = ? AND slug = ?")
            .bind(server_id.get())
            .bind(slug)
            .execute(self.pool())
            .await?;
        Ok(sqlx::query(
            "INSERT INTO modpacks (server_id, slug, name, version_number, mc_version, loader, loader_version, file_count, skipped_json, installed_at_ms)
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(server_id.get())
        .bind(slug)
        .bind(name)
        .bind(version)
        .bind(mc)
        .bind(loader)
        .bind(loader_version)
        .bind(i64::from(file_count))
        .bind(r::to_json(skipped)?)
        .bind(now_ms())
        .execute(self.pool())
        .await?
        .last_insert_rowid())
    }

    pub async fn modpacks(&self, server_id: ServerId) -> DbResult<Vec<serde_json::Value>> {
        let rows = sqlx::query("SELECT slug, name, version_number, mc_version, loader, loader_version, file_count, skipped_json, installed_at_ms FROM modpacks WHERE server_id = ? ORDER BY installed_at_ms DESC")
            .bind(server_id.get())
            .fetch_all(self.pool())
            .await?;
        Ok(rows
            .iter()
            .map(|row| {
                serde_json::json!({
                    "slug": row.try_get::<String, _>("slug").unwrap_or_default(),
                    "name": row.try_get::<String, _>("name").unwrap_or_default(),
                    "versionNumber": row.try_get::<String, _>("version_number").unwrap_or_default(),
                    "mcVersion": row.try_get::<String, _>("mc_version").unwrap_or_default(),
                    "loader": row.try_get::<String, _>("loader").unwrap_or_default(),
                    "loaderVersion": row.try_get::<String, _>("loader_version").unwrap_or_default(),
                    "fileCount": row.try_get::<i64, _>("file_count").unwrap_or(0),
                    "skipped": row.try_get::<String, _>("skipped_json").ok().and_then(|s| serde_json::from_str::<Vec<String>>(&s).ok()).unwrap_or_default(),
                    "installedAtMs": row.try_get::<i64, _>("installed_at_ms").unwrap_or(0),
                })
            })
            .collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::{Db, DbPath};
    use sp_core::model::{InstallState, ServerKind, ServerRuntimeConfig};
    use crate::repos::ServerDraft;
    use std::path::PathBuf;

    async fn setup() -> (tempfile::TempDir, Db, ServerId) {
        let dir = tempfile::tempdir().unwrap();
        let db = Db::connect(&DbPath::file(dir.path().join("p.db"))).await.unwrap();
        let mut config = ServerRuntimeConfig::default();
        config.server_dir = PathBuf::from("/tmp/sp-content-test");
        let s = db.servers().create(ServerDraft {
            name: "c".into(),
            kind: ServerKind::Fabric,
            mc_version: "1.21.4".into(),
            build: "0.16.9".into(),
            server_dir: PathBuf::from("/tmp/sp-content-test"),
            config,
            eula_accepted: true,
            install_state: InstallState::Ready,
            template_of: None,
            tags: vec![],
            position: None,
        }).await.unwrap();
        (dir, db, s.id)
    }

    fn new_content(server_id: ServerId, path: &str) -> NewContent {
        NewContent {
            server_id,
            kind: ContentKind::Mod,
            project_id: Some("mnmm".into()),
            project_slug: Some("sodium".into()),
            title: "Sodium".into(),
            version_id: Some("v1".into()),
            version_number: "0.6.0".into(),
            filename: path.rsplit('/').next().unwrap().to_string(),
            path: path.into(),
            sha1: Some("a".repeat(40)),
            sha512: None,
            size_bytes: Some(1000),
            mc_versions: vec!["1.21.4".into()],
            loaders: vec!["fabric".into()],
            source: ContentSource::Modrinth,
            install_json: serde_json::json!({ "files": [] }),
        }
    }

    #[tokio::test]
    async fn records_lists_toggles_and_forgets() {
        let (_t, db, sid) = setup().await;
        let id = db.content().record(&new_content(sid, "mods/sodium.jar")).await.unwrap();
        let list = db.content().list(sid, Some(ContentKind::Mod)).await.unwrap();
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].title, "Sodium");
        assert!(list[0].enabled);
        assert_eq!(list[0].sha1.as_deref(), Some(&"a".repeat(40)[..]));
        assert_eq!(list[0].mc_versions, vec!["1.21.4".to_string()]);

        db.content().set_enabled(id, false).await.unwrap();
        assert!(!db.content().by_path(sid, "mods/sodium.jar").await.unwrap().unwrap().enabled);

        db.content().set_version(id, "v2", "0.6.1", "sodium-0.6.1.jar", "mods/sodium-0.6.1.jar", None, None, Some(2000)).await.unwrap();
        let updated = db.content().forget(id).await.unwrap();
        assert_eq!(updated.version_number, "0.6.1");
        assert_eq!(updated.path, "mods/sodium-0.6.1.jar");
        assert!(db.content().list(sid, None).await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn reinstall_same_path_updates_in_place() {
        let (_t, db, sid) = setup().await;
        db.content().record(&new_content(sid, "mods/x.jar")).await.unwrap();
        let mut again = new_content(sid, "mods/x.jar");
        again.version_number = "9.9.9".into();
        let id2 = db.content().record(&again).await.unwrap();
        let list = db.content().list(sid, None).await.unwrap();
        assert_eq!(list.len(), 1, "upsert must not duplicate");
        assert_eq!(list[0].version_number, "9.9.9");
        assert_eq!(list[0].id, id2);
        assert_eq!(db.content().known_paths(sid).await.unwrap(), vec!["mods/x.jar".to_string()]);
    }

    #[tokio::test]
    async fn modrinth_rows_filter_and_modpacks() {
        let (_t, db, sid) = setup().await;
        db.content().record(&new_content(sid, "mods/m.jar")).await.unwrap();
        let mut manual = new_content(sid, "mods/manual.jar");
        manual.source = ContentSource::Upload;
        db.content().record(&manual).await.unwrap();
        assert_eq!(db.content().modrinth_rows().await.unwrap().len(), 1);

        let mp = db.content().record_modpack(sid, "fabric-example", "Example Pack", "1.0.0", "1.21.4", "fabric", "0.16.9", 12, &["client-only.jar".into()]).await.unwrap();
        assert!(mp > 0);
        let packs = db.content().modpacks(sid).await.unwrap();
        assert_eq!(packs.len(), 1);
        assert_eq!(packs[0]["slug"], serde_json::json!("fabric-example"));
        assert_eq!(packs[0]["fileCount"], serde_json::json!(12));
        assert_eq!(packs[0]["skipped"].as_array().map(Vec::len), Some(1));
        // re-install of the same pack replaces rather than appends
        db.content().record_modpack(sid, "fabric-example", "Example Pack", "1.0.1", "1.21.4", "fabric", "0.16.9", 13, &[]).await.unwrap();
        assert_eq!(db.content().modpacks(sid).await.unwrap().len(), 1);
        assert_eq!(db.content().modpacks(sid).await.unwrap()[0]["versionNumber"], serde_json::json!("1.0.1"));
    }

    #[tokio::test]
    async fn forget_missing_is_not_found() {
        let (_t, db, _sid) = setup().await;
        let err = db.content().forget(9999).await.unwrap_err();
        assert_eq!(<DbError as Into<CoreError>>::into(err).code(), "not_found");
        assert_eq!(db.content().forget_path(_sid, "mods/gone.jar").await.unwrap(), 0);
    }
}
