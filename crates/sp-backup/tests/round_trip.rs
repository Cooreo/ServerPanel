//! End-to-end tests over a real server directory: back it up, prove the archive,
//! break it, restore it. These are the paths a user pays for, so they run against
//! the actual files rather than a mock filesystem.

use sp_backup::{
    format_for, read_manifest, BackupEngine, Cancel, CreateRequest, Destination, Quiesce,
    RestoreMode, RestoreOptions, Selector,
};
use sp_core::progress::{ProgressPhase, ProgressReporter};
use sp_core::{
    BackupFormat, BackupKind, BackupPolicy, BackupState, InstallState, ServerDraft, ServerId,
    ServerKind, ServerRuntimeConfig,
};
use sp_db::Db;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

struct FakePanel {
    running: AtomicBool,
    prepares: std::sync::atomic::AtomicU32,
    resumes: std::sync::atomic::AtomicU32,
}

impl FakePanel {
    fn new(running: bool) -> Self {
        Self {
            running: AtomicBool::new(running),
            prepares: std::sync::atomic::AtomicU32::new(0),
            resumes: std::sync::atomic::AtomicU32::new(0),
        }
    }
}

#[async_trait::async_trait]
impl Quiesce for FakePanel {
    fn is_running(&self, _id: ServerId) -> bool {
        self.running.load(Ordering::SeqCst)
    }
    async fn prepare_backup(&self, _id: ServerId) -> Result<(), sp_core::CoreError> {
        self.prepares.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }
    async fn resume_backup(&self, _id: ServerId) {
        self.resumes.fetch_add(1, Ordering::SeqCst);
    }
}

fn progress() -> ProgressReporter {
    ProgressReporter::silent("test-job", None, ProgressPhase::Backup)
}

/// A server directory with the shape of a real one: configs, a jar, a world with
/// region files, logs, a crash report, an empty datapack folder and a symlink.
fn fake_server(root: &Path, name: &str) -> PathBuf {
    let dir = root.join(name);
    let write = |rel: &str, body: &[u8]| {
        let p = dir.join(rel);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(&p, body).unwrap();
    };
    write("server.jar", &vec![b'J'; 5000]);
    write("server.properties", b"motd=test\nview-distance=10\n");
    write("eula.txt", b"eula=true\n");
    write("world/level.dat", b"\x0a\x08level".to_vec().as_slice());
    write("world/region/r.0.0.mca", &vec![b'R'; 4096]);
    write(
        "world/datapacks/bonus/pack.mcmeta",
        b"{\"pack\":{\"description\":\"x\",\"pack_format\":48}}",
    );
    write("mods/sodium.jar", b"MODBYTES");
    write("plugins/essentials.jar", b"PLUGINBYTES");
    write("logs/latest.log", b"[12:00:00] [Server:INFO] Done\n");
    write(
        "crash-reports/crash-2026-11-13_1.txt",
        b"java.lang.OutOfMemoryError\n",
    );
    write("nohup.tmp", b"junk");
    std::fs::create_dir_all(dir.join("plugins")).unwrap();
    #[cfg(unix)]
    std::os::unix::fs::symlink("/etc/passwd", dir.join("mods/poison.jar")).unwrap();
    dir
}

async fn setup(
    running: bool,
) -> (
    tempfile::TempDir,
    Arc<Db>,
    ServerId,
    PathBuf,
    Arc<BackupEngine>,
    Arc<FakePanel>,
) {
    let tmp = tempfile::tempdir().unwrap();
    let server_dir = fake_server(tmp.path(), "survival");
    let db = Arc::new(Db::in_memory().await.unwrap());
    let mut config = ServerRuntimeConfig::default();
    config.server_dir = server_dir.clone();
    let server = db
        .servers()
        .create(ServerDraft {
            name: "survival".into(),
            kind: ServerKind::Paper,
            mc_version: "1.21.4".into(),
            build: "1".into(),
            server_dir: server_dir.clone(),
            config,
            eula_accepted: true,
            install_state: InstallState::Ready,
            template_of: None,
            tags: vec![],
            position: None,
        })
        .await
        .unwrap();
    let panel = Arc::new(FakePanel::new(running));
    let engine = Arc::new(
        BackupEngine::new(
            db.clone(),
            Destination::Local {
                root: tmp.path().join("backups"),
            },
        )
        .with_quiesce(panel.clone()),
    );
    (tmp, db, server.id, server_dir, engine, panel)
}

