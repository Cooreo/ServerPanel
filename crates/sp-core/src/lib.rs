//! `sp-core` — the domain layer shared by every other ServerPanel crate.
//!
//! Design rules for this crate:
//! * No I/O beyond reading config files and probing local ports/system info.
//! * No axum, no sqlx, no reqwest — everything here must be reusable by the
//!   desktop shell, the CLI, and unit tests.
//! * Types here are the wire format. They are exported to TypeScript with
//!   `cargo test -p sp-core --features codegen`.
#![forbid(unsafe_code)]
#![warn(missing_debug_implementations)]

pub mod config;
pub mod cron;
pub mod error;
pub mod event;
pub mod flags;
pub mod ids;
pub mod model;
pub mod paths;
pub mod progress;
pub mod system;
pub mod verify;

pub use error::{CoreError, Result};
pub use ids::{BackupId, FileTokenId, ServerId, TaskId, UserId};
pub use model::*;

/// Re-export so downstream crates can name the version without depending on it
/// directly.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// The `User-Agent` sent to third-party APIs (Modrinth requires a descriptive
/// one; Mojang and PaperMC ask for something identifiable too).
pub fn user_agent() -> String {
    format!(
        "ServerPanel/{VERSION} (self-hosted Minecraft server manager; https://github.com/Cooreo/ServerPanel)"
    )
}
