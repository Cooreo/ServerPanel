//! Repositories: one struct per table group, constructed from `&Db`.
//!
//! They contain SQL and row mapping only. No validation, no filesystem access,
//! no orchestration — those are `sp-server`'s services, which keeps every write
//! path auditable in one place.

pub(crate) mod codec;

mod audit;
mod backups;
mod content;
mod eula;
mod java;
mod metrics;
mod notifications;
mod servers;
mod sessions;
mod settings;
mod tasks;
mod updates;
mod users;

pub use audit::Audit;
pub use backups::Backups;
pub use content::Content;
pub use eula::Eula;
pub use java::Java;
pub use metrics::Metrics;
pub use notifications::Notifications;
pub use servers::{ServerDraft, Servers};
pub use sessions::Sessions;
pub use settings::Settings;
pub use tasks::Tasks;
pub use updates::Updates;
pub use users::Users;

use crate::error::{DbError, DbResult};
use sqlx::sqlite::SqliteRow;
use sqlx::Row;

/// Read a `TEXT` column into `String`, mapping NULL to an error the caller can
/// avoid by using [`text_opt`].
pub(crate) fn text(row: &SqliteRow, col: &str) -> DbResult<String> {
    Ok(text_opt(row, col)?
        .ok_or_else(|| DbError::Corrupt {
            column: col.to_string(),
            value: "<null>".to_string(),
        })?)
}

pub(crate) fn text_opt(row: &SqliteRow, col: &str) -> DbResult<Option<String>> {
    Ok(row.try_get::<Option<String>, _>(col)?)
}

pub(crate) fn i64(row: &SqliteRow, col: &str) -> DbResult<i64> {
    Ok(row.try_get::<i64, _>(col)?)
}

pub(crate) fn i64_opt(row: &SqliteRow, col: &str) -> DbResult<Option<i64>> {
    Ok(row.try_get::<Option<i64>, _>(col)?)
}

pub(crate) fn u64_opt(row: &SqliteRow, col: &str) -> DbResult<Option<u64>> {
    Ok(i64_opt(row, col)?.map(|v| v.max(0) as u64))
}

pub(crate) fn u32(row: &SqliteRow, col: &str) -> DbResult<u32> {
    Ok(i64(row, col)?.clamp(0, u128::from(u32::MAX) as i64) as u32)
}

pub(crate) fn u32_opt(row: &SqliteRow, col: &str) -> DbResult<Option<u32>> {
    Ok(i64_opt(row, col)?.map(|v| v.clamp(0, i64::from(u32::MAX)) as u32))
}

pub(crate) fn u16_opt(row: &SqliteRow, col: &str) -> DbResult<Option<u16>> {
    Ok(i64_opt(row, col)?.map(|v| v.clamp(0, i64::from(u16::MAX)) as u16))
}

pub(crate) fn f64_opt(row: &SqliteRow, col: &str) -> DbResult<Option<f64>> {
    Ok(row.try_get::<Option<f64>, _>(col)?)
}

pub(crate) fn boolean(row: &SqliteRow, col: &str) -> DbResult<bool> {
    Ok(row.try_get::<bool, _>(col).unwrap_or_else(|_| {
        // A non-zero INTEGER is also acceptable, so a hand-edited DB works.
        row.try_get::<i64, _>(col).unwrap_or(0) != 0
    }))
}

/// Deserialize a JSON-in-TEXT column.
pub(crate) fn json<T: serde::de::DeserializeOwned>(row: &SqliteRow, col: &str) -> DbResult<T> {
    let raw = text(row, col)?;
    serde_json::from_str(&raw).map_err(|e| DbError::Corrupt {
        column: col.to_string(),
        value: format!("invalid JSON ({e}): {}", &raw.chars().take(120).collect::<String>()),
    })
}

/// Same, but NULL/empty/invalid falls back to `Default` (used for columns we made
/// optional after the fact).
pub(crate) fn json_or_default<T: serde::de::DeserializeOwned + Default>(row: &SqliteRow, col: &str) -> T {
    text_opt(row, col)
        .ok()
        .flatten()
        .and_then(|raw| serde_json::from_str(&raw).ok())
        .unwrap_or_default()
}

pub(crate) fn to_json<T: serde::Serialize>(v: &T) -> DbResult<String> {
    Ok(serde_json::to_string(v)?)
}

pub(crate) fn to_json_pretty<T: serde::Serialize>(v: &T) -> DbResult<String> {
    Ok(serde_json::to_string_pretty(v)?)
}

pub(crate) use sp_core::model::now_ms;