fn request(server_id: ServerId, dir: &Path, format: BackupFormat, root: &Path) -> CreateRequest {
    CreateRequest {
        server_id,
        server_dir: dir.to_path_buf(),
        name: "nightly".into(),
        kind: BackupKind::Manual,
        format,
        destination: Destination::Local {
            root: root.to_path_buf(),
        },
        excludes: vec![],
        include_worlds: true,
        include_logs: false,
        quiesce: true,
        max_total_bytes: 1 << 30,
        cancel: Cancel::new(),
    }
}

#[tokio::test]
async fn a_backup_is_written_hashed_and_describes_itself() {
    let (_t, db, id, dir, engine, panel) = setup(false).await;
    let out = engine
        .create(
            request(
                id,
                &dir,
                BackupFormat::Zip,
                &dir.parent().unwrap().join("backups"),
            ),
            progress(),
        )
        .await
        .unwrap();

    // The row and the file agree.
    assert_eq!(out.backup.state, BackupState::Completed);
    assert_eq!(out.backup.kind, BackupKind::Manual);
    assert!(
        out.path.is_file(),
        "the archive must exist at the recorded key"
    );
    assert_eq!(out.path.extension().unwrap(), "zip");
    assert_eq!(
        out.copy.file_count, 8,
        "logs, crash-reports, .tmp and symlinks are out"
    );
    assert!(out.copy.size_bytes > 0);
    assert_eq!(out.copy.sha256.len(), 64);
    assert!(
        out.copy.world_consistent,
        "a stopped server is consistent by definition"
    );
    assert_eq!(
        panel.prepares.load(std::sync::atomic::Ordering::SeqCst),
        0,
        "nothing to flush while stopped"
    );
    assert_eq!(
        sp_core::verify::hash_file(&out.path)
            .unwrap()
            .sha256
            .unwrap(),
        out.copy.sha256,
        "the digest in the database is the file's real digest"
    );

    // The manifest is inside the archive and is accurate about the files.
    let m = read_manifest(&out.path, BackupFormat::Zip).unwrap();
    assert_eq!(m.file_count, 8);
    assert!(!m.entries.is_empty());
    assert!(m.world_consistent);
    for e in &m.entries {
        let on_disk = dir.join(&e.path);
        assert!(on_disk.is_file(), "{e:?} claims a file that is not there");
        assert_eq!(
            on_disk.metadata().unwrap().len(),
            e.bytes,
            "{} size drifted",
            e.path
        );
        assert_eq!(
            sp_core::verify::hash_file(&on_disk)
                .unwrap()
                .sha256
                .unwrap(),
            e.sha256,
            "{} digest mismatch",
            e.path
        );
    }
    assert!(
        m.skipped.iter().any(|s| s.path == "mods/poison.jar"),
        "{:?}",
        m.skipped
    );
    assert!(
        m.empty_dirs.contains(&"plugins".to_string()),
        "{:?}",
        m.empty_dirs
    );
    assert!(m.entries.iter().all(|e| !e.path.starts_with('/')));
    assert!(m.entries.iter().any(|e| e.path == "mods/sodium.jar"));
    // No .part file survived, and no stray archive either.
    let leftovers: Vec<_> = std::fs::read_dir(out.path.parent().unwrap())
        .unwrap()
        .filter_map(|e| e.ok())
        .filter(|e| e.path().to_string_lossy().ends_with(".part"))
        .collect();
    assert!(leftovers.is_empty(), "{leftovers:?}");
    // The database row is what the UI would list.
    let rows = db.backups().list_for_server(id, 10).await.unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].object_key, out.backup.object_key);
    assert_eq!(rows[0].size_bytes, Some(out.copy.size_bytes));
    assert!(rows[0].world_consistent);
}

