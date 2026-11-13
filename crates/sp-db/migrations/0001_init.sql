-- ServerPanel initial schema.
-- Applied by sp_db::migrate inside a transaction, one statement at a time.
-- Timestamps are unix milliseconds in INTEGER columns (see sp_core::model::now_ms)
-- so there is exactly one time representation across the app.
--
-- Note: no PRAGMA lines here. Migrations run inside a transaction, and SQLite
-- ignores `PRAGMA foreign_keys` inside one — those settings live in
-- `sp_db::db::connect_options`, which is the single source of truth.CREATE TABLE IF NOT EXISTS meta (
    key         TEXT PRIMARY KEY,
    value       TEXT NOT NULL
);

-- ---------------------------------------------------------------- auth ----
CREATE TABLE IF NOT EXISTS users (
    id                 INTEGER PRIMARY KEY,
    username           TEXT NOT NULL UNIQUE,
    -- Argon2id PHC string, e.g. $argon2id$v=19$m=19456,t=2,p=1$<salt>$<hash>
    password_hash      TEXT NOT NULL,
    role               TEXT NOT NULL DEFAULT 'admin',
    totp_secret        TEXT,
    totp_enabled       INTEGER NOT NULL DEFAULT 0,
    created_at_ms      INTEGER NOT NULL,
    last_login_at_ms   INTEGER,
    failed_attempts    INTEGER NOT NULL DEFAULT 0,
    locked_until_ms    INTEGER,
    theme_pref         TEXT,
    density_pref       TEXT,
    accent_pref        TEXT
);

CREATE TABLE IF NOT EXISTS sessions (
    token_hash     TEXT PRIMARY KEY,
    user_id        INTEGER NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    created_at_ms  INTEGER NOT NULL,
    expires_at_ms  INTEGER NOT NULL,
    last_seen_ms   INTEGER NOT NULL,
    ip             TEXT,
    user_agent     TEXT,
    csrf_token     TEXT NOT NULL,
    revoked_at_ms  INTEGER
);
CREATE INDEX IF NOT EXISTS idx_sessions_user ON sessions(user_id);
CREATE INDEX IF NOT EXISTS idx_sessions_expiry ON sessions(expires_at_ms);

