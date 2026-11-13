//! `eula_agreements`: the legally meaningful log of "who accepted what, when".

use crate::db::Db;
use crate::error::DbResult;
use crate::repos as r;
use sp_core::ids::{ServerId, UserId};
use sp_core::model::EulaAgreement;
use sqlx::{Row, SqlitePool};

pub struct Eula<'a> {
    pub(crate) db: &'a Db,
}

impl<'a> Eula<'a> {
    pub(crate) fn new(db: &'a Db) -> Self {
        Self { db }
    }
    fn pool(&self) -> &SqlitePool {
        self.db.pool()
    }

    pub async fn record(
        &self,
        server_id: ServerId,
        user_id: Option<UserId>,
        text_sha256: &str,
        source_url: &str,
        user_agent: Option<&str>,
        ip: Option<&str>,
    ) -> DbResult<i64> {
        Ok(sqlx::query(
            "INSERT INTO eula_agreements (server_id, user_id, accepted_at_ms, text_sha256, source_url, user_agent, ip)
             VALUES (?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(server_id.get())
        .bind(user_id.map(|u| u.get()))
        .bind(now_ms())
        .bind(text_sha256)
        .bind(source_url)
        .bind(user_agent)
        .bind(ip)
        .execute(self.pool())
        .await?
        .last_insert_rowid())
    }

    /// The most recent agreement for a server, if any.
    pub async fn latest_for(&self, server_id: ServerId) -> DbResult<Option<EulaAgreement>> {
        let row = sqlx::query(
            "SELECT server_id, user_id, accepted_at_ms, text_sha256, source_url, user_agent, ip
             FROM eula_agreements WHERE server_id = ? ORDER BY accepted_at_ms DESC, id DESC LIMIT 1",
        )
        .bind(server_id.get())
        .fetch_optional(self.pool())
        .await?;
        let Some(row) = row else { return Ok(None) };
        Ok(Some(EulaAgreement {
            server_id: ServerId::new(r::i64(&row, "server_id")?),
            accepted_by: r::i64_opt(&row, "user_id")?.map(UserId::new),
            accepted_at_ms: r::i64(&row, "accepted_at_ms")?,
            text_sha256: r::text(&row, "text_sha256")?,
            source_url: r::text(&row, "source_url")?,
            user_agent: r::text_opt(&row, "user_agent")?,
            ip: r::text_opt(&row, "ip")?,
        }))
    }

    /// `eula.txt` says `true` but no agreement was recorded (folder imported from
    /// elsewhere): the wizard requires an explicit acceptance before first start.
    pub async fn has_any(&self, server_id: ServerId) -> DbResult<bool> {
        Ok(
            sqlx::query("SELECT 1 AS x FROM eula_agreements WHERE server_id = ? LIMIT 1")
                .bind(server_id.get())
                .fetch_optional(self.pool())
                .await?
                .is_some(),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::{Db, DbPath};
    use crate::repos::ServerDraft;
    use sp_core::model::{InstallState, Role, ServerKind, ServerRuntimeConfig};
    use std::path::PathBuf;

    async fn server(db: &Db) -> ServerId {
        let mut config = ServerRuntimeConfig::default();
        config.server_dir = PathBuf::from("/tmp/sp-eula-test");
        db.servers()
            .create(ServerDraft {
                name: "eula-test".into(),
                kind: ServerKind::Vanilla,
                mc_version: "1.21.4".into(),
                build: String::new(),
                server_dir: PathBuf::from("/tmp/sp-eula-test"),
                config,
                eula_accepted: false,
                install_state: InstallState::Pending,
                template_of: None,
                tags: vec![],
                position: None,
            })
            .await
            .unwrap()
            .id
    }

    #[tokio::test]
    async fn records_and_returns_latest() {
        let dir = tempfile::tempdir().unwrap();
        let db = Db::connect(&DbPath::file(dir.path().join("p.db")))
            .await
            .unwrap();
        let sid = server(&db).await;
        let uid = db
            .users()
            .create("admin", "h", Role::Admin)
            .await
            .unwrap()
            .id;
        assert!(!db.eula().has_any(sid).await.unwrap());
        assert!(db.eula().latest_for(sid).await.unwrap().is_none());

        db.eula()
            .record(
                sid,
                Some(uid),
                "aaa",
                "https://aka.ms/MinecraftEULA",
                Some("ServerPanel/0.1.0"),
                Some("127.0.0.1"),
            )
            .await
            .unwrap();
        let second = db
            .eula()
            .record(
                sid,
                Some(uid),
                "bbb",
                "https://aka.ms/MinecraftEULA",
                None,
                None,
            )
            .await
            .unwrap();
        assert!(second > 0);
        let latest = db.eula().latest_for(sid).await.unwrap().unwrap();
        assert_eq!(latest.text_sha256, "bbb", "newest row wins");
        assert_eq!(latest.accepted_by, Some(uid));
        assert!(db.eula().has_any(sid).await.unwrap());
    }

    #[tokio::test]
    async fn deleting_a_user_keeps_the_agreement() {
        let dir = tempfile::tempdir().unwrap();
        let db = Db::connect(&DbPath::file(dir.path().join("p.db")))
            .await
            .unwrap();
        let sid = server(&db).await;
        let admin = db
            .users()
            .create("admin", "h", Role::Admin)
            .await
            .unwrap()
            .id;
        let victim = db
            .users()
            .create("bob", "h", Role::Viewer)
            .await
            .unwrap()
            .id;
        db.eula()
            .record(sid, Some(victim), "x", "u", None, None)
            .await
            .unwrap();
        db.users().delete(victim).await.unwrap();
        let latest = db.eula().latest_for(sid).await.unwrap().unwrap();
        assert_eq!(
            latest.accepted_by, None,
            "the audit trail survives, the FK nulls out"
        );
        assert!(admin.get() > 0);
    }
}
