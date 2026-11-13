# Changelog

All notable changes to ServerPanel are documented here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and this project adheres to
[Semantic Versioning](https://semver.org/spec/v2.0.0.html).

What is *not* here yet is a version heading with a date: no release has been cut.
The first tag (`v0.1.0`) appears once the desktop app and the web UI exist, because a
ServerPanel release is defined by its installers — tagging earlier would publish a
GitHub Release with nothing to download. `.github/workflows/release.yml` is already in
place and will build and attach them when that tag is pushed; `scripts/release.sh`
bumps every version string, promotes this file's `Unreleased` section, and tags.

## [Unreleased]

### Added — data + platform layer

- `sp-core`: model types (`Server`, `ServerStatus` including `NotInstalled`/`Starting`/
  `Crashed`, `ResourceStats`, `LogLine`, `PanelEvent`), newtype ids with a
  `MAX_SAFE_ID` guard for JS numbers, `CoreError` with stable machine-readable codes
  and HTTP statuses, `Sandbox` path resolution (no escape from a server directory),
  `sanitize_slug`/`is_safe_file_name` for untrusted filenames, config loading with
  `SERVERPANEL__<SECTION>__<KEY>` overrides, `EventBus` (sequence-numbered fan-out
  with lag detection), throttled `ProgressReporter`, a small cron parser, JVM flag
  helpers (`heap_flags`, `validate_flags`), and streaming multi-hash verification.
- `sp-db`: SQLite via sqlx — forward-only migrations (33 objects), WAL + busy timeout
  + slow-statement logging, thirteen repositories (servers, users, sessions,
  settings, EULA, audit, backups, tasks, content, Java runtimes, metrics,
  notifications, update state), Argon2id password hashing with lockout, hashed
  session tokens + CSRF tokens, and enum `codec`s whose DB codes are asserted equal to
  their serde representations by a test.
- `sp-net`: the shared HTTP layer every outbound request goes through — retry policy
  honouring `Retry-After`, resumable downloads written to `.part` then verified and
  renamed, checksum mismatch → delete-and-fail, reuse-if-valid for cheap retries,
  stale `.part` cleanup at startup, a TTL cache with stale-while-revalidate, and
  upstream rate-limit tracking so a 429 is respected instead of provoked.

### Added — runtime layer

- `sp-process`: the instance supervisor. Argv built from `ServerRuntimeConfig`
  (never a shell string), `LogRing` with ANSI stripped for storage and preserved for
  the terminal, boot-marker detection with a bounded fallback, `TPS`/MSPT/player
  parsing from console output, console commands echoed into the log, save-flush
  escalation (`save-all flush` → `save-all`), the stop ladder
  `stop` → SIGTERM-to-process-group → SIGKILL, crash classification with a linear
  backoff restart policy and a runaway guard, `shutdown_all` with a deadline, Java
  runtime discovery plus Temurin install (SHA-256 verified, zip-slip safe), selection
  that refuses a runtime older than the game needs, and Windows Job Objects so a
  killed panel cannot orphan `javaw.exe`.
- `sp-process` holds the workspace's only `unsafe` block, confined to
  `job_object.rs` with `#![deny(unsafe_code)]` at the crate root and a comment per
  Win32 call.

### Added — software catalogue

- `sp-providers`: live catalogues for Mojang, Paper/Folia/Velocity/Waterfall, Purpur,
  Fabric, Quilt, Forge, NeoForge, Spigot/CraftBukkit and Geyser. No supported version,
  build number, loader version, library list or checksum is hard-coded — new Minecraft
  versions need no ServerPanel release.
- Mojang launch-profile parsing with rule evaluation (OS/arch/feature), profile
  `inherits_from` merging, and maven-coordinate paths.
- Era-aware installs: vanilla `server.jar`; Fabric/Quilt via their official headless
  installers; Forge/NeoForge via installer + the `unix_args.txt`/`win_args.txt` argfile
  the installer writes (the JVM expands `@argfile` itself — still no shell);
  Spigot/CraftBukkit by running BuildTools on the host with `javac`/`git` detected
  first; Geyser from GitHub releases; BungeeCord as an explicit upload-with-instructions
  flow, because its jars are behind an account login.
- An install executor: `.part`-then-rename for every file, reuse of already-correct
  files, installer stdout/stderr streamed into the server console ring,
  expected-artifact verification instead of trusting exit codes, canonical jar rename
  with an `.old-<ms>` backup, and a `serverpanel-install.json` receipt so a repair
  replays the exact same plan.

### Added — content installs

- `sp-modrinth`: search, project/version metadata, and the install resolver built on
  top of them. Nothing is cached in the UI: every read goes through a 10-minute TTL
  cache keyed by URL, and upstream errors keep their service name so a Modrinth
  outage says "modrinth" rather than "internal error".
- Version selection that respects what a *server* can load: Minecraft version and
  loader filtering, `server_side: unsupported` projects skipped with an explanation
  rather than installed and blamed for a crash, prereleases only on request,
  `required` dependencies pulled transitively (depth-bounded, cycle-safe) and
  `optional` ones left out unless asked for, `incompatible` declarations winning
  over the user's own selection. Each chosen file records `required_by`, so the UI
  can answer "why is this here".
- `.mrpack` reading: index validation before any write, base64 digests normalised to
  hex, `envs` respected (client-only files are not copied into a server),
  `overrides/` then `server-overrides/` so the server-specific tree wins, existing
  user-edited files kept unless the caller says overwrite, and a plan diffed against
  on-disk SHA-1 so a re-run fetches only what changed and re-fetches what is corrupt.
- Update checks against the files actually installed, via Modrinth's hash lookup with
  a per-project fallback when that endpoint changes shape.

### Added — backups

- `sp-backup`: zip/deflate and tar+zstd archives, both carrying a
  `serverpanel-backup.json` manifest (per-file size, mtime, SHA-256) that makes an
  archive self-describing and a restore reviewable before it happens.
  `Selector::plan` decides contents first, so the progress total is honest and
  `max_total_bytes` can refuse before a disk fills; symlinks are reported, never
  followed; two runs over an unchanged tree produce the same entry order.
- Create writes `.part` → hash → rename, so an interrupted backup can never be
  mistaken for a complete one; `verify` re-reads every entry (which forces the CRC
  check) and cross-checks it against the manifest, catching storage rot that a file
  digest cannot; restore always takes a `pre-restore` copy first and refuses to run
  against a live server; retention prunes to `keep_last`/`keep_days` and never drops
  the pre-restore copy that a restore already used.
- `Quiesce` is the seam to RCON (`save-off`/`save-all flush`/`save-on`) that licenses
  `world_consistent`, and the scheduler is stateless — due means "a fire time exists
  since the last completed backup", so a panel that was off overnight catches up once
  and cannot double-run.

### Added — repository plumbing

- `LICENSE` (MIT), this `CHANGELOG.md`, `README.md`.
- `.github/workflows/ci.yml`: `cargo fmt --check`, `clippy -D warnings` (Linux +
  Windows), tests on Linux/Windows/macOS, an MSRV check at the declared
  `rust-version`, and frontend typecheck/lint/Vitest/Playwright/build jobs that
  activate automatically once `web/` has a `package.json`.
- `.github/workflows/release.yml`: tag-triggered. Verifies the tag matches the
  workspace version, requires a `CHANGELOG.md` section, drafts a release with notes
  generated from it, builds Windows (NSIS + MSI), Linux (AppImage + deb) and macOS
  (universal dmg) bundles, adds the headless server binary per platform, and refuses
  to publish a release that ended up with no installer attached.
- `.github/dependabot.yml` for cargo, npm and actions; `scripts/release.sh` as the
  only supported way to bump versions and tag.

### Verification status

CI compiles the workspace on every push (`manifest` → `rustfmt` → `clippy` → `test`,
plus an MSRV job), which is the first time this tree has met a Rust compiler: that
pass found and fixed two non-existent crate features (`reqwest/proxy`,
`totp-rs/gen`), a swapped argument pair, and a workflow expression that made GitHub
fail the run before any step produced a log. The sandbox this is written in still has
no toolchain, so local verification is limited to `tools/rs-check.js` (tree-sitter
parse), `tools/dep-check.py` (workspace wiring), `tools/wf-check.py` (workflow
expressions and `needs`) and `tools/feat-check.py` (feature names against the
crates.io index) — all four run in CI too, ahead of the expensive jobs.

Remaining before `v0.1.0`: `sp-server` (axum API, SSE event stream, auth sessions,
file editor over `Sandbox`, RCON, job runner), `sp-desktop` (Tauri shell: tray,
autostart, self-update) and `web/` (the UI). The installers attached to a release are
what make a ServerPanel release usable on Windows, so the tag waits for those three.
