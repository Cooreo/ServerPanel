//! "Back this up every night at 03:15" — and what happens when the panel was
//! asleep at 03:15.
//!
//! The schedule is deliberately *stateless*: there is no `next_run_at` column, no
//! timer list, no "did we already run this job" flag. The database already knows
//! when each server was last backed up, and the cron expression says when a run
//! *would* fire, so a due check is one comparison. That choice has three
//! consequences this module implements:
//!
//! * A panel restarted at 04:00 after being down all night still backs the server
//!   up, because a fire time exists inside the gap since the last backup.
//! * A panel restarted at 03:16 does **not** run twice: after the 03:15 run the
//!   last backup is newer than any fire time.
//! * A machine that is off at 03:15 every day gets exactly one backup per boot,
//!   not one per missed day. Backfilling a week of missed backups because the
//!   server was patched would fill the disk with copies of a world that has not
//!   changed — the newest state is what matters, so one catch-up run is enough.

use crate::engine::BackupEngine;
use crate::selector::Cancel;
use crate::CreateRequest;
use sp_core::progress::{ProgressPhase, ProgressReporter};
use sp_core::{BackupKind, BackupPolicy, CoreError, ServerId};
use sp_db::Db;
use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

/// The instant of the next fire strictly after `from_ms`, if the expression can
/// ever fire again.
pub fn next_fire(expr: &str, from_ms: i64) -> Result<Option<i64>, CoreError> {
    let cron = sp_core::cron::Cron::parse(expr)?;
    let from = millis_to_utc(from_ms);
    Ok(cron.next_after(from).map(|d| d.timestamp_millis()))
}

fn millis_to_utc(ms: i64) -> chrono::DateTime<chrono::Utc> {
    chrono::DateTime::from_timestamp_millis(ms)
        .unwrap_or_else(|_| chrono::DateTime::from_timestamp_millis(0).expect("the epoch is representable"))
}

/// Whether a policy is due at `now_ms`, given the last completed backup.
///
/// `None` for `since_ms` means "never backed up", which is always due — a policy
/// switched on should take its first backup immediately rather than at 03:15 next
/// Tuesday, because the user turned it on precisely because they have no backup.
pub fn is_due(expr: &str, since_ms: Option<i64>, now_ms: i64) -> Result<bool, CoreError> {
    let since = match since_ms {
        Some(ms) => ms,
        None => return Ok(true),
    };
    // `next_after` is strict, so a backup taken *at* a fire time means the next fire
    // is the following one: no double run when a manual backup and the nightly job
    // collide, and no missed run when the clock jumps.
    Ok(next_fire(expr, since)?.is_some_and(|next| next <= now_ms))
}

/// One server's standing for one tick.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SchedulerTick {
    pub server_id: ServerId,
    pub due: bool,
    /// Next fire time after `now_ms`, for the UI's "next backup in 6 h".
    pub next_ms: Option<i64>,
    pub last_ms: Option<i64>,
}

impl SchedulerTick {
    pub fn human_countdown(&self, now_ms: i64) -> String {
        match self.next_ms {
            Some(next) if next > now_ms => human_duration((next - now_ms) / 1000),
            Some(_) => "now".into(),
            None => "no further runs scheduled".into(),
        }
    }
}

fn human_duration(secs: i64) -> String {
    let (d, h, m) = (secs / 86400, (secs % 86400) / 3600, (secs % 3600) / 60);
    if d > 0 {
        format!("{d}d {h}h")
    } else if h > 0 {
        format!("{h}h {m}m")
    } else {
        format!("{m}m")
    }
}

/// Decide which servers run now. Pure, so the policy maths has tests without a
/// database.
pub fn plan_tick(
    policies: &[(ServerId, BackupPolicy)],
    last_backup: &BTreeMap<ServerId, i64>,
    now_ms: i64,
) -> Result<Vec<SchedulerTick>, CoreError> {
    let mut out = Vec::with_capacity(policies.len());
    for (id, policy) in policies {
        if !policy.enabled {
            continue;
        }
        let since = last_backup.get(id).copied();
        out.push(SchedulerTick {
            server_id: *id,
            due: is_due(&policy.cron, since, now_ms)?,
            next_ms: next_fire(&policy.cron, now_ms)?,
            last_ms: since,
        });
    }
    Ok(out)
}