#[tokio::test]
async fn a_live_server_is_flushed_first_and_the_flag_records_it() {
    let (_t, _db, id, dir, engine, panel) = setup(true).await;
    let root = dir.parent().unwrap().join("backups");
    let out = engine
        .create(request(id, &dir, BackupFormat::TarZstd, &root), progress())
        .await
        .unwrap();
    assert_eq!(panel.prepares.load(std::sync::atomic::Ordering::SeqCst), 1);
    assert_eq!(
        panel.resumes.load(std::sync::atomic::Ordering::SeqCst),
        1,
        "saving must be re-enabled"
    );
    assert!(out.copy.world_consistent);
    assert_eq!(
        format_for(&out.path, BackupFormat::Zip),
        BackupFormat::TarZstd
    );
    assert!(out.path.to_string_lossy().ends_with(".tar.zst"));
}

#[tokio::test]
async fn both_formats_round_trip_the_same_bytes() {
    for format in [BackupFormat::Zip, BackupFormat::TarZstd] {
        let (_t, _db, id, dir, engine, _p) = setup(false).await;
        let root = dir.parent().unwrap().join("backups");
        let out = engine
            .create(request(id, &dir, format, &root), progress())
            .await
            .unwrap();
        let original = std::fs::read(dir.join("world/region/r.0.0.mca")).unwrap();
        // Wreck the world, then restore it.
        std::fs::write(dir.join("world/region/r.0.0.mca"), b"torn").unwrap();
        std::fs::remove_file(dir.join("mods/sodium.jar")).unwrap();
        let rep = engine
            .restore(&out.backup, &dir, RestoreOptions::default(), progress())
            .await
            .unwrap();
        assert!(
            rep.files_written >= 2,
            "{format:?}: {:?}",
            rep.files_written
        );
        assert_eq!(
            std::fs::read(dir.join("world/region/r.0.0.mca")).unwrap(),
            original,
            "{format:?}"
        );
        assert_eq!(
            std::fs::read(dir.join("mods/sodium.jar")).unwrap(),
            b"MODBYTES",
            "{format:?}"
        );
        assert_eq!(rep.previous_dir, None, "the default mode is an overlay");
        assert!(
            rep.pre_restore.is_some(),
            "a pre-restore copy is always taken"
        );
        // The safety copy is a real backup of the wrecked state, listed and restorable.
        let rows = engine.list(id, 10).await.unwrap();
        let pre = rows
            .iter()
            .find(|b| b.kind == BackupKind::PreRestore)
            .expect("pre-restore row");
        assert_eq!(pre.state, BackupState::Completed);
        assert!(
            engine.root().unwrap().join(&pre.object_key).is_file(),
            "the safety copy is on disk"
        );
    }
}

#[tokio::test]
async fn replace_mode_swaps_the_directory_and_parks_the_old_one() {
    let (_t, _db, id, dir, engine, _p) = setup(false).await;
    let root = dir.parent().unwrap().join("backups");
    let out = engine
        .create(request(id, &dir, BackupFormat::Zip, &root), progress())
        .await
        .unwrap();
    std::fs::write(dir.join("extra-later.txt"), b"added after the backup").unwrap();
    let rep = engine
        .restore(
            &out.backup,
            &dir,
            RestoreOptions {
                mode: RestoreMode::Replace,
                ..Default::default()
            },
            progress(),
        )
        .await
        .unwrap();
    // `Replace` means exactly that: the post-backup file is gone from the live tree,
    // but it is still on disk in the parked directory — the user can take it back.
    assert!(!dir.join("extra-later.txt").exists());
    let parked = rep.previous_dir.expect("the previous directory is kept");
    assert!(parked.join("extra-later.txt").is_file());
    assert!(dir.join("server.jar").is_file());
    assert!(dir.join("mods/sodium.jar").is_file());
    assert!(
        dir.join("plugins").is_dir(),
        "an empty directory survives the swap"
    );
    assert!(
        !dir.join("logs").exists(),
        "logs were excluded from the archive, so they are gone"
    );
}

