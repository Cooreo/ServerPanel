//! `sp-net` — one HTTP client for every outbound call the panel makes.
//!
//! Why it exists: Mojang, PaperMC, Adoptium, Modrinth and GitHub all need the
//! same four things — a descriptive `User-Agent`, retries with backoff, streaming
//! into a `.part` file, and hash verification on completion. Doing that once here
//! means a flaky connection is handled identically for a 40 MB server jar and a
//! 400 MB modpack.
//!
//! TLS is rustls only, so Windows/macOS users never need OpenSSL.
#![forbid(unsafe_code)]

pub mod cache;
pub mod client;
pub mod download;

pub use cache::TtlCache;
pub use client::{HttpClient, HttpClientBuilder, Retry};
pub use download::{DownloadJob, Downloaded};
