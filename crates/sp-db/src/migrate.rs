//! Forward-only migration runner.
//!
//! Deliberately tiny: a table of applied names, `include_str!` of the SQL files,
//! and one transaction per migration (SQLite DDL is transactional, so a failed
//! migration leaves the schema untouched). There is no down-migration: users can
//! be on any older version, so every migration must be additive, and rolling back
//! is done by reinstalling the previous panel build.

use crate::error::{DbError, DbResult};
use sqlx::{Row, SqlitePool};

#[derive(Debug, Clone, Copy)]
pub struct Migration {
    pub name: &'static str,
    pub sql: &'static str,
}

/// Append new entries here and add the matching `migrations/NNNN_*.sql` file.
/// Names must sort lexicographically in apply order.
pub const MIGRATIONS: &[Migration] = &[Migration {
    name: "0001_init",
    sql: include_str!("../migrations/0001_init.sql"),
}];

/// Ensure the bookkeeping tables exist, apply anything missing, then record the
/// schema version. Returns the list of migrations applied by *this* call.
pub async fn run(pool: &SqlitePool) -> DbResult<Vec<String>> {
    let _ = sqlx::query(
        "CREATE TABLE IF NOT EXISTS sp_migrations (
            name           TEXT PRIMARY KEY,
            applied_at_ms  INTEGER NOT NULL
         )",
    )
    .execute(pool)
    .await?;
    let _ = sqlx::query(
        "CREATE TABLE IF NOT EXISTS meta (
            key   TEXT PRIMARY KEY,
            value TEXT NOT NULL
         )",
    )
    .execute(pool)
    .await?;

    let rows = sqlx::query("SELECT name FROM sp_migrations")
        .fetch_all(pool)
        .await?;
    let applied: Vec<String> = rows
        .iter()
        .map(|r| r.try_get::<String, _>("name"))
        .collect::<Result<_, _>>()?;

    let wanted = crate::SCHEMA_VERSION;
    if let Some(current) = read_meta_u32(pool, "schema_version").await? {
        if current > wanted {
            return Err(DbError::SchemaTooNew {
                found: current,
                wanted,
            });
        }
    }

    let mut done = Vec::new();
    for m in MIGRATIONS {
        if applied.iter().any(|a| a == m.name) {
            continue;
        }
        let mut tx = pool.begin().await?;
        for (idx, stmt) in split_statements(m.sql).into_iter().enumerate() {
            sqlx::query(&stmt)
                .execute(&mut *tx)
                .await
                .map_err(|e| DbError::Migration {
                    name: format!("{} (statement {})", m.name, idx + 1),
                    reason: e.to_string(),
                })?;
        }
        let _ = sqlx::query("INSERT INTO sp_migrations (name, applied_at_ms) VALUES (?, ?)")
            .bind(m.name)
            .bind(sp_core::model::now_ms())
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        tracing::info!(migration = m.name, "applied migration");
        done.push(m.name.to_string());
    }

    write_meta(pool, "schema_version", &wanted.to_string()).await?;
    write_meta(
        pool,
        "panel_version",
        &semver_from(env!("CARGO_PKG_VERSION")).to_string(),
    )
    .await?;
    Ok(done)
}

async fn read_meta_u32(pool: &SqlitePool, key: &str) -> DbResult<Option<u32>> {
    let row = sqlx::query("SELECT value FROM meta WHERE key = ?")
        .bind(key)
        .fetch_optional(pool)
        .await?;
    let Some(row) = row else { return Ok(None) };
    let v: String = row.try_get("value")?;
    Ok(v.trim().parse::<u32>().ok())
}

pub async fn write_meta(pool: &SqlitePool, key: &str, value: &str) -> DbResult<()> {
    let _ = sqlx::query(
        "INSERT INTO meta (key, value) VALUES (?, ?)
         ON CONFLICT(key) DO UPDATE SET value = excluded.value",
    )
    .bind(key)
    .bind(value)
    .execute(pool)
    .await?;
    Ok(())
}

pub async fn read_meta(pool: &SqlitePool, key: &str) -> DbResult<Option<String>> {
    let row = sqlx::query("SELECT value FROM meta WHERE key = ?")
        .bind(key)
        .fetch_optional(pool)
        .await?;
    match row {
        Some(r) => Ok(Some(r.try_get::<String, _>("value")?)),
        None => Ok(None),
    }
}

/// Trim `0.1.0` to a valid semver triple (defensive against `-next` suffixes in
/// local dev builds, which SQLite stores as text anyway).
fn semver_from(v: &str) -> String {
    let core: Vec<&str> = v
        .split('+')
        .next()
        .unwrap_or(v)
        .split('-')
        .next()
        .unwrap_or(v)
        .split('.')
        .take(3)
        .collect();
    if core.len() == 3 {
        core.join(".")
    } else {
        let mut parts = core;
        while parts.len() < 3 {
            parts.push("0");
        }
        parts.join(".")
    }
}

