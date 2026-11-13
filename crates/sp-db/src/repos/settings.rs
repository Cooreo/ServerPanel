//! `settings` + `meta` key/value store (JSON values).

use crate::db::Db;
use crate::error::DbResult;
use sqlx::Row;

pub struct Settings<'a> {
    pub(crate) db: &'a Db,
}

impl<'a> Settings<'a> {
    pub(crate) fn new(db: &'a Db) -> Self {
        Self { db }
    }

    pub async fn get(&self, key: &str) -> DbResult<Option<serde_json::Value>> {
        let row = sqlx::query("SELECT value FROM settings WHERE key = ?")
            .bind(key)
            .fetch_optional(self.db.pool())
            .await?;
        Ok(match row {
            Some(r) => r
                .try_get::<String, _>("value")
                .ok()
                .and_then(|v| serde_json::from_str(&v).ok()),
            None => None,
        })
    }

    /// Raw column text, bypassing JSON parsing (values written as bare strings
    /// stay bare; anything structured comes back as its JSON form).
    pub async fn get_string(&self, key: &str) -> DbResult<Option<String>> {
        Ok(sqlx::query("SELECT value FROM settings WHERE key = ?")
            .bind(key)
            .fetch_optional(self.db.pool())
            .await?
            .and_then(|row| row.try_get::<String, _>("value").ok()))
    }

    pub async fn set(&self, key: &str, value: &serde_json::Value) -> DbResult<()> {
        let text = match value {
            serde_json::Value::String(s) => s.clone(),
            other => other.to_string(),
        };
        let _ = sqlx::query(
            "INSERT INTO settings (key, value) VALUES (?, ?)
             ON CONFLICT(key) DO UPDATE SET value = excluded.value",
        )
        .bind(key)
        .bind(text)
        .execute(self.db.pool())
        .await?;
        Ok(())
    }

    pub async fn remove(&self, key: &str) -> DbResult<bool> {
        Ok(sqlx::query("DELETE FROM settings WHERE key = ?")
            .bind(key)
            .execute(self.db.pool())
            .await?
            .rows_affected()
            > 0)
    }

    pub async fn all(&self) -> DbResult<Vec<(String, String)>> {
        let rows = sqlx::query("SELECT key, value FROM settings ORDER BY key")
            .fetch_all(self.db.pool())
            .await?;
        Ok(rows
            .iter()
            .map(|r| {
                (
                    r.try_get::<String, _>("key").unwrap_or_default(),
                    r.try_get::<String, _>("value").unwrap_or_default(),
                )
            })
            .collect())
    }

    /// Bulk read for the settings page (one query instead of N).
    pub async fn get_many(
        &self,
        keys: &[&str],
    ) -> DbResult<serde_json::Map<String, serde_json::Value>> {
        let mut out = serde_json::Map::new();
        for k in keys {
            if let Some(v) = self.get(k).await? {
                out.insert((*k).to_string(), v);
            }
        }
        Ok(out)
    }

    pub async fn get_or(
        &self,
        key: &str,
        default: serde_json::Value,
    ) -> DbResult<serde_json::Value> {
        Ok(self.get(key).await?.unwrap_or(default))
    }

    /// Typed helper for the handful of booleans the UI toggles.
    pub async fn bool_or(&self, key: &str, default: bool) -> DbResult<bool> {
        Ok(match self.get(key).await? {
            Some(serde_json::Value::Bool(b)) => b,
            Some(serde_json::Value::String(s)) => s == "true",
            Some(other) => other.as_bool().unwrap_or(default),
            None => default,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::{Db, DbPath};
    use serde_json::json;

    async fn db() -> (tempfile::TempDir, Db) {
        let dir = tempfile::tempdir().unwrap();
        (
            dir,
            Db::connect(&DbPath::file(dir.path().join("p.db")))
                .await
                .unwrap(),
        )
    }

    #[tokio::test]
    async fn roundtrip_and_overwrite() {
        let (_t, db) = db().await;
        assert!(db.settings().get("theme").await.unwrap().is_none());
        db.settings()
            .set("theme", &json!("midnight"))
            .await
            .unwrap();
        assert_eq!(
            db.settings().get("theme").await.unwrap(),
            Some(json!("midnight"))
        );
        db.settings().set("theme", &json!("emerald")).await.unwrap();
        assert_eq!(
            db.settings().get_string("theme").await.unwrap().as_deref(),
            Some("emerald")
        );
        db.settings()
            .set("reduced_motion", &json!(true))
            .await
            .unwrap();
        assert!(db
            .settings()
            .bool_or("reduced_motion", false)
            .await
            .unwrap());
        assert!(!db.settings().bool_or("missing", false).await.unwrap());
        db.settings()
            .set("density", &json!("compact"))
            .await
            .unwrap();
        assert_eq!(
            db.settings()
                .get_or("density", json!("comfortable"))
                .await
                .unwrap(),
            json!("compact")
        );
        assert_eq!(
            db.settings()
                .get_or("absent", json!("fallback"))
                .await
                .unwrap(),
            json!("fallback")
        );
        assert!(db.settings().remove("density").await.unwrap());
        assert!(!db.settings().remove("density").await.unwrap());
    }

    #[tokio::test]
    async fn structured_values_survive() {
        let (_t, db) = db().await;
        let v = json!({ "accent": "#7c5cff", "charts": true, "n": 3 });
        db.settings().set("ui", &v).await.unwrap();
        assert_eq!(db.settings().get("ui").await.unwrap(), Some(v.clone()));
        let many = db.settings().get_many(&["ui", "absent"]).await.unwrap();
        assert_eq!(many.len(), 1);
        assert_eq!(many["ui"], v);
        let all = db.settings().all().await.unwrap();
        assert_eq!(all.len(), 1);
        assert_eq!(all[0].0, "ui");
    }
}