/// The loop that calls the engine.
///
/// It never exits on its own: a bad cron expression on one server, or one server
/// whose directory is on a disconnected NAS, must not end backups for every other
/// server on the host. Errors are logged and retried on the next tick.
pub struct BackupScheduler {
    engine: Arc<BackupEngine>,
    db: Arc<Db>,
    dirs: Arc<dyn ServerDirs + Send + Sync>,
    tick: Duration,
    cancel: Cancel,
}

/// How the scheduler finds a server's directory. Implemented by `sp-server` over
/// its registry; a test implements it with a map.
pub trait ServerDirs: Send + Sync {
    fn dir(&self, server_id: ServerId) -> Option<std::path::PathBuf>;
    /// Called before a scheduled run so the panel can refuse to back up a server
    /// that is mid-install (whose directory is half-written by definition).
    fn is_installing(&self, _server_id: ServerId) -> bool {
        false
    }
}

impl BackupScheduler {
    pub fn new(
        engine: Arc<BackupEngine>,
        db: Arc<Db>,
        dirs: Arc<dyn ServerDirs + Send + Sync>,
    ) -> Self {
        Self { engine, db, dirs, tick: Duration::from_secs(60), cancel: Cancel::new() }
    }

    /// Check for due work once and run it. Exposed for tests, for a
    /// "run scheduled backups now" button, and for cron-driven systems that prefer
    /// to be poked by the OS rather than keep a thread alive.
    pub async fn run_once(&self, now_ms: i64) -> Result<Vec<ServerId>, CoreError> {
        let pairs = self.db.backups().servers_with_enabled_policies().await?;
        if pairs.is_empty() {
            return Ok(Vec::new());
        }
        let mut last: BTreeMap<ServerId, i64> = BTreeMap::new();
        for (id, _) in &pairs {
            // `Scheduled` and `Manual` both count as "we backed this up", otherwise
            // one manual backup before 03:15 still triggers the nightly copy, and
            // one the night after does not.
            let rows = self.db.backups().list_for_server(*id, 1).await?;
            if let Some(newest) = rows
                .iter()
                .filter(|b| {
                    b.state == sp_core::BackupState::Completed
                        && matches!(b.kind, BackupKind::Scheduled | BackupKind::Manual)
                })
                .map(|b| b.created_at_ms)
                .max()
            {
                last.insert(*id, newest);
            }
        }
        let mut done = Vec::new();
        for tick in plan_tick(&pairs, &last, now_ms)? {
            if !tick.due || self.cancel.is_cancelled() {
                continue;
            }
            let policy = match pairs.iter().find(|(id, _)| *id == tick.server_id) {
                Some((_, p)) => p.clone(),
                None => continue,
            };
            if self.dirs.is_installing(tick.server_id) {
                tracing::info!(server = %tick.server_id, "skipping a scheduled backup of a server that is installing");
                continue;
            }
            let Some(dir) = self.dirs.dir(tick.server_id) else {
                tracing::warn!(server = %tick.server_id, "a backup policy points at a server that no longer exists");
                continue;
            };
            let stamp = chrono::DateTime::from_timestamp_millis(now_ms)
                .map(|d| d.format("%Y-%m-%d %H:%M").to_string())
                .unwrap_or_else(|| "scheduled".into());
            let req = CreateRequest {
                server_id: tick.server_id,
                server_dir: dir,
                name: format!("scheduled {stamp}"),
                kind: BackupKind::Scheduled,
                format: policy.format,
                destination: crate::Destination::Local {
                    root: self.engine.root()?.to_path_buf(),
                },
                excludes: Vec::new(),
                include_worlds: policy.include_worlds,
                include_logs: policy.include_logs,
                quiesce: policy.quiesce,
                max_total_bytes: 0,
                cancel: self.cancel.clone(),
            };
            let progress = ProgressReporter::silent(
                format!("backup-scheduled-{}", tick.server_id),
                Some(tick.server_id),
                ProgressPhase::Backup,
            );
            match self.engine.create(req, progress).await {
                Ok(o) => {
                    tracing::info!(
                        server = %tick.server_id,
                        bytes = o.copy.size_bytes,
                        files = o.copy.file_count,
                        consistent = o.copy.world_consistent,
                        "scheduled backup finished"
                    );
                    done.push(tick.server_id);
                }
                Err(e) => {
                    // Logged, not propagated: one failing server is not a reason to
                    // skip the others in this tick, and the next tick retries.
                    tracing::warn!(server = %tick.server_id, error = %e, "scheduled backup failed");
                }
            }
            if let Err(e) = self.engine.prune_with(tick.server_id, &policy).await {
                tracing::warn!(server = %tick.server_id, error = %e, "could not apply the retention policy");
            }
        }
        Ok(done)
    }