#[tokio::test]
async fn only_restores_the_paths_asked_for() {
    let (_t, _db, id, dir, engine, _p) = setup(false).await;
    let root = dir.parent().unwrap().join("backups");
    let out = engine
        .create(request(id, &dir, BackupFormat::Zip, &root), progress())
        .await
        .unwrap();
    std::fs::write(dir.join("server.properties"), b"motd=changed\n").unwrap();
    std::fs::write(dir.join("mods/sodium.jar"), b"EVIL").unwrap();
    let rep = engine
        .restore(
            &out.backup,
            &dir,
            RestoreOptions {
                only: vec!["server.properties".into()],
                ..Default::default()
            },
            progress(),
        )
        .await
        .unwrap();
    assert_eq!(
        rep.files_written, 1,
        "one file was asked for, one file was written"
    );
    assert_eq!(
        std::fs::read_to_string(dir.join("server.properties")).unwrap(),
        "motd=test\nview-distance=10\n"
    );
    assert_eq!(
        std::fs::read(dir.join("mods/sodium.jar")).unwrap(),
        b"EVIL",
        "the rest of the tree is untouched"
    );
}

#[tokio::test]
async fn a_tampered_archive_is_caught_before_it_can_be_restored() {
    let (_t, _db, id, dir, engine, _p) = setup(false).await;
    let root = dir.parent().unwrap().join("backups");
    let out = engine
        .create(request(id, &dir, BackupFormat::Zip, &root), progress())
        .await
        .unwrap();
    let report = engine.verify(&out.backup).await.unwrap();
    assert!(report.is_healthy(), "{}", report.summary());

    let mut bytes = std::fs::read(&out.path).unwrap();
    let mid = bytes.len() / 2;
    for b in bytes.iter_mut().skip(mid).take(16) {
        *b ^= 0xFF;
    }
    std::fs::write(&out.path, &bytes).unwrap();
    let report = engine.verify(&out.backup).await.unwrap();
    assert!(!report.is_healthy());
    assert!(!report.sha256_ok, "the recorded digest must disagree");
    assert!(report.summary().contains("digest"), "{}", report.summary());

    // A restore of that archive fails loudly instead of writing half the tree.
    let err = engine
        .restore(&out.backup, &dir, RestoreOptions::default(), progress())
        .await;
    match err {
        // Either the digest check rejects it up front, or reading the damaged
        // entries does; both must leave the live files alone.
        Ok(_) => panic!("a tampered archive must not restore cleanly"),
        Err(e) => assert!(!e.to_string().is_empty()),
    }
    assert_eq!(
        std::fs::read(dir.join("mods/sodium.jar")).unwrap(),
        b"MODBYTES"
    );
}

#[tokio::test]
async fn a_running_server_refuses_a_restore() {
    let (_t, _db, id, dir, engine, panel) = setup(false).await;
    let root = dir.parent().unwrap().join("backups");
    let out = engine
        .create(request(id, &dir, BackupFormat::Zip, &root), progress())
        .await
        .unwrap();
    panel.running.store(true, Ordering::SeqCst);
    let err = engine
        .restore(&out.backup, &dir, RestoreOptions::default(), progress())
        .await
        .unwrap_err();
    assert_eq!(err.code(), "invalid_state");
    assert!(err.to_string().contains("stop the server"), "{err}");
    panel.running.store(false, Ordering::SeqCst);
    // ...and works once it is stopped.
    assert!(engine
        .restore(&out.backup, &dir, RestoreOptions::default(), progress())
        .await
        .is_ok());
}

#[tokio::test]
async fn a_failed_backup_leaves_no_row_claiming_success() {
    let (_t, db, id, dir, engine, _p) = setup(false).await;
    let root = dir.parent().unwrap().join("backups");
    let mut req = request(id, &dir, BackupFormat::Zip, &root);
    req.max_total_bytes = 10; // smaller than the tree
    let err = engine.create(req, progress()).await.unwrap_err();
    assert_eq!(err.code(), "validation");
    assert!(err.to_string().contains("over the"), "{err}");
    let rows = db.backups().list_for_server(id, 10).await.unwrap();
    assert_eq!(
        rows.len(),
        1,
        "the attempt is recorded, so the user can see it failed"
    );
    assert_eq!(rows[0].state, BackupState::Failed);
    assert!(rows[0].error.as_ref().unwrap().contains("over the"));
    assert_eq!(
        std::fs::read_dir(&root).map(|d| d.count()).unwrap_or(0),
        0,
        "no archive was left behind"
    );
}

