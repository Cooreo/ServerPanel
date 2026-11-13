//! Database errors, mapped onto the API error vocabulary.

use sp_core::CoreError;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum DbError {
    #[error(transparent)]
    Core(#[from] CoreError),

    #[error("database query failed: {0}")]
    Sql(#[from] sqlx::Error),

    #[error("could not encode a column as JSON: {0}")]
    Json(#[from] serde_json::Error),

    #[error("column {column} contained unrecognised value `{value}`")]
    Corrupt { column: String, value: String },

    #[error("migration {name} failed: {reason}")]
    Migration { name: String, reason: String },

    #[error("database is at schema version {found}, but this build needs {wanted}")]
    SchemaTooNew { found: u32, wanted: u32 },
}

pub type DbResult<T> = Result<T, DbError>;

impl From<DbError> for CoreError {
    fn from(value: DbError) -> Self {
        match value {
            DbError::Core(c) => c,
            DbError::Json(j) => CoreError::Serde(j.to_string()),
            DbError::Sql(sqlx::Error::RowNotFound) => CoreError::not_found("database row"),
            DbError::Sql(err) => {
                // UNIQUE violations are the common "you typed a name that exists"
                // case, so translate them into a 409 with a human message.
                let text = err.to_string();
                if text.contains("UNIQUE constraint failed") {
                    let what = text
                        .split("UNIQUE constraint failed: ")
                        .nth(1)
                        .map(|s| s.trim().to_string())
                        .unwrap_or_else(|| "unique field".to_string());
                    CoreError::Conflict { what }
                } else if text.contains("FOREIGN KEY constraint failed") {
                    CoreError::Conflict {
                        what: "a referenced row still exists".into(),
                    }
                } else {
                    tracing::error!(error = %text, "sqlx error");
                    CoreError::Db(text)
                }
            }
            DbError::Corrupt { column, value } => CoreError::Db(format!(
                "column {column} contained unrecognised value `{value}`"
            )),
            DbError::Migration { name, reason } => CoreError::Db(format!("migration {name}: {reason}")),
            DbError::SchemaTooNew { found, wanted } => CoreError::Db(format!(
                "database schema v{found} is newer than this build (v{wanted}); update ServerPanel"
            )),
        }
    }
}

impl DbError {
    /// True when the API should answer 409 rather than 500.
    pub fn is_conflict(&self) -> bool {
        matches!(
            self,
            DbError::Sql(sqlx::Error::Database(d))
                if d.message().contains("UNIQUE constraint failed")
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn core_errors_pass_through() {
        let e: DbError = CoreError::not_found("server 3").into();
        let back: CoreError = e.into();
        assert_eq!(back.code(), "not_found");
    }

    #[test]
    fn json_errors_become_serialization() {
        let err = serde_json::from_str::<serde_json::Value>("{oops").unwrap_err();
        let db: DbError = err.into();
        let core: CoreError = db.into();
        assert_eq!(core.code(), "serialization");
    }
}