    /// Stop after the current file boundary.
    pub fn stop(&self) {
        self.cancel.cancel();
    }

    /// The long-running form of [`Self::run_once`]: wake on the minute, check,
    /// sleep. Minute resolution is what cron expressions are written in, so the
    /// loop never needs to know anything about the schedule to sleep correctly.
    pub async fn run(self: Arc<Self>) {
        loop {
            let now = chrono::Utc::now().timestamp_millis();
            if let Err(e) = self.run_once(now).await {
                tracing::warn!(error = %e, "backup scheduler tick failed");
            }
            if self.cancel.is_cancelled() {
                return;
            }
            // Align to the next minute boundary so a "0 3 * * *" job fires at
            // 03:00:00-03:00:59 rather than drifting with the panel's start time.
            let ms_to_next = 60_000 - (chrono::Utc::now().timestamp_millis().rem_euclid(60_000));
            tokio::time::sleep(Duration::from_millis(ms_to_next as u64 + 250)).await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn next_and_last_fire_bracket_the_present() {
        // 2026-11-13T03:15:00Z exactly.
        let now = chrono::DateTime::parse_from_rfc3339("2026-11-13T03:15:00Z")
            .unwrap()
            .with_timezone(&chrono::Utc)
            .timestamp_millis();
        let next = next_fire("15 3 * * *", now).unwrap().unwrap();
        assert!(next > now, "the next run must be strictly after now");
        assert_eq!(next - now, 24 * 3600 * 1000, "daily at 03:15 -> 24 h later, not 23:59");
        // A fire time exactly at `now` counts as reachable from one minute ago...
        assert_eq!(next_fire("15 3 * * *", now - 60_000).unwrap().unwrap(), now);
        // ...and strictly later than `now` from `now`, so the schedule never re-runs
        // the instant it just ran.
        assert!(next_fire("15 3 * * *", now).unwrap().unwrap() > now);
    }

    #[test]
    fn due_requires_a_fire_time_inside_the_gap() {
        let t = |s: &str| {
            chrono::DateTime::parse_from_rfc3339(s).unwrap().with_timezone(&chrono::Utc).timestamp_millis()
        };
        // Backed up at 02:00, now 03:20, schedule 03:15 → due.
        assert!(is_due("15 3 * * *", Some(t("2026-11-13T02:00:00Z")), t("2026-11-13T03:20:00Z")).unwrap());
        // Backed up at 03:19 (after the fire time) → not due until tomorrow.
        assert!(!is_due("15 3 * * *", Some(t("2026-11-13T03:19:00Z")), t("2026-11-13T03:20:00Z")).unwrap());
        assert!(is_due("15 3 * * *", Some(t("2026-11-13T03:19:00Z")), t("2026-11-14T03:16:00Z")).unwrap());
        // Never backed up → due, immediately.
        assert!(is_due("15 3 * * *", None, t("2026-11-13T12:00:00Z")).unwrap());
        // A week of missed fires collapses into one run, not seven: `is_due` says
        // "yes" once, and after that run the last backup is newer than the fire time.
        let missed = t("2026-11-13T03:20:00Z");
        assert!(is_due("15 3 * * *", Some(t("2026-11-06T03:15:00Z")), missed).unwrap());
        assert!(!is_due("15 3 * * *", Some(missed), missed).unwrap());
    }

    #[test]
    fn a_missed_night_while_the_machine_was_off_still_runs() {
        let t = |s: &str| {
            chrono::DateTime::parse_from_rfc3339(s).unwrap().with_timezone(&chrono::Utc).timestamp_millis()
        };
        // Last backup 4 days ago at 03:15; the panel was down nightly since; boot at 09:00.
        let since = t("2026-11-09T03:15:00Z");
        let now = t("2026-11-13T09:00:00Z");
        assert!(is_due("15 3 * * *", Some(since), now).unwrap());
        // ...and after that one catch-up run, nothing is due again.
        assert!(!is_due("15 3 * * *", Some(now), now).unwrap());
    }

    #[test]
    fn hourly_and_weekly_expressions_behave() {
        let t = |s: &str| {
            chrono::DateTime::parse_from_rfc3339(s).unwrap().with_timezone(&chrono::Utc).timestamp_millis()
        };
        let now = t("2026-11-13T03:15:00Z");
        assert_eq!(next_fire("0 * * * *", now).unwrap().unwrap() - now, 45 * 60 * 1000);
        // 2026-11-13 is a Friday; Sunday at 04:00.
        let weekly = next_fire("0 4 * * 0", now).unwrap().unwrap();
        let d = chrono::DateTime::from_timestamp_millis(weekly).unwrap();
        assert_eq!(d.format("%Y-%m-%d %H:%M").to_string(), "2026-11-15 04:00");
        assert!(next_fire("0 4 1 1 *", now).unwrap().unwrap() > t("2026-12-31T00:00:00Z"));
        assert!(matches!(next_fire("not a cron", now), Err(_)));
    }

    #[test]
    fn plan_tick_skips_disabled_and_not_due_and_sorts_nothing_away() {
        let t = chrono::DateTime::parse_from_rfc3339("2026-11-13T03:20:00Z")
            .unwrap()
            .with_timezone(&chrono::Utc)
            .timestamp_millis();
        let mut on = BackupPolicy::default();
        on.enabled = true;
        on.cron = "15 3 * * *".into();
        let off = BackupPolicy { enabled: false, ..on.clone() };
        let policies = vec![
            (ServerId::new(1), on.clone()),
            (ServerId::new(2), off),
            (ServerId::new(3), on),
        ];
        let mut last = BTreeMap::new();
        last.insert(ServerId::new(1), t - 24 * 3600 * 1000); // yesterday → due
        last.insert(ServerId::new(3), t - 60_000); // just now → not due
        let ticks = plan_tick(&policies, &last, t).unwrap();
        assert_eq!(ticks.len(), 2, "the disabled policy is not scheduled at all");
        assert!(ticks[0].due, "server 1 is overdue");
        assert!(!ticks[1].due, "server 3 ran five minutes ago");
        assert_eq!(ticks[0].last_ms, Some(t - 24 * 3600 * 1000));
        assert!(ticks[0].next_ms.unwrap() > t);
        let countdown = ticks[0].human_countdown(t);
        assert!(countdown.ends_with('h') || countdown.ends_with('m'), "{countdown}");
        assert_eq!(
            SchedulerTick { server_id: ServerId::new(9), due: true, next_ms: Some(t), last_ms: None }
                .human_countdown(t),
            "now"
        );
        assert_eq!(
            SchedulerTick { server_id: ServerId::new(9), due: false, next_ms: None, last_ms: None }
                .human_countdown(t),
            "no further runs scheduled"
        );
    }

    #[test]
    fn a_broken_cron_is_a_validation_error_not_a_panic() {
        for bad in ["", "75 * * * *", "* * * *", "a b c d e", "*/0 * * * *"] {
            let r = is_due(bad, None, 0);
            match r {
                Ok(v) => assert!(v, "an always-valid expression should not be treated as due: {bad}"),
                Err(e) => assert!(!e.to_string().is_empty(), "{bad} produced an empty error"),
            }
        }
    }

    #[test]
    fn human_duration_reads_well() {
        assert_eq!(human_duration(59), "0m");
        assert_eq!(human_duration(60), "1m");
        assert_eq!(human_duration(3_725), "1h 2m");
        assert_eq!(human_duration(90_000), "1d 1h");
    }

    #[test]
    fn epoch_helper_clamps_garbage_instead_of_panicking() {
        assert_eq!(millis_to_utc(0).timestamp_millis(), 0);
        assert!(millis_to_utc(i64::MIN).timestamp_millis() <= 0);
        assert!(millis_to_utc(i64::MAX).timestamp_millis() > 0);
    }

    #[tokio::test]
    async fn run_once_is_a_noop_with_no_policies() {
        let db = Arc::new(Db::in_memory().await.unwrap());
        let engine = Arc::new(BackupEngine::new(
            db.clone(),
            crate::Destination::Local { root: std::env::temp_dir() },
        ));
        let dirs = Arc::new(MapDirs(BTreeMap::new()));
        let s = BackupScheduler::new(engine, db, dirs);
        assert!(s.run_once(chrono::Utc::now().timestamp_millis()).await.unwrap().is_empty());
    }

    /// A server row is required by `backups.server_id`'s foreign key, so tests create
    /// one the same way the panel does.
    async fn new_server(db: &Db, dir: &std::path::Path) -> ServerId {
        let mut config = sp_core::model::ServerRuntimeConfig::default();
        config.server_dir = dir.to_path_buf();
        let draft = sp_core::model::ServerDraft {
            name: format!("srv-{}", dir.file_name().unwrap().to_string_lossy()),
            kind: sp_core::model::ServerKind::Paper,
            mc_version: "1.21.4".into(),
            build: "1".into(),
            server_dir: dir.to_path_buf(),
            config,
            eula_accepted: false,
            install_state: sp_core::model::InstallState::Pending,
            template_of: None,
            tags: vec![],
            position: None,
        };
        db.servers().create(draft).await.unwrap().id
    }

    struct MapDirs(BTreeMap<ServerId, std::path::PathBuf>);
    impl ServerDirs for MapDirs {
        fn dir(&self, id: ServerId) -> Option<std::path::PathBuf> {
            self.0.get(&id).cloned()
        }
    }

    #[tokio::test]
    async fn run_once_backs_up_a_due_server_and_records_it() {
        let dir = tempfile::tempdir().unwrap();
        let server_dir = dir.path().join("srv");
        std::fs::create_dir_all(server_dir.join("world")).unwrap();
        std::fs::write(server_dir.join("server.properties"), b"level-seed=x\n").unwrap();
        std::fs::write(server_dir.join("world/level.dat"), b"dat").unwrap();
        let backups = dir.path().join("backups");
        std::fs::create_dir_all(&backups).unwrap();

        let db = Arc::new(Db::in_memory().await.unwrap());
        // A real server row is required by the foreign key.
        let server_id = new_server(&db, &server_dir).await;
        let mut policy = BackupPolicy::default();
        policy.enabled = true;
        policy.cron = "15 3 * * *".into();
        policy.quiesce = false;
        db.backups().set_policy(server_id, &policy).await.unwrap();

        let mut dirs = BTreeMap::new();
        dirs.insert(server_id, server_dir.clone());
        let engine = Arc::new(BackupEngine::new(db.clone(), crate::Destination::Local { root: backups.clone() }));
        let scheduler = BackupScheduler::new(engine, db.clone(), Arc::new(MapDirs(dirs)));

        let now = chrono::DateTime::parse_from_rfc3339("2026-11-13T03:20:00Z")
            .unwrap()
            .with_timezone(&chrono::Utc)
            .timestamp_millis();
        let done = scheduler.run_once(now).await.unwrap();
        assert_eq!(done, vec![server_id]);
        let rows = db.backups().list_for_server(server_id, 10).await.unwrap();
        assert_eq!(rows.len(), 1, "one scheduled run for one due policy");
        assert_eq!(rows[0].kind, BackupKind::Scheduled);
        assert_eq!(rows[0].state, sp_core::BackupState::Completed);
        assert_eq!(rows[0].format, policy.format);
        let on_disk: Vec<_> = std::fs::read_dir(&backups).unwrap().filter_map(|e| e.ok()).collect();
        assert_eq!(on_disk.len(), 1, "exactly one archive, no .part left behind");
        assert!(!on_disk[0].path().to_string_lossy().ends_with(".part"));

        // The second tick must not duplicate the work.
        let again = scheduler.run_once(now + 60_000).await.unwrap();
        assert!(again.is_empty(), "the same fire time must not run twice");
        // ...and a day later it must.
        let next_day = now + 24 * 3600 * 1000;
        assert_eq!(scheduler.run_once(next_day).await.unwrap(), vec![server_id]);
        assert_eq!(db.backups().list_for_server(server_id, 10).await.unwrap().len(), 2);
    }

    #[tokio::test]
    async fn a_policy_pointing_at_a_deleted_server_is_skipped_not_fatal() {
        let db = Arc::new(Db::in_memory().await.unwrap());
        let server_id = new_server(&db, &std::env::temp_dir().join("ghost")).await;
        let mut policy = BackupPolicy::default();
        policy.enabled = true;
        policy.cron = "* * * * *".into();
        db.backups().set_policy(server_id, &policy).await.unwrap();
        let engine = Arc::new(BackupEngine::new(
            db.clone(),
            crate::Destination::Local { root: std::env::temp_dir() },
        ));
        let scheduler = BackupScheduler::new(engine, db.clone(), Arc::new(MapDirs(BTreeMap::new())));
        let now = chrono::Utc::now().timestamp_millis();
        assert!(scheduler.run_once(now).await.unwrap().is_empty());
        assert!(db.backups().list_for_server(server_id, 10).await.unwrap().is_empty());
    }
}