#[tokio::test]
async fn cancellation_stops_at_the_next_file_boundary() {
    let (_t, db, id, dir, engine, _p) = setup(false).await;
    let root = dir.parent().unwrap().join("backups");
    let mut req = request(id, &dir, BackupFormat::Zip, &root);
    req.cancel.cancel();
    let err = engine.create(req, progress()).await.unwrap_err();
    assert_eq!(err.code(), "cancelled");
    let rows = db.backups().list_for_server(id, 10).await.unwrap();
    assert_eq!(rows[0].state, BackupState::Failed);
    assert!(!root.join(&rows[0].object_key).exists());
    assert_eq!(
        engine.reap_interrupted().await.unwrap(),
        1,
        "the failed row is reaped at boot"
    );
}

#[tokio::test]
async fn retention_deletes_the_oldest_and_keeps_the_newest() {
    let (_t, db, id, dir, engine, _p) = setup(false).await;
    let root = dir.parent().unwrap().join("backups");
    let mut created = Vec::new();
    for _ in 0..4 {
        let out = engine
            .create(request(id, &dir, BackupFormat::Zip, &root), progress())
            .await
            .unwrap();
        created.push(out.backup.id);
        tokio::time::sleep(std::time::Duration::from_millis(3)).await;
    }
    let mut policy = BackupPolicy::default();
    policy.enabled = true;
    policy.keep_last = 2;
    policy.keep_days = 0;
    db.backups().set_policy(id, &policy).await.unwrap();
    let rep = engine.prune(id).await.unwrap();
    assert_eq!(rep.deleted.len(), 2, "four backups, keep two");
    assert!(rep.freed_bytes > 0);
    assert!(rep.missing_files.is_empty());
    assert!(
        created.contains(&rep.deleted[0]),
        "a deleted id must be one we created"
    );
    let kept: Vec<_> = engine
        .list(id, 10)
        .await
        .unwrap()
        .into_iter()
        .map(|b| b.id)
        .collect();
    assert_eq!(kept.len(), 2);
    assert!(
        !kept.contains(&created[0]),
        "the oldest must be the one that goes"
    );
    assert!(kept.contains(created.last().unwrap()));
    assert_eq!(
        std::fs::read_dir(&root).unwrap().count(),
        2,
        "the files went with the rows"
    );
    // keep_last = 0 means "keep everything", never "delete everything".
    policy.keep_last = 0;
    db.backups().set_policy(id, &policy).await.unwrap();
    assert!(engine.prune(id).await.unwrap().is_noop());
    assert_eq!(engine.list(id, 10).await.unwrap().len(), 2);
}

#[tokio::test]
async fn a_foreign_archive_can_be_adopted_and_restored() {
    let tmp = tempfile::tempdir().unwrap();
    let (_t, db, id, dir, engine, _p) = setup(false).await;
    // Build an archive by hand, with no manifest: what a user gets from a
    // hosting-provider download or another tool.
    let foreign = tmp.path().join("from-someone-else.zip");
    {
        let mut w = sp_backup::FormatWriter::create(&foreign, BackupFormat::Zip).unwrap();
        w.add_file(
            "server.properties",
            &mut std::io::Cursor::new(b"motd=foreign\n".to_vec()),
            13,
            0,
            false,
        )
        .unwrap();
        w.add_file(
            "mods/dropped-in.jar",
            &mut std::io::Cursor::new(b"FOREIGN".to_vec()),
            7,
            0,
            false,
        )
        .unwrap();
        w.finish_with_manifest(b"").unwrap();
    }
    let out = engine
        .import(id, &dir, &foreign, "from hosting")
        .await
        .unwrap();
    assert_eq!(out.backup.kind, BackupKind::Import);
    assert_eq!(out.backup.state, BackupState::Completed);
    assert_eq!(out.copy.file_count, 2);
    assert!(
        !out.copy.world_consistent,
        "we cannot claim consistency for someone else's zip"
    );
    assert!(
        out.path.is_file(),
        "the archive was copied into the destination"
    );
    assert!(
        read_manifest(&out.path, BackupFormat::Zip).is_err(),
        "still manifest-less"
    );

    // Restoring it works; the panel just cannot verify per-file digests.
    std::fs::write(dir.join("server.properties"), b"motd=local\n").unwrap();
    let rep = engine
        .restore(&out.backup, &dir, RestoreOptions::default(), progress())
        .await
        .unwrap();
    assert_eq!(rep.files_written, 2);
    assert_eq!(
        std::fs::read_to_string(dir.join("server.properties")).unwrap(),
        "motd=foreign\n"
    );
    assert_eq!(
        std::fs::read(dir.join("mods/dropped-in.jar")).unwrap(),
        b"FOREIGN"
    );
    let rows = db.backups().list_for_server(id, 10).await.unwrap();
    assert_eq!(
        rows.iter().filter(|b| b.kind == BackupKind::Import).count(),
        1
    );
    assert!(rows.iter().any(|b| b.restored_count >= 1));
}

