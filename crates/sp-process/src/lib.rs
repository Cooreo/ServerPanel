//! `sp-process` — running Minecraft servers without orphaning them.
//!
//! Responsibilities:
//! * [`instance::Instance`] — one JVM: argv construction, piped stdin/stdout/stderr,
//!   ANSI-preserving log ring, status machine, graceful stop → SIGTERM → SIGKILL.
//! * [`manager::ProcessManager`] — the supervisor: bounded concurrent starts,
//!   crash classification, [`sp_core::model::RestartPolicy`] backoff, bulk shutdown.
//! * [`java::JavaManager`] — detection, Temurin install, per-server selection.
//! * [`job_object`] — Windows Job Objects so a killed panel cannot leak `javaw.exe`.
//!
//! `unsafe_code` is *denied* at the crate root and permitted only inside
//! `job_object.rs`'s Win32 block, where every call carries a comment. That is the
//! one documented exception in the workspace.
//!
//! Lints: `unsafe_code` is denied crate-wide. `job_object` is the single
//! documented exception and carries its own `#![allow(unsafe_code)]` with a comment
//! per Win32 call, so `grep -rn "unsafe " crates/` stays meaningful.
#![deny(unsafe_code)]

pub mod instance;
pub mod java;
pub mod job_object;
pub mod manager;
pub mod ring;
pub mod signals;
pub mod tick;

pub use instance::{ExitInfo, ExitReason, Instance, LaunchPlan};
pub use java::{JavaCandidate, JavaManager};
pub use manager::{ManagerPolicy, NullDelegate, ProcessDelegate, ProcessManager, ShutdownReport};
pub use ring::{strip_ansi, LogRing};
