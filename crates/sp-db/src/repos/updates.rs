//! `update_state` (singleton row) — remembered self-update answers.

use crate::db::Db;
use crate::error::DbResult;
use sp_core::model::ReleaseInfo;
use sqlx::Row;

pub struct Updates<'a> {
    pub(crate) db: &'a Db,
}

#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UpdateState {
    pub current_version: String,
    pub available_version: Option<String>,
    pub checked_at_ms: Option<i64>,
    pub changelog_md: Option<String>,
    pub download_url: Option<String>,
    pub asset_name: Option<String>,
    pub sha256: Option<String>,
    pub staged_path: Option<String>,
    pub previous_version_dir: Option<String>,
    /// unknown | up-to-date | available | downloading | staged | applied |
    /// rolled-back | error
    pub state: String,
    pub last_error: Option<String>,
}

impl<'a> Updates<'a> {
    pub(crate) fn new(db: &'a Db) -> Self {
        Self { db }
    }

    pub async fn get(&self, current_version: &str) -> DbResult<UpdateState> {
        let row = sqlx::query(
            "SELECT current_version, available_version, checked_at_ms, changelog_md, download_url,
                    asset_name, sha256, staged_path, previous_version_dir, state, last_error
             FROM update_state WHERE id = 1",
        )
        .fetch_optional(self.db.pool())
        .await?;
        let Some(row) = row else {
            return Ok(UpdateState {
                current_version: current_version.to_string(),
                state: "unknown".into(),
                ..Default::default()
            })
        };
        let read = |c: &str| row.try_get::<Option<String>, _>(c).ok().flatten();
        let num = |c: &str| row.try_get::<Option<i64>, _>(c).ok().flatten();
        Ok(UpdateState {
            current_version: row.try_get("current_version").unwrap_or_else(|_| current_version.to_string()),
            available_version: read("available_version"),
            checked_at_ms: num("checked_at_ms"),
            changelog_md: read("changelog_md"),
            download_url: read("download_url"),
            asset_name: read("asset_name"),
            sha256: read("sha256"),
            staged_path: read("staged_path"),
            previous_version_dir: read("previous_version_dir"),
            state: read("state").unwrap_or_else(|| "unknown".into()),
            last_error: read("last_error"),
        })
    }

    pub async fn save(&self, s: &UpdateState) -> DbResult<()> {
        let _ = sqlx::query(
            "INSERT INTO update_state (id, current_version, available_version, checked_at_ms, changelog_md,
                                       download_url, asset_name, sha256, staged_path, previous_version_dir,
                                       state, last_error)
             VALUES (1, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
             ON CONFLICT(id) DO UPDATE SET current_version = excluded.current_version,
                 available_version = excluded.available_version, checked_at_ms = excluded.checked_at_ms,
                 changelog_md = excluded.changelog_md, download_url = excluded.download_url,
                 asset_name = excluded.asset_name, sha256 = excluded.sha256, staged_path = excluded.staged_path,
                 previous_version_dir = excluded.previous_version_dir, state = excluded.state,
                 last_error = excluded.last_error",
        )
        .bind(&s.current_version)
        .bind(&s.available_version)
        .bind(s.checked_at_ms)
        .bind(&s.changelog_md)
        .bind(&s.download_url)
        .bind(&s.asset_name)
        .bind(&s.sha256)
        .bind(&s.staged_path)
        .bind(&s.previous_version_dir)
        .bind(&s.state)
        .bind(&s.last_error)
        .execute(self.db.pool())
        .await?;
        Ok(())
    }

    /// Cache a "check" result for `ttl` so opening Settings never hits GitHub twice.
    pub async fn cached_release(&self, ttl_ms: i64) -> DbResult<Option<ReleaseInfo>> {
        let s = self.get("").await?;
        if s.state != "available" && s.state != "up-to-date" {
            return Ok(None);
        }
        let fresh = s.checked_at_ms.map(|t| crate::repos::now_ms() - t < ttl_ms).unwrap_or(false);
        if !fresh {
            return Ok(None);
        }
        let Some(version) = s.available_version else { return Ok(None) };
        Ok(Some(ReleaseInfo {
            version: version.clone(),
            channel: sp_core::model::ReleaseChannel::Stable,
            tag: format!("v{version}"),
            published_at_ms: s.checked_at_ms.unwrap_or_default(),
            changelog_md: s.changelog_md.unwrap_or_default(),
            url: s.download_url.unwrap_or_default(),
            assets: Vec::new(),
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::{Db, DbPath};

    #[tokio::test]
    async fn singleton_row_upserts() {
        let dir = tempfile::tempdir().unwrap();
        let db = Db::connect(&DbPath::file(dir.path().join("p.db"))).await.unwrap();
        let fresh = db.updates().get("0.1.0").await.unwrap();
        assert_eq!(fresh.state, "unknown");
        assert_eq!(fresh.current_version, "0.1.0");

        db.updates()
            .save(&UpdateState {
                current_version: "0.1.0".into(),
                available_version: Some("0.2.0".into()),
                checked_at_ms: Some(crate::repos::now_ms()),
                state: "available".into(),
                changelog_md: Some("## 0.2.0\n- fix".into()),
                download_url: Some("https://example/x.exe".into()),
                asset_name: Some("x.exe".into()),
                sha256: Some("ab".into()),
                ..Default::default()
            })
            .await
            .unwrap();
        let got = db.updates().get("0.1.0").await.unwrap();
        assert_eq!(got.state, "available");
        assert_eq!(got.available_version.as_deref(), Some("0.2.0"));
        assert_eq!(got.asset_name.as_deref(), Some("x.exe"));
        assert!(db.updates().cached_release(60_000).await.unwrap().is_some());
        assert!(db.updates().cached_release(0).await.unwrap().is_none(), "stale cache must be refetched");
    }
}