/// Split on `;` while respecting `--` line comments, `/* */` block comments, and
/// single-quoted string literals. Our SQL never embeds a semicolon in a literal,
/// but doing this properly means a future migration can.
pub fn split_statements(sql: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut buf = String::with_capacity(sql.len() / 4);
    let bytes: Vec<char> = sql.chars().collect();
    let mut i = 0;
    let mut in_str = false;
    let mut in_line = false;
    let mut in_block = false;
    while i < bytes.len() {
        let c = bytes[i];
        let next = bytes.get(i + 1).copied();
        if in_line {
            if c == '\n' {
                in_line = false;
                buf.push('\n');
            }
            i += 1;
            continue;
        }
        if in_block {
            if c == '*' && next == Some('/') {
                in_block = false;
                i += 2;
                continue;
            }
            if c == '\n' {
                buf.push('\n');
            }
            i += 1;
            continue;
        }
        if in_str {
            buf.push(c);
            if c == '\'' {
                // SQL escapes a quote by doubling it.
                if next == Some('\'') {
                    buf.push('\'');
                    i += 2;
                    continue;
                }
                in_str = false;
            }
            i += 1;
            continue;
        }
        match c {
            '-' if next == Some('-') => {
                in_line = true;
                i += 2;
                continue;
            }
            '/' if next == Some('*') => {
                in_block = true;
                i += 2;
                continue;
            }
            '\'' => {
                in_str = true;
                buf.push(c);
                i += 1;
                continue;
            }
            ';' => {
                let stmt = buf.trim().to_string();
                if !stmt.is_empty() {
                    out.push(stmt);
                }
                buf.clear();
                i += 1;
                continue;
            }
            _ => {
                buf.push(c);
                i += 1;
            }
        }
    }
    let tail = buf.trim().to_string();
    if !tail.is_empty() {
        out.push(tail);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splits_on_semicolons() {
        let sql = "CREATE TABLE a (x INT);\nCREATE TABLE b (y INT);";
        assert_eq!(
            split_statements(sql),
            vec![
                "CREATE TABLE a (x INT)".to_string(),
                "CREATE TABLE b (y INT)".to_string()
            ]
        );
    }

    #[test]
    fn ignores_comments_and_quotes() {
        let sql =
            "-- a; comment; never splits\nSELECT ';' AS x; /* block ; split\nhere */ SELECT 2;";
        let stmts = split_statements(sql);
        assert_eq!(stmts.len(), 2, "{stmts:?}");
        assert!(stmts[0].contains("';' AS x"), "{:?}", stmts[0]);
        assert!(!stmts[0].contains("a; comment"));
        assert!(!stmts[0].contains("block"));
        assert_eq!(stmts[1], "SELECT 2");
    }

    #[test]
    fn doubled_quotes_are_literal() {
        let sql = "INSERT INTO t VALUES ('it''s ; fine'); SELECT 1;";
        let stmts = split_statements(sql);
        assert_eq!(stmts.len(), 2);
        assert_eq!(stmts[0], "INSERT INTO t VALUES ('it''s ; fine')");
    }

    #[test]
    fn trailing_statement_without_semicolon_is_kept() {
        assert_eq!(split_statements("SELECT 1"), vec!["SELECT 1".to_string()]);
        assert!(split_statements("  \n ;  ").is_empty());
    }

    #[test]
    fn semver_normalisation() {
        assert_eq!(semver_from("0.1.0"), "0.1.0");
        assert_eq!(semver_from("1.2.3-rc1"), "1.2.3");
        assert_eq!(semver_from("1.2"), "1.2.0");
        assert_eq!(semver_from("1"), "1.0.0");
    }

    #[test]
    fn the_single_initial_migration_is_valid_sql() {
        let stmts = split_statements(MIGRATIONS[0].sql);
        assert!(
            stmts.len() > 30,
            "expected many statements, got {}",
            stmts.len()
        );
        for s in &stmts {
            assert!(
                !s.contains(';'),
                "statement must not contain a semicolon: {s}"
            );
            assert!(
                s.starts_with("CREATE") || s.starts_with("INSERT"),
                "unexpected statement shape: {s}"
            );
        }
    }

    #[tokio::test]
    async fn applies_once_and_records_version() {
        let db = crate::Db::in_memory().await.unwrap();
        let pool = db.pool();
        let again = run(pool).await.unwrap();
        assert!(
            again.is_empty(),
            "a second run must apply nothing, got {again:?}"
        );
        assert_eq!(
            read_meta_u32(pool, "schema_version").await.unwrap(),
            Some(crate::SCHEMA_VERSION)
        );
        assert_eq!(
            read_meta(pool, "panel_version").await.unwrap().as_deref(),
            Some("0.1.0")
        );
        let n: i64 = sqlx::query("SELECT COUNT(*) AS n FROM sp_migrations")
            .fetch_one(pool)
            .await
            .unwrap()
            .try_get("n")
            .unwrap();
        assert_eq!(n, 1);
    }

    #[tokio::test]
    async fn refuses_a_newer_database() {
        let db = crate::Db::in_memory().await.unwrap();
        write_meta(db.pool(), "schema_version", "999")
            .await
            .unwrap();
        let err = run(db.pool()).await.unwrap_err();
        assert!(
            matches!(err, DbError::SchemaTooNew { found: 999, .. }),
            "{err}"
        );
    }
}
