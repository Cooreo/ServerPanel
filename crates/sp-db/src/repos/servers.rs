//! `servers` table.

use super::codec;
use crate::db::Db;
use crate::error::{DbError, DbResult};
use crate::repos as r;
use sp_core::ids::ServerId;
use sp_core::model::*;
use sp_core::CoreError;
use sqlx::{Row, SqlitePool};
use std::path::PathBuf;

/// Everything needed to insert a server row. `id`, timestamps and status are
/// assigned here so callers cannot desynchronise them.
#[derive(Debug, Clone)]
pub struct ServerDraft {
    pub name: String,
    pub kind: ServerKind,
    pub mc_version: String,
    pub build: String,
    pub server_dir: PathBuf,
    pub config: ServerRuntimeConfig,
    pub eula_accepted: bool,
    pub install_state: InstallState,
    pub template_of: Option<ServerId>,
    pub tags: Vec<String>,
    pub position: Option<i32>,
}

pub struct Servers<'a> {
    pub(crate) db: &'a Db,
}

const COLS: &str =
    "id, name, kind, mc_version, build, server_dir, config_json, status, install_state, \
                    install_error, eula_accepted, position, template_of, cpu_shares, tags_json, \
                    updated_at_ms, created_at_ms";

pub(crate) fn map(row: &sqlx::sqlite::SqliteRow) -> DbResult<Server> {
    let mut config: ServerRuntimeConfig = r::json(row, "config_json")?;
    // The dedicated `server_dir` column is authoritative (it is also what the file
    // sandbox is rooted at), so it always overrides the copy inside the JSON blob.
    config.server_dir = PathBuf::from(r::text(row, "server_dir")?);
    Ok(Server {
        id: ServerId::new(r::i64(row, "id")?),
        name: r::text(row, "name")?,
        kind: codec::kind_from_str(&r::text(row, "kind")?)?,
        mc_version: r::text(row, "mc_version")?,
        build: r::text(row, "build")?,
        status: codec::status_from_str(&r::text(row, "status")?)?,
        install_state: codec::install_state_from_str(&r::text(row, "install_state")?)?,
        config,
        eula_accepted: r::boolean(row, "eula_accepted")?,
        created_at_ms: r::i64(row, "created_at_ms")?,
        updated_at_ms: r::i64(row, "updated_at_ms")?,
        position: r::i64(row, "position")? as i32,
        template_of: r::i64_opt(row, "template_of")?.map(ServerId::new),
        cpu_shares_percent: r::u32_opt(row, "cpu_shares")?,
        tags: r::json_or_default(row, "tags_json"),
        last_error: r::text_opt(row, "install_error")?,
        // Filled by the service layer (a stat syscall, not a column).
        has_icon: false,
    })
}

impl<'a> Servers<'a> {
    pub(crate) fn new(db: &'a Db) -> Self {
        Self { db }
    }
    fn pool(&self) -> &SqlitePool {
        self.db.pool()
    }

    pub async fn list(&self) -> DbResult<Vec<Server>> {
        let rows = sqlx::query(&format!(
            "SELECT {COLS} FROM servers ORDER BY position ASC, name COLLATE NOCASE ASC"
        ))
        .fetch_all(self.pool())
        .await?;
        rows.iter().map(map).collect()
    }

    pub async fn list_ids(&self) -> DbResult<Vec<ServerId>> {
        let rows = sqlx::query("SELECT id FROM servers ORDER BY position ASC")
            .fetch_all(self.pool())
            .await?;
        Ok(rows
            .iter()
            .map(|r| ServerId::new(r.try_get("id").unwrap_or(0)))
            .collect())
    }

    pub async fn get(&self, id: ServerId) -> DbResult<Server> {
        self.get_opt(id)
            .await?
            .ok_or_else(|| DbError::Core(CoreError::not_found(format!("server {id}"))))
    }

    pub async fn get_opt(&self, id: ServerId) -> DbResult<Option<Server>> {
        let row = sqlx::query(&format!("SELECT {COLS} FROM servers WHERE id = ?"))
            .bind(id.get())
            .fetch_optional(self.pool())
            .await?;
        row.as_ref().map(map).transpose()
    }

    pub async fn get_by_name(&self, name: &str) -> DbResult<Option<Server>> {
        let row = sqlx::query(&format!(
            "SELECT {COLS} FROM servers WHERE name COLLATE NOCASE = ?"
        ))
        .bind(name)
        .fetch_optional(self.pool())
        .await?;
        row.as_ref().map(map).transpose()
    }

