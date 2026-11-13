//! `sp-db` — embedded SQLite. Zero configuration, no server process, one file.
//!
//! Layout:
//! * [`db::Db`] owns the pool and exposes repositories (`db.servers()`, …).
//! * [`migrate`] is a tiny forward-only runner over embedded `.sql` files. We do
//!   not use `sqlx::migrate!` so the SQL stays greppable and the runner can be
//!   unit-tested with an in-memory database.
//! * `repos/*` contain SQL only. Business rules live in `sp-server`'s services.
//!
//! Conventions:
//! * timestamps are unix **milliseconds** in `INTEGER` columns
//! * enum columns are `TEXT` written from `sp_core`'s kebab-case serde names, so
//!   the DB is human-inspectable (`sqlite3 panel.db 'select * from servers'`)
//! * structured config is one JSON column per table (`config_json`), so adding a
//!   field never needs a migration
#![forbid(unsafe_code)]

pub mod db;
pub mod error;
pub mod migrate;
pub mod repos;

pub use db::{Db, DbPath};
pub use error::{DbError, DbResult};

/// Schema version this build of the panel expects. Bumped by adding a migration
/// file; recorded in `meta.schema_version` for diagnostics and downgrade checks.
pub const SCHEMA_VERSION: u32 = 1;
