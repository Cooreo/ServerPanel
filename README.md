# ServerPanel

A desktop-first control panel for Minecraft servers: one app that installs the
server software, runs it, backs it up, edits its files, and stays out of the way.
Built for Windows first (an installer, not a zip of jars), with Linux and macOS
binaries from the same code base.

**Status: under construction, no releases yet.** The engine layer is written
(`sp-core`, `sp-db`, `sp-net`, `sp-process`, `sp-providers`); the API server, the
desktop shell and the web UI are not, so there is nothing to download and no tag. A
ServerPanel release is defined by its installers — `v0.1.0` gets tagged when
`sp-desktop` and `web/` exist, and `.github/workflows/release.yml` builds and attaches
them from that one tag. Until then, [CHANGELOG.md](CHANGELOG.md) records what is in
the tree and what stands between it and that tag.

[![CI](https://github.com/Cooreo/ServerPanel/actions/workflows/ci.yml/badge.svg)](https://github.com/Cooreo/ServerPanel/actions/workflows/ci.yml)

## Why this exists

Most self-hosted Minecraft panels are built to manage many tenants on one Linux box,
which shows: you need a reverse proxy, a database server, and a browser you trust not
to be logged into anything else. The things that actually go wrong on a personal
server — an orphaned `javaw.exe` holding port 25565 after a crash, a Forge install
that half-downloaded, a world lost to a backup that never ran, a 1.21.4 server
launched on Java 8 — are what this project treats as the product:

- **No orphans.** Every JVM on Windows is born inside a Job Object with
  `KILL_ON_JOB_CLOSE`, and on Unix into its own process group, so a panel that is
  killed, updated or loses power takes its servers with it instead of leaking them.
- **Nothing half-installed.** Downloads land in `.part`, are checksum-verified, then
  renamed. A truncated Paper build is never `server.jar`. Retries reuse what is
  already correct, so "try again" costs nothing.
- **Version lists are live.** Supported Minecraft versions, Paper/Purpur build
  numbers, Fabric/Quilt loader versions, Forge and NeoForge promotions, library lists
  and checksums all come from the upstream APIs at install time. Minecraft 1.99 needs
  no ServerPanel release.
- **The right Java.** Per-server runtime selection reads Mojang's own
  `javaVersion.majorVersion` when available, installs a Temurin JRE into the panel's
  data directory when nothing suitable exists, and never picks a runtime *older* than
  the game requires — the failure people describe as "corrupted install".
- **Backups that restore.** Snapshot scheduling with retention, `save-off`/`flush`
  quiescing around the read, verification of the archive it wrote, and a restore that
  refuses to touch a running server.
- **Your files stay yours.** All server data lives in the user's data directory, never
  under `Program Files`; every path from the UI is resolved through a sandbox, so
  `../../` is a 400 rather than a read of `/etc/passwd`.

## Architecture

Six library crates and, eventually, two binaries and a SPA. Dependencies point one
way: the API layer owns HTTP and orchestration; nothing below it knows what a request
is.

```
sp-core       models, ids, errors, paths/sandbox, config, event bus, progress,
              cron, JVM flags, hashing          (no I/O beyond the filesystem it owns)
sp-db         SQLite (sqlx), migrations, 13 repositories, Argon2id auth primitives
sp-net        one shared HTTP client: retries, resumable downloads, TTL cache,
                upstream rate-limit accounting
sp-process    the instance supervisor: argv, log ring, console parsing, restart
                policy, Java discovery + Temurin install, Windows Job Objects
sp-providers  server-software catalogues + the install executor
sp-modrinth   mods, plugins, resource packs, modpacks (planned)
sp-backup     create/restore/verify, retention, scheduling (planned)
sp-server     axum REST + WebSocket console + SSE event stream, rust-embed UI (planned)
sp-desktop    Tauri shell: tray, autostart, single instance, self-update (planned)
web/          React + TypeScript + Tailwind SPA, shared with the browser build (planned)
```

Two rules hold the whole thing together:

1. **`unsafe` is denied workspace-wide**, with exactly one documented exception
   (`sp-process/src/job_object.rs`, where Win32 FFI is unavoidable); `grep -rn "unsafe " crates/`
   returns that file and nothing else.
2. **A child process is never launched through a shell.** Argv is a `Vec<String>`
   built from validated fields; JVM flags are checked so a "flag" cannot smuggle an
   argument into the command line.

Data goes to `~/.local/share/ServerPanel` (Linux),
`%APPDATA%\ServerPanel` (Windows), `~/Library/Application Support/ServerPanel`
(macOS); override with `SERVERPANEL_HOME`. Logs and config sit beside it, not in the
install directory, so an update never touches a world.

## Building from source

The engine crates are library-only today, so there is no binary to run yet — this
builds the parts that exist and runs their tests.

```sh
cargo test --workspace          # unit + integration tests for all five crates
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --all --check
```

Requires Rust 1.80 or newer (the workspace `rust-version`; CI also checks it).
`sp-process` links `nix` on Unix and the `windows` crate's Job Object APIs on
Windows; both are target-gated, so each platform builds without the other.

Outbound HTTPS is expected at *runtime*, not build time: the version catalogues are
fetched on demand, and every provider test runs against fixtures and a loopback mock
instead of the network, so CI is deterministic and offline-safe.

## Status

| Area | State |
|---|---|
| Models, errors, paths, config, events, progress, cron, flags, hashing | written |
| SQLite schema, migrations, repositories, auth primitives | written |
| Shared HTTP client, downloads, cache, rate limits | written |
| Process supervisor, console log ring, Java runtime manager, Job Objects | written |
| Catalogues + install executor (10 server types) | written |
| Mod/plugin/modpack installs, backup engine | next |
| HTTP API, SSE, WebSocket console, file editor, RCON | planned |
| Tauri desktop shell + Windows installer | planned |
| Web UI (React, themed, animated) | planned |
| `docs/` (architecture, providers, API, security, packaging) | planned |

Nothing above is a promise about how fast it lands, and nothing marked "written" has
been compiled yet: the environment this code was authored in has no Rust toolchain and
no route to install one, so CI is the first real `cargo build`. Expect the green badge
to arrive after some fixes, not immediately.

## Contributing

Branch, open a PR, let CI do the arguing. `cargo fmt` and `clippy -D warnings` are
enforced, as are the two rules above. The release process is `scripts/release.sh
<version>`, which bumps every version string, promotes the `Unreleased` section of
`CHANGELOG.md`, and tags — a tag pushed by hand is rejected by the preflight job if
those disagree.

## License

MIT — see [LICENSE](LICENSE).

ServerPanel is an independent project and is not affiliated with, endorsed by, or
sponsored by Mojang Studios or Microsoft. "Minecraft" is a trademark of Mojang AB. The
EULA for the software you install is yours to accept: the panel records your agreement
per server and will never accept it for you.