    pub async fn create(&self, draft: ServerDraft) -> DbResult<Server> {
        let now = now_ms();
        let config_json = r::to_json(&draft.config)?;
        let tags_json = r::to_json(&draft.tags)?;
        let position: i64 = match draft.position {
            Some(p) => i64::from(p),
            None => sqlx::query("SELECT COALESCE(MAX(position) + 1, 0) FROM servers")
                .fetch_one(self.pool())
                .await?
                .try_get::<i64, _>(0)?,
        };
        let id = sqlx::query(
            "INSERT INTO servers (name, kind, mc_version, build, server_dir, config_json, status,
                                  install_state, eula_accepted, position, template_of, tags_json,
                                  created_at_ms, updated_at_ms)
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(&draft.name)
        .bind(codec::kind_to_str(draft.kind))
        .bind(&draft.mc_version)
        .bind(&draft.build)
        .bind(draft.server_dir.to_string_lossy().to_string())
        .bind(&config_json)
        .bind(codec::status_to_str(ServerStatus::NotInstalled))
        .bind(codec::install_state_to_str(draft.install_state))
        .bind(i64::from(draft.eula_accepted))
        .bind(position)
        .bind(draft.template_of.map(|i| i.get()))
        .bind(&tags_json)
        .bind(now)
        .bind(now)
        .execute(self.pool())
        .await?
        .last_insert_rowid();

        let mut server = self.get(ServerId::new(id)).await?;
        server.position = position as i32;
        Ok(server)
    }

    /// Replace the runtime config and the derived columns the UI sorts on.
    pub async fn update_config(
        &self,
        id: ServerId,
        config: &ServerRuntimeConfig,
    ) -> DbResult<Server> {
        let json = r::to_json(config)?;
        let rows =
            sqlx::query("UPDATE servers SET config_json = ?, updated_at_ms = ? WHERE id = ?")
                .bind(&json)
                .bind(now_ms())
                .bind(id.get())
                .execute(self.pool())
                .await?
                .rows_affected();
        if rows == 0 {
            return Err(DbError::Core(CoreError::not_found(format!("server {id}"))));
        }
        self.get(id).await
    }

    pub async fn set_status(&self, id: ServerId, status: ServerStatus) -> DbResult<()> {
        let _ = sqlx::query("UPDATE servers SET status = ?, updated_at_ms = ? WHERE id = ?")
            .bind(codec::status_to_str(status))
            .bind(now_ms())
            .bind(id.get())
            .execute(self.pool())
            .await?;
        Ok(())
    }

    pub async fn set_install_state(
        &self,
        id: ServerId,
        state: InstallState,
        error: Option<&str>,
        build: Option<&str>,
        mc_version: Option<&str>,
    ) -> DbResult<()> {
        let _ = sqlx::query(
            "UPDATE servers SET install_state = ?, install_error = ?,
                    build = COALESCE(?, build),
                    mc_version = COALESCE(?, mc_version),
                    status = CASE
                        WHEN ? = 'ready' THEN 'stopped'
                        WHEN ? = 'failed' THEN 'crashed'
                        ELSE status
                    END,
                    updated_at_ms = ?
             WHERE id = ?",
        )
        .bind(codec::install_state_to_str(state))
        .bind(error.map(str::to_string))
        .bind(build)
        .bind(mc_version)
        .bind(codec::install_state_to_str(state))
        .bind(codec::install_state_to_str(state))
        .bind(now_ms())
        .bind(id.get())
        .execute(self.pool())
        .await?;
        Ok(())
    }

    pub async fn set_eula_accepted(&self, id: ServerId, accepted: bool) -> DbResult<()> {
        let _ = sqlx::query("UPDATE servers SET eula_accepted = ?, updated_at_ms = ? WHERE id = ?")
            .bind(i64::from(accepted))
            .bind(now_ms())
            .bind(id.get())
            .execute(self.pool())
            .await?;
        Ok(())
    }

    pub async fn set_name(&self, id: ServerId, name: &str) -> DbResult<Server> {
        let _ = sqlx::query("UPDATE servers SET name = ?, updated_at_ms = ? WHERE id = ?")
            .bind(name)
            .bind(now_ms())
            .bind(id.get())
            .execute(self.pool())
            .await?;
        self.get(id).await
    }