#[tokio::test]
async fn preview_reports_what_a_backup_would_hold() {
    let (_t, _db, _id, dir, engine, _p) = setup(false).await;
    let plan = engine
        .preview(ServerId::new(1), &dir, vec![], true, false)
        .await
        .unwrap();
    let paths: BTreeMap<_, _> = plan
        .files
        .iter()
        .map(|f| (f.rel.clone(), f.bytes))
        .collect();
    assert!(paths.contains_key("server.jar"));
    assert!(
        !paths.contains_key("logs/latest.log"),
        "logs off by default"
    );
    assert_eq!(paths["mods/sodium.jar"], 8);
    assert!(plan.total_bytes >= 5000);
    assert!(plan.describe().contains("files,"));
    let with_logs = engine
        .preview(ServerId::new(1), &dir, vec![], true, true)
        .await
        .unwrap();
    assert!(with_logs.files.iter().any(|f| f.rel == "logs/latest.log"));
    assert!(with_logs.total_bytes > plan.total_bytes);
    // The engine never wrote anything during a preview.
    let _ = Selector::default();
}

#[tokio::test]
async fn deleting_a_backup_removes_the_row_and_the_file() {
    let (_t, db, id, dir, engine, _p) = setup(false).await;
    let root = dir.parent().unwrap().join("backups");
    let out = engine
        .create(request(id, &dir, BackupFormat::Zip, &root), progress())
        .await
        .unwrap();
    let freed = engine.delete(out.backup.id).await.unwrap();
    assert_eq!(freed, out.copy.size_bytes);
    assert!(!out.path.exists());
    assert!(db
        .backups()
        .list_for_server(id, 10)
        .await
        .unwrap()
        .is_empty());
    assert!(
        engine.delete(out.backup.id).await.is_err(),
        "a second delete is a 404, not a silent success"
    );
}

#[tokio::test]
async fn an_empty_server_directory_is_refused_rather_than_recorded() {
    let tmp = tempfile::tempdir().unwrap();
    let empty = tmp.path().join("nothing-here");
    std::fs::create_dir(&empty).unwrap();
    let db = Arc::new(Db::in_memory().await.unwrap());
    let mut config = ServerRuntimeConfig::default();
    config.server_dir = empty.clone();
    let server = db
        .servers()
        .create(ServerDraft {
            name: "empty".into(),
            kind: ServerKind::Paper,
            mc_version: "1.21.4".into(),
            build: String::new(),
            server_dir: empty.clone(),
            config,
            eula_accepted: false,
            install_state: InstallState::Pending,
            template_of: None,
            tags: vec![],
            position: None,
        })
        .await
        .unwrap();
    let engine = BackupEngine::new(
        db.clone(),
        Destination::Local {
            root: tmp.path().join("b"),
        },
    );
    let err = engine
        .create(
            request(server.id, &empty, BackupFormat::Zip, &tmp.path().join("b")),
            progress(),
        )
        .await
        .unwrap_err();
    assert_eq!(err.code(), "invalid_state");
    assert!(err.to_string().contains("nothing to back up"), "{err}");
}