-- ------------------------------------------------------------- servers ----
CREATE TABLE IF NOT EXISTS servers (
    id             INTEGER PRIMARY KEY,
    name           TEXT NOT NULL UNIQUE COLLATE NOCASE,
    kind           TEXT NOT NULL,
    mc_version     TEXT NOT NULL,
    build          TEXT NOT NULL DEFAULT '',
    server_dir     TEXT NOT NULL,
    -- full ServerRuntimeConfig as JSON: memory, flags, java, port, timeouts…
    config_json    TEXT NOT NULL,
    status         TEXT NOT NULL DEFAULT 'not-installed',
    eula_accepted  INTEGER NOT NULL DEFAULT 0,
    installed_at_ms INTEGER,
    -- 'installing' | 'ready' | 'failed' — separates "never finished an install"
    -- from "installed and stopped"
    install_state  TEXT NOT NULL DEFAULT 'pending',
    install_error  TEXT,
    template_of    INTEGER REFERENCES servers(id) ON DELETE SET NULL,
    position       INTEGER NOT NULL DEFAULT 0,
    cpu_shares     INTEGER,
    tags_json      TEXT NOT NULL DEFAULT '[]',
    created_at_ms  INTEGER NOT NULL,
    updated_at_ms  INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_servers_position ON servers(position);

CREATE TABLE IF NOT EXISTS eula_agreements (
    id            INTEGER PRIMARY KEY,
    server_id     INTEGER NOT NULL REFERENCES servers(id) ON DELETE CASCADE,
    user_id       INTEGER REFERENCES users(id) ON DELETE SET NULL,
    accepted_at_ms INTEGER NOT NULL,
    text_sha256   TEXT NOT NULL,
    source_url    TEXT NOT NULL,
    user_agent    TEXT,
    ip            TEXT
);
CREATE INDEX IF NOT EXISTS idx_eula_server ON eula_agreements(server_id);

-- ---------------------------------------------------------------- java ----
CREATE TABLE IF NOT EXISTS java_runtimes (
    id               INTEGER PRIMARY KEY,
    major            INTEGER NOT NULL,
    version          TEXT NOT NULL,
    vendor           TEXT NOT NULL DEFAULT '',
    path             TEXT NOT NULL UNIQUE,
    source           TEXT NOT NULL,
    arch             TEXT NOT NULL DEFAULT '',
    sha256           TEXT,
    size_bytes       INTEGER,
    installed_at_ms  INTEGER,
    UNIQUE(major, path)
);

-- ------------------------------------------------------------- backups ----
CREATE TABLE IF NOT EXISTS backup_destinations (
    id            TEXT PRIMARY KEY,
    name          TEXT NOT NULL,
    kind          TEXT NOT NULL,           -- local | s3 | sftp
    config_json   TEXT NOT NULL,
    enabled       INTEGER NOT NULL DEFAULT 1,
    created_at_ms INTEGER NOT NULL
);

CREATE TABLE IF NOT EXISTS backups (
    id                 INTEGER PRIMARY KEY,
    server_id          INTEGER NOT NULL REFERENCES servers(id) ON DELETE CASCADE,
    destination_id     TEXT NOT NULL DEFAULT 'local',
    name               TEXT NOT NULL,
    kind               TEXT NOT NULL,      -- manual | scheduled | pre-restore | import
    format             TEXT NOT NULL,      -- zip | tar-zstd
    state              TEXT NOT NULL,      -- queued | running | completed | failed | restoring
    object_key         TEXT NOT NULL,
    size_bytes         INTEGER,
    sha256             TEXT,
    included_bytes     INTEGER NOT NULL DEFAULT 0,
    file_count         INTEGER NOT NULL DEFAULT 0,
    world_consistent   INTEGER NOT NULL DEFAULT 0,
    error              TEXT,
    created_at_ms      INTEGER NOT NULL,
    completed_at_ms    INTEGER,
    restored_count     INTEGER NOT NULL DEFAULT 0,
    last_restored_at_ms INTEGER
);
CREATE INDEX IF NOT EXISTS idx_backups_server ON backups(server_id, created_at_ms DESC);
CREATE INDEX IF NOT EXISTS idx_backups_state ON backups(state);

CREATE TABLE IF NOT EXISTS backup_policies (
    server_id      INTEGER PRIMARY KEY REFERENCES servers(id) ON DELETE CASCADE,
    enabled        INTEGER NOT NULL DEFAULT 0,
    cron           TEXT NOT NULL DEFAULT '0 4 * * *',
    format         TEXT NOT NULL DEFAULT 'tar-zstd',
    keep_last      INTEGER NOT NULL DEFAULT 7,
    keep_days      INTEGER NOT NULL DEFAULT 14,
    exclude_json   TEXT NOT NULL DEFAULT '[]',
    quiesce        INTEGER NOT NULL DEFAULT 1,
    include_worlds INTEGER NOT NULL DEFAULT 1,
    include_logs   INTEGER NOT NULL DEFAULT 0,
    destination_id TEXT NOT NULL DEFAULT 'local'
);

-- ----------------------------------------------------------- scheduler ----
CREATE TABLE IF NOT EXISTS scheduled_tasks (
    id                 INTEGER PRIMARY KEY,
    server_id          INTEGER REFERENCES servers(id) ON DELETE CASCADE,
    name               TEXT NOT NULL,
    action             TEXT NOT NULL,
    cron               TEXT NOT NULL,
    payload            TEXT NOT NULL DEFAULT '',
    enabled            INTEGER NOT NULL DEFAULT 1,
    only_if_running    INTEGER NOT NULL DEFAULT 0,
    warn_before_json   TEXT NOT NULL DEFAULT '[]',
    last_run_at_ms     INTEGER,
    last_result        TEXT,
    fail_count         INTEGER NOT NULL DEFAULT 0,
    created_at_ms      INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_tasks_enabled ON scheduled_tasks(enabled);

-- ------------------------------------------------------------- content ----
CREATE TABLE IF NOT EXISTS server_content (
    id             INTEGER PRIMARY KEY,
    server_id      INTEGER NOT NULL REFERENCES servers(id) ON DELETE CASCADE,
    kind           TEXT NOT NULL,
    project_id     TEXT,
    project_slug   TEXT,
    title          TEXT NOT NULL DEFAULT '',
    version_id     TEXT,
    version_number TEXT NOT NULL DEFAULT '',
    filename       TEXT NOT NULL,
    path           TEXT NOT NULL,
    enabled        INTEGER NOT NULL DEFAULT 1,
    source         TEXT NOT NULL DEFAULT 'modrinth',
    sha1           TEXT,
    sha512         TEXT,
    size_bytes     INTEGER,
    mc_versions_json TEXT NOT NULL DEFAULT '[]',
    loaders_json     TEXT NOT NULL DEFAULT '[]',
    install_json     TEXT NOT NULL DEFAULT '{}',
    installed_at_ms  INTEGER NOT NULL,
    updated_at_ms    INTEGER NOT NULL,
    UNIQUE(server_id, path)
);
CREATE INDEX IF NOT EXISTS idx_content_server ON server_content(server_id, kind);
CREATE INDEX IF NOT EXISTS idx_content_project ON server_content(project_id);

CREATE TABLE IF NOT EXISTS modpacks (
    id              INTEGER PRIMARY KEY,
    server_id       INTEGER NOT NULL REFERENCES servers(id) ON DELETE CASCADE,
    slug            TEXT NOT NULL,
    name            TEXT NOT NULL,
    version_number  TEXT NOT NULL,
    mc_version      TEXT NOT NULL,
    loader          TEXT NOT NULL,
    loader_version  TEXT NOT NULL,
    file_count      INTEGER NOT NULL DEFAULT 0,
    skipped_json    TEXT NOT NULL DEFAULT '[]',
    installed_at_ms INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_modpacks_server ON modpacks(server_id);

-- ------------------------------------------------------------ audit ----
CREATE TABLE IF NOT EXISTS audit_log (
    id         INTEGER PRIMARY KEY,
    at_ms      INTEGER NOT NULL,
    user_id    INTEGER REFERENCES users(id) ON DELETE SET NULL,
    username   TEXT,
    action     TEXT NOT NULL,
    target     TEXT,
    ip         TEXT,
    outcome    TEXT NOT NULL DEFAULT 'success',
    detail     TEXT
);
CREATE INDEX IF NOT EXISTS idx_audit_at ON audit_log(at_ms DESC);
CREATE INDEX IF NOT EXISTS idx_audit_server ON audit_log(target);

-- -------------------------------------------------------- monitoring ----
CREATE TABLE IF NOT EXISTS player_samples (
    server_id  INTEGER NOT NULL REFERENCES servers(id) ON DELETE CASCADE,
    t_ms       INTEGER NOT NULL,
    online     INTEGER NOT NULL,
    max        INTEGER NOT NULL,
    PRIMARY KEY (server_id, t_ms)
) WITHOUT ROWID;
CREATE INDEX IF NOT EXISTS idx_samples_time ON player_samples(server_id, t_ms DESC);

-- Downsampled minute buckets so a month of history stays cheap to read.
CREATE TABLE IF NOT EXISTS metric_samples (
    server_id      INTEGER NOT NULL REFERENCES servers(id) ON DELETE CASCADE,
    t_ms           INTEGER NOT NULL,
    cpu_percent    REAL NOT NULL,
    mem_used       INTEGER NOT NULL,
    mem_total      INTEGER NOT NULL,
    tps            REAL,
    mspt           REAL,
    players_online INTEGER NOT NULL DEFAULT 0,
    PRIMARY KEY (server_id, t_ms)
) WITHOUT ROWID;
CREATE INDEX IF NOT EXISTS idx_metrics_time ON metric_samples(server_id, t_ms DESC);

-- ---------------------------------------------------------- settings ----
CREATE TABLE IF NOT EXISTS settings (
    key   TEXT PRIMARY KEY,
    value TEXT NOT NULL
);

-- Queued outbound notifications (Discord/email/desktop) with retry state.
CREATE TABLE IF NOT EXISTS notifications (
    id            INTEGER PRIMARY KEY,
    created_at_ms INTEGER NOT NULL,
    channel       TEXT NOT NULL,
    title         TEXT NOT NULL,
    body          TEXT NOT NULL,
    level         TEXT NOT NULL DEFAULT 'info',
    server_id     INTEGER,
    attempts      INTEGER NOT NULL DEFAULT 0,
    delivered_at_ms INTEGER,
    last_error    TEXT
);
CREATE INDEX IF NOT EXISTS idx_notifications_pending ON notifications(delivered_at_ms) WHERE delivered_at_ms IS NULL;

-- Remembered "update available" answer so the UI does not hammer GitHub.
CREATE TABLE IF NOT EXISTS update_state (
    id                INTEGER PRIMARY KEY CHECK (id = 1),
    current_version     TEXT NOT NULL,
    available_version   TEXT,
    checked_at_ms       INTEGER,
    changelog_md        TEXT,
    download_url        TEXT,
    asset_name          TEXT,
    sha256              TEXT,
    staged_path         TEXT,
    previous_version_dir  TEXT,
    state               TEXT NOT NULL DEFAULT 'unknown',
    last_error          TEXT
);