    pub async fn set_tags(&self, id: ServerId, tags: &[String]) -> DbResult<Server> {
        let json = r::to_json(tags)?;
        let _ = sqlx::query("UPDATE servers SET tags_json = ?, updated_at_ms = ? WHERE id = ?")
            .bind(json)
            .bind(now_ms())
            .bind(id.get())
            .execute(self.pool())
            .await?;
        self.get(id).await
    }

    pub async fn set_cpu_shares(&self, id: ServerId, shares: Option<u32>) -> DbResult<Server> {
        let _ = sqlx::query("UPDATE servers SET cpu_shares = ?, updated_at_ms = ? WHERE id = ?")
            .bind(shares)
            .bind(now_ms())
            .bind(id.get())
            .execute(self.pool())
            .await?;
        self.get(id).await
    }

    /// Apply a new order: `ids[0]` gets position 0, and so on.
    pub async fn reorder(&self, ids: &[ServerId]) -> DbResult<()> {
        let mut tx = self.pool().begin().await?;
        for (i, id) in ids.iter().enumerate() {
            let _ = sqlx::query("UPDATE servers SET position = ? WHERE id = ?")
                .bind(i as i64)
                .bind(id.get())
                .execute(&mut *tx)
                .await?;
        }
        tx.commit().await?;
        Ok(())
    }

    pub async fn delete(&self, id: ServerId) -> DbResult<()> {
        // Cascades wipe content, backups, policies, tasks, EULA records, samples.
        let n = sqlx::query("DELETE FROM servers WHERE id = ?")
            .bind(id.get())
            .execute(self.pool())
            .await?
            .rows_affected();
        if n == 0 {
            return Err(DbError::Core(CoreError::not_found(format!("server {id}"))));
        }
        Ok(())
    }

    pub async fn count(&self) -> DbResult<u32> {
        Ok(sqlx::query("SELECT COUNT(*) AS n FROM servers")
            .fetch_one(self.pool())
            .await?
            .try_get::<i64, _>("n")? as u32)
    }

    /// Ports already claimed by *another* server — used by the wizard to pick a
    /// free port without asking the OS.
    pub async fn used_ports(&self, except: Option<ServerId>) -> DbResult<Vec<u16>> {
        // `json_extract` keeps this a single indexed-free scan of a table that is
        // tiny by construction (a dozen servers), and means the port never has to
        // be duplicated into its own column.
        let rows =
            sqlx::query("SELECT json_extract(config_json, '$.port') AS port, id FROM servers")
                .fetch_all(self.pool())
                .await?;
        let mut out = Vec::new();
        for row in rows {
            let id: i64 = row.try_get("id")?;
            if Some(ServerId::new(id)) == except {
                continue;
            }
            if let Ok(port) = row.try_get::<i64, _>("port") {
                if (1..=i64::from(u16::MAX)).contains(&port) {
                    out.push(port as u16);
                }
            }
        }
        Ok(out)
    }

    pub async fn all_dirs(&self) -> DbResult<Vec<(ServerId, PathBuf)>> {
        let rows = sqlx::query("SELECT id, server_dir FROM servers")
            .fetch_all(self.pool())
            .await?;
        let mut out = Vec::with_capacity(rows.len());
        for row in &rows {
            let id: i64 = row.try_get("id")?;
            let dir: String = row.try_get("server_dir")?;
            out.push((ServerId::new(id), PathBuf::from(dir)));
        }
        Ok(out)
    }

    /// Servers that were running when the panel last shut down (auto-start).
    pub async fn should_autostart(&self) -> DbResult<Vec<Server>> {
        let rows = sqlx::query(&format!(
            "SELECT {COLS} FROM servers WHERE status IN ('running','starting','saving') ORDER BY position ASC"
        ))
        .fetch_all(self.pool())
        .await?;
        rows.iter().map(map).collect()
    }

    /// Startup bookkeeping: a server left as `starting`/`running` from a previous
    /// process is, by definition, not running now.
    pub async fn mark_stale_statuses_stopped(&self) -> DbResult<u64> {
        Ok(sqlx::query(
            "UPDATE servers SET status = 'stopped', updated_at_ms = ?
             WHERE status IN ('running','starting','saving','stopping','installing')",
        )
        .bind(now_ms())
        .execute(self.pool())
        .await?
        .rows_affected())
    }

    /// The next free position for a new server.
    pub async fn next_position(&self) -> DbResult<i32> {
        Ok(
            sqlx::query("SELECT COALESCE(MAX(position) + 1, 0) AS p FROM servers")
                .fetch_one(self.pool())
                .await?
                .try_get::<i64, _>("p")? as i32,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::{Db, DbPath};

    async fn db() -> (tempfile::TempDir, Db) {
        let dir = tempfile::tempdir().unwrap();
        let db = Db::connect(&DbPath::file(dir.path().join("panel.db")))
            .await
            .unwrap();
        (dir, db)
    }

    fn draft(name: &str, dir: &std::path::Path) -> ServerDraft {
        let mut config = ServerRuntimeConfig::default();
        config.server_dir = dir.to_path_buf();
        config.port = 25565;
        ServerDraft {
            name: name.into(),
            kind: ServerKind::Paper,
            mc_version: "1.21.4".into(),
            build: "build 123".into(),
            server_dir: dir.join(name),
            config,
            eula_accepted: false,
            install_state: InstallState::Pending,
            template_of: None,
            tags: vec!["test".into()],
            position: None,
        }
    }

    #[tokio::test]
    async fn create_read_update_delete() {
        let (_t, db) = db().await;
        let root = tempfile::tempdir().unwrap();
        let s = db
            .servers()
            .create(draft("Survival", root.path()))
            .await
            .unwrap();
        assert_eq!(s.name, "Survival");
        assert_eq!(s.status, ServerStatus::NotInstalled);
        assert_eq!(s.install_state, InstallState::Pending);
        assert_eq!(s.kind, ServerKind::Paper);
        assert_eq!(s.tags, vec!["test".to_string()]);
        assert!(s.config.server_dir.ends_with("Survival"));

        let mut cfg = s.config.clone();
        cfg.memory_mb = 4096;
        cfg.port = 25570;
        let s2 = db.servers().update_config(s.id, &cfg).await.unwrap();
        assert_eq!(s2.config.memory_mb, 4096);
        assert_eq!(s2.config.port, 25570);
        // dir stays authoritative from its own column
        assert_eq!(s2.config.server_dir, root.path().join("Survival"));

        db.servers().set_name(s.id, "Survival 2").await.unwrap();
        assert_eq!(db.servers().get(s.id).await.unwrap().name, "Survival 2");
        assert!(
            db.servers()
                .get_by_name("survival 2")
                .await
                .unwrap()
                .is_some(),
            "case-insensitive lookup"
        );

        db.servers().delete(s.id).await.unwrap();
        assert!(db.servers().get_opt(s.id).await.unwrap().is_none());
        assert_eq!(db.servers().count().await.unwrap(), 0);
        assert!(db.servers().get(s.id).await.is_err());
    }

    #[tokio::test]
    async fn positions_auto_increment_and_reorder() {
        let (_t, db) = db().await;
        let root = tempfile::tempdir().unwrap();
        let a = db.servers().create(draft("a", root.path())).await.unwrap();
        let b = db.servers().create(draft("b", root.path())).await.unwrap();
        let c = db.servers().create(draft("c", root.path())).await.unwrap();
        assert_eq!((a.position, b.position, c.position), (0, 1, 2));
        assert_eq!(db.servers().next_position().await.unwrap(), 3);
        db.servers().reorder(&[c.id, a.id, b.id]).await.unwrap();
        let list = db.servers().list().await.unwrap();
        assert_eq!(
            list.iter().map(|s| s.name.as_str()).collect::<Vec<_>>(),
            vec!["c", "a", "b"]
        );
    }

    #[tokio::test]
    async fn duplicate_names_conflict() {
        let (_t, db) = db().await;
        let root = tempfile::tempdir().unwrap();
        db.servers()
            .create(draft("dup", root.path()))
            .await
            .unwrap();
        let err = db
            .servers()
            .create(draft("DUP", root.path()))
            .await
            .unwrap_err();
        let core: CoreError = err.into();
        assert_eq!(core.code(), "conflict", "{core}");
    }

    #[tokio::test]
    async fn install_state_drives_status() {
        let (_t, db) = db().await;
        let root = tempfile::tempdir().unwrap();
        let s = db
            .servers()
            .create(draft("inst", root.path()))
            .await
            .unwrap();
        db.servers()
            .set_install_state(s.id, InstallState::Running, None, None, None)
            .await
            .unwrap();
        assert_eq!(
            db.servers().get(s.id).await.unwrap().install_state,
            InstallState::Running
        );

        db.servers()
            .set_install_state(
                s.id,
                InstallState::Ready,
                None,
                Some("build 9"),
                Some("1.21.4"),
            )
            .await
            .unwrap();
        let s = db.servers().get(s.id).await.unwrap();
        assert_eq!(s.install_state, InstallState::Ready);
        assert_eq!(s.status, ServerStatus::Stopped);
        assert_eq!(s.build, "build 9");

        db.servers()
            .set_install_state(
                s.id,
                InstallState::Failed,
                Some("404 from papermc"),
                None,
                None,
            )
            .await
            .unwrap();
        let s = db.servers().get(s.id).await.unwrap();
        assert_eq!(s.install_state, InstallState::Failed);
        assert_eq!(
            s.status,
            ServerStatus::Crashed,
            "a failed install shows as crashed-with-reason"
        );
    }

    #[tokio::test]
    async fn autostart_bookkeeping() {
        let (_t, db) = db().await;
        let root = tempfile::tempdir().unwrap();
        let a = db.servers().create(draft("a", root.path())).await.unwrap();
        let b = db.servers().create(draft("b", root.path())).await.unwrap();
        db.servers()
            .set_status(a.id, ServerStatus::Running)
            .await
            .unwrap();
        db.servers()
            .set_status(b.id, ServerStatus::Stopped)
            .await
            .unwrap();
        assert_eq!(db.servers().should_autostart().await.unwrap().len(), 1);
        assert_eq!(db.servers().mark_stale_statuses_stopped().await.unwrap(), 1);
        assert!(db.servers().should_autostart().await.unwrap().is_empty());
        assert_eq!(
            db.servers().get(a.id).await.unwrap().status,
            ServerStatus::Stopped
        );
    }

    #[tokio::test]
    async fn used_ports_excludes_the_edited_server() {
        let (_t, db) = db().await;
        let root = tempfile::tempdir().unwrap();
        let mut d = draft("p1", root.path());
        let a = {
            d.config.port = 25565;
            db.servers().create(d.clone()).await.unwrap()
        };
        let mut d2 = draft("p2", root.path());
        d2.config.port = 25566;
        let b = db.servers().create(d2).await.unwrap();
        let used = db.servers().used_ports(None).await.unwrap();
        assert_eq!(used, vec![25565, 25566]);
        assert_eq!(
            db.servers().used_ports(Some(a.id)).await.unwrap(),
            vec![25566]
        );
        assert_eq!(
            db.servers().used_ports(Some(b.id)).await.unwrap(),
            vec![25565]
        );
    }

    #[tokio::test]
    async fn json_columns_round_trip_complex_config() {
        let (_t, db) = db().await;
        let root = tempfile::tempdir().unwrap();
        let mut d = draft("rich", root.path());
        d.config.jvm_flags = sp_core::flags::zgc_flags(21);
        d.config.env.insert("SPRING_PROFILES".into(), "prod".into());
        d.config.program_args = vec!["--nogui".into()];
        d.config.restart_policy = RestartPolicy::on_crash(5, 30);
        d.config.world_override = Some("world_nether".into());
        let s = db.servers().create(d).await.unwrap();
        let got = db.servers().get(s.id).await.unwrap();
        assert_eq!(got.config.jvm_flags, sp_core::flags::zgc_flags(21));
        assert_eq!(got.config.restart_policy, RestartPolicy::on_crash(5, 30));
        assert_eq!(
            got.config.env.get("SPRING_PROFILES").map(String::as_str),
            Some("prod")
        );
        assert_eq!(got.config.program_args, vec!["--nogui".to_string()]);
        assert_eq!(got.config.world_override.as_deref(), Some("world_nether"));
        assert_eq!(got.config.memory_mb, 2048);
    }

    #[tokio::test]
    async fn all_dirs_reports_absolute_paths() {
        let (_t, db) = db().await;
        let root = tempfile::tempdir().unwrap();
        let s = db.servers().create(draft("d", root.path())).await.unwrap();
        let dirs = db.servers().all_dirs().await.unwrap();
        assert_eq!(dirs.len(), 1);
        assert_eq!(dirs[0].0, s.id);
        assert!(dirs[0].1.is_absolute());
    }

    #[tokio::test]
    async fn cpu_shares_are_optional() {
        let (_t, db) = db().await;
        let root = tempfile::tempdir().unwrap();
        let s = db
            .servers()
            .create(draft("cpu", root.path()))
            .await
            .unwrap();
        assert_eq!(s.cpu_shares_percent, None);
        let s = db.servers().set_cpu_shares(s.id, Some(75)).await.unwrap();
        assert_eq!(s.cpu_shares_percent, Some(75));
        assert!(db
            .servers()
            .set_cpu_shares(s.id, None)
            .await
            .unwrap()
            .cpu_shares_percent
            .is_none());
    }
}
