//! One running (or stopped) Minecraft server: pipes, lifecycle, live state.
//!
//! An [`Instance`] owns exactly one OS process and is `Send + Sync`: the API layer
//! hands out `Arc<Instance>` freely, tasks pump its pipes, and
//! [`crate::manager::ProcessManager`] supervises restarts. Nothing here touches the
//! database — status changes go out over the event bus, so the process layer stays
//! testable with a fake server program (`tests/fake_server.rs`).

use crate::job_object::ProcessGroupGuard;
use crate::ring::{strip_ansi, LogRing};
use crate::tick;
use sp_core::event::EventBus;
use sp_core::ids::ServerId;
use sp_core::model::*;
use sp_core::CoreError;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering::Relaxed};
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, Command};
use tokio::sync::{mpsc, oneshot, watch, Mutex, Notify};

/// How to launch: a resolved command line with no shell involved.
///
/// Building argv as a `Vec<String>` (rather than a command *string*) is a security
/// property: there is no parser for a hostile server name or flag to escape from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LaunchPlan {
    pub program: PathBuf,
    pub args: Vec<String>,
    pub env: std::collections::BTreeMap<String, String>,
    pub cwd: PathBuf,
}

impl LaunchPlan {
    /// Compose `java <flags> -jar server.jar [args]` from a server's config.
    pub fn build(
        server_id: ServerId,
        kind: ServerKind,
        cfg: &ServerRuntimeConfig,
        java: &Path,
    ) -> Self {
        let mut args: Vec<String> = Vec::with_capacity(cfg.jvm_flags.len() + 8);
        args.extend(sp_core::flags::heap_flags(cfg.memory_mb, cfg.tune_heap));
        if cfg.prefer_ipv4 {
            args.push("-Djava.net.preferIPv4Stack=true".to_string());
        }
        for flag in &cfg.jvm_flags {
            // De-duplicate: the preset may already contain the IPv4 property, and
            // a repeated `-XX:` flag would make some JDKs refuse to start.
            if !args.contains(flag) {
                args.push(flag.clone());
            }
        }
        if let Some(jmx) = cfg.jmx_port {
            args.push(format!("-Dcom.sun.management.jmxremote.port={jmx}"));
            args.push("-Dcom.sun.management.jmxremote.authenticate=false".to_string());
            args.push("-Dcom.sun.management.jmxremote.ssl=false".to_string());
            // JMX without auth is only ever bound to loopback; the settings tooltip
            // says so explicitly.
            args.push("-Dcom.sun.management.jmxremote.local.only=true".to_string());
        }
        // Where the code comes from is the provider's decision, recorded per server
        // (`ServerRuntimeConfig::launch`), so a Forge argfile install and a vanilla
        // `-jar` install both survive a settings edit.
        match &cfg.launch {
            LaunchMode::Jar { jar, .. } => {
                args.push("-jar".to_string());
                args.push(jar.clone());
            }
            LaunchMode::Classpath {
                entries,
                main_class,
                ..
            } => {
                args.push("-cp".to_string());
                args.push(entries.join(LaunchMode::classpath_separator()));
                args.push(main_class.clone());
            }
            LaunchMode::ArgFile { argfile, .. } => {
                // The JVM expands `@file` itself; no shell is involved, which is
                // why we can allow it at all.
                args.push(format!("@{argfile}"));
            }
        }
        // Vanilla and the Forge-family installers open a GUI window unless told
        // otherwise; proxies have no such flag.
        if !kind.is_proxy()
            && matches!(
                kind,
                ServerKind::Vanilla | ServerKind::Forge | ServerKind::NeoForge
            )
            && !args.iter().any(|a| a == "--nogui" || a == "nogui")
        {
            args.push("--nogui".to_string());
        }
        if let Some(world) = &cfg.world_override {
            // There is no `-World` JVM switch; `level-name` in server.properties is
            // the real knob, so we express the override as a system property that
            // the config writer also applies.
            args.push(format!("-Dlevel-name={world}"));
        }
        args.extend(cfg.launch.program_args().to_vec());
        for extra in &cfg.program_args {
            args.push(extra.clone());
        }

        let mut env = cfg.env.clone();
        env.insert(
            "SERVERPANEL_SERVER_ID".to_string(),
            server_id.get().to_string(),
        );

        Self {
            program: java.to_path_buf(),
            args,
            env,
            cwd: cfg.server_dir.clone(),
        }
    }

    /// Command line for the UI and the log (env vars are deliberately excluded).
    pub fn display(&self) -> String {
        let mut out = self.program.to_string_lossy().to_string();
        for a in &self.args {
            out.push(' ');
            out.push_str(a);
        }
        out
    }
}

/// Jar name each server type expects. Modded installers write versioned jars; the
/// providers rename them to these at install time so launching stays uniform.
pub fn server_jar_name(kind: ServerKind) -> &'static str {
    kind.jar_name()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExitReason {
    /// The panel asked for a stop and the server exited cleanly.
    IntentionalStop,
    /// Stopped on request but with a non-zero code.
    StoppedWithError,
    /// We SIGKILLed it (or closed the Windows job object).
    Killed,
    /// Gone without us asking.
    Crashed,
    /// `exec` failed: bad java path, missing jar, unreadable cwd.
    SpawnFailed,
}

#[derive(Debug, Clone)]
pub struct ExitInfo {
    pub code: Option<i32>,
    pub signal: Option<i32>,
    pub reason: ExitReason,
    pub ran_for: Duration,
    pub exit_ms: i64,
}

impl ExitInfo {
    pub fn crashed(&self) -> bool {
        matches!(self.reason, ExitReason::Crashed | ExitReason::SpawnFailed)
            || (self.reason == ExitReason::StoppedWithError && self.code.unwrap_or(0) != 0)
    }

    pub fn describe(&self) -> String {
        match self.reason {
            ExitReason::IntentionalStop => "stopped".into(),
            ExitReason::StoppedWithError => {
                format!("stopped with exit code {}", self.code.unwrap_or(-1))
            }
            ExitReason::Killed => "killed".into(),
            ExitReason::Crashed => match (self.code, self.signal) {
                (Some(c), _) if c != 0 => format!("crashed (exit code {c})"),
                (_, Some(s)) => format!("terminated by signal {s}"),
                _ => "crashed".into(),
            },
            ExitReason::SpawnFailed => "failed to start".into(),
        }
    }
}

#[derive(Debug)]
struct LiveState {
    pid: Option<u32>,
    started_at_ms: Option<i64>,
    exit: Option<ExitInfo>,
    last_error: Option<String>,
    last_crash_report: Option<PathBuf>,
    stats: ResourceStats,
}

#[derive(Debug)]
struct Inner {
    server_id: ServerId,
    ring: LogRing,
    /// Current status, readable from sync code (`watch::Sender` cannot be borrowed,
    /// and the API polls this on every request that lists servers).
    status_cell: std::sync::Mutex<ServerStatus>,
    status: watch::Sender<ServerStatus>,
    state: Mutex<LiveState>,
    child: Mutex<Option<Child>>,
    stdin_tx: Mutex<Option<mpsc::UnboundedSender<String>>>,
    /// Holds the Windows job object open for as long as the child should live.
    group: Mutex<Option<ProcessGroupGuard>>,
    plan: Mutex<Arc<LaunchPlan>>,
    intentional: AtomicBool,
    save_disabled: AtomicBool,
    consecutive_crashes: AtomicU32,
    restart_at_ms: Mutex<Option<i64>>,
    pending_start: AtomicBool,
    exit_tx: Mutex<Option<oneshot::Sender<ExitInfo>>>,
    boot_done: Notify,
    save_done: Notify,
    online: AtomicU32,
    max_players: AtomicU32,
}

#[derive(Debug, Clone)]
pub struct Instance {
    inner: Arc<Inner>,
    bus: Arc<EventBus>,
    boot_timeout: Duration,
}

impl Instance {
    /// Spawn the process and start pumping its pipes.
    ///
    /// The instance starts in [`ServerStatus::Starting`] and moves to `Running`
    /// when the boot-complete line appears in the log, or after `boot_timeout`
    /// (so a mod pack with a custom banner is not stuck showing "starting").
    pub async fn start(
        server_id: ServerId,
        plan: LaunchPlan,
        ring_capacity: usize,
        bus: Arc<EventBus>,
        boot_timeout: Duration,
    ) -> Result<Self, CoreError> {
        let inner = Arc::new(Inner {
            server_id,
            ring: LogRing::new(ring_capacity),
            status_cell: std::sync::Mutex::new(ServerStatus::Starting),
            status: watch::channel(ServerStatus::Starting).0,
            state: Mutex::new(LiveState {
                pid: None,
                started_at_ms: Some(now_ms()),
                exit: None,
                last_error: None,
                last_crash_report: None,
                stats: ResourceStats::default(),
            }),
            child: Mutex::new(None),
            stdin_tx: Mutex::new(None),
            group: Mutex::new(None),
            plan: Mutex::new(Arc::new(plan)),
            intentional: AtomicBool::new(false),
            save_disabled: AtomicBool::new(false),
            consecutive_crashes: AtomicU32::new(0),
            restart_at_ms: Mutex::new(None),
            pending_start: AtomicBool::new(false),
            exit_tx: Mutex::new(None),
            boot_done: Notify::new(),
            save_done: Notify::new(),
            online: AtomicU32::new(0),
            max_players: AtomicU32::new(20),
        });
        let this = Self {
            inner,
            bus,
            boot_timeout: boot_timeout.max(Duration::from_secs(5)),
        };
        this.spawn().await?;
        Ok(this)
    }

    /// The currently running instance's cwd + jar (used by backup/restore and the
    /// file manager to resolve paths for a live server).
    pub async fn plan(&self) -> Arc<LaunchPlan> {
        self.inner.plan.lock().await.clone()
    }

    pub async fn replace_plan(&self, plan: LaunchPlan) {
        *self.inner.plan.lock().await = Arc::new(plan);
    }

    async fn spawn(&self) -> Result<(), CoreError> {
        let plan = self.plan().await;
        std::fs::create_dir_all(&plan.cwd).map_err(|e| {
            CoreError::Io(std::io::Error::new(
                e.kind(),
                format!("could not create {}: {e}", plan.cwd.display()),
            ))
        })?;
        if !plan.program.exists() && !which_ok(&plan.program) {
            let msg = format!("java executable not found at {}", plan.program.display());
            self.set_status(ServerStatus::Crashed, Some(&msg)).await;
            self.inner.state.lock().await.last_error = Some(msg.clone());
            return Err(CoreError::Process(msg));
        }

        let mut cmd = Command::new(&plan.program);
        cmd.args(&plan.args)
            .current_dir(&plan.cwd)
            .envs(plan.env.iter())
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            // The panel owns the lifetime: dropping our handle must never kill a
            // JVM mid-world-write. `ProcessManager::shutdown_all` is the
            // deliberate path.
            .kill_on_drop(false);
        // Unix: its own process group, so `killpg` reaches the server *and*
        // anything it spawned (BuildTools, wrappers), and nothing of ours.
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt;
            // Safe API since Rust 1.64 — no `unsafe` needed to set a process group.
            cmd.process_group(0);
        }

        let mut child = cmd.spawn().map_err(|e| {
            let msg = format!(
                "could not start {} (cwd {}): {e}",
                plan.program.display(),
                plan.cwd.display()
            );
            tracing::error!(error = %e, "spawn failed");
            CoreError::Process(msg)
        })?;

        let pid = child.id().unwrap_or(0);
        *self.inner.group.lock().await = ProcessGroupGuard::attach(pid);
        let stdin = child.stdin.take();
        if let Some(mut stdin) = stdin {
            let (tx, mut rx) = mpsc::unbounded_channel::<String>();
            *self.inner.stdin_tx.lock().await = Some(tx);
            tokio::spawn(async move {
                while let Some(line) = rx.recv().await {
                    if stdin.write_all(line.as_bytes()).await.is_err() {
                        break;
                    }
                    if stdin.write_all(b"\n").await.is_err() {
                        break;
                    }
                    let _ = stdin.flush().await;
                }
                // Closing stdin on exit is what lets a wedged server finish dying.
                let _ = stdin.shutdown().await;
            });
        }
        let stdout = child.stdout.take();
        let stderr = child.stderr.take();
        *self.inner.child.lock().await = Some(child);

        {
            let mut st = self.inner.state.lock().await;
            st.pid = Some(pid);
            st.started_at_ms = Some(now_ms());
            st.exit = None;
            st.last_error = None;
        }
        self.inner.intentional.store(false, Relaxed);
        self.set_status(ServerStatus::Starting, None).await;

        if let Some(stdout) = stdout {
            self.pump(stdout, LogStream::Stdout);
        }
        if let Some(stderr) = stderr {
            self.pump(stderr, LogStream::Stderr);
        }
        self.supervise(pid);
        Ok(())
    }

    /// Wait for exit, classify it, publish the transition, wake `graceful_stop`.
    fn supervise(&self, pid: u32) {
        let inner = self.inner.clone();
        let bus = self.bus.clone();
        let boot_timeout = self.boot_timeout;
        tokio::spawn(async move {
            let started = now_ms();
            // Boot detection: wait for the log marker, with a ceiling.
            let _ = tokio::time::timeout(boot_timeout, inner.boot_done.notified()).await;
            if inner.status_now() == ServerStatus::Starting {
                let reason = if inner.ring.stats().total == 0 {
                    Some("no output seen yet; treating as online".to_string())
                } else {
                    None
                };
                inner
                    .set_and_publish(ServerStatus::Running, reason.as_deref(), &bus)
                    .await;
            }

            let status = {
                let mut guard = inner.child.lock().await;
                match guard.as_mut() {
                    Some(child) => child.wait().await.ok(),
                    None => None,
                }
            };
            // Let the pumps drain whatever is still in the pipes before we declare
            // the console closed.
            tokio::time::sleep(Duration::from_millis(200)).await;

            let (code, signal) = match &status {
                Some(s) => (s.code(), signal_of(s)),
                None => (None, None),
            };
            let intentional = inner.intentional.load(Relaxed);
            let ran_for = Duration::from_millis((now_ms() - started).max(0) as u64);
            let reason = if intentional {
                if code.unwrap_or(0) == 0 {
                    ExitReason::IntentionalStop
                } else {
                    ExitReason::StoppedWithError
                }
            } else if code.unwrap_or(1) == 0 {
                ExitReason::IntentionalStop
            } else {
                ExitReason::Crashed
            };
            let exit = ExitInfo {
                code,
                signal,
                reason,
                ran_for,
                exit_ms: now_ms(),
            };
            tracing::info!(
                server = inner.server_id.get(),
                pid,
                code = ?exit.code,
                signal = ?exit.signal,
                reason = ?exit.reason,
                secs = exit.ran_for.as_secs(),
                "server process exited"
            );

            let crashed = exit.crashed();
            if crashed {
                inner.consecutive_crashes.fetch_add(1, Relaxed);
                inner
                    .set_and_publish(ServerStatus::Crashed, Some(&exit.describe()), &bus)
                    .await;
            } else {
                inner.consecutive_crashes.store(0, Relaxed);
                inner
                    .set_and_publish(ServerStatus::Stopped, None, &bus)
                    .await;
            }
            {
                let mut st = inner.state.lock().await;
                st.exit = Some(exit.clone());
                st.pid = None;
                if crashed {
                    st.last_error = Some(exit.describe());
                    st.last_crash_report = latest_crash_report(&inner).await;
                }
            }
            if let Some(tx) = inner.exit_tx.lock().await.take() {
                let _ = tx.send(exit);
            }
            inner.save_disabled.store(false, Relaxed);
            *inner.group.lock().await = None;
            *inner.stdin_tx.lock().await = None;
            *inner.child.lock().await = None;
        });
    }

    fn pump<R>(&self, reader: R, stream: LogStream)
    where
        R: tokio::io::AsyncRead + Unpin + Send + 'static,
    {
        let inner = self.inner.clone();
        let bus = self.bus.clone();
        tokio::spawn(async move {
            let mut lines = BufReader::new(reader).lines();
            // `lines()` splits on \n and keeps the `\r`, which is what xterm.js
            // needs to render progress-bar rewrites correctly.
            loop {
                match lines.next_line().await {
                    Ok(None) => break,
                    Ok(Some(line)) => {
                        if line.trim().is_empty() {
                            continue;
                        }
                        let plain = strip_ansi(&line);
                        if plain.trim().is_empty() {
                            continue;
                        }
                        let parsed = tick::parse(&plain);
                        if let Some((online, max)) = parsed.players {
                            inner.online.store(online, Relaxed);
                            inner.max_players.store(max, Relaxed);
                        }
                        match parsed.event {
                            Some(tick::ConsoleEvent::PlayerJoin) => {
                                inner.online.fetch_add(1, Relaxed);
                            }
                            Some(tick::ConsoleEvent::PlayerLeave) => {
                                let prev = inner.online.load(Relaxed);
                                inner.online.store(prev.saturating_sub(1), Relaxed);
                            }
                            Some(tick::ConsoleEvent::BootDone) => inner.boot_done.notify_waiters(),
                            Some(tick::ConsoleEvent::SaveDone) => inner.save_done.notify_waiters(),
                            Some(tick::ConsoleEvent::Crash) => {
                                inner
                                    .set_and_publish(
                                        ServerStatus::Crashed,
                                        Some("crash report detected"),
                                        &bus,
                                    )
                                    .await;
                            }
                            _ => {}
                        }
                        {
                            let mut st = inner.state.lock().await;
                            if let Some(tps) = parsed.tps {
                                st.stats.tps = Some(tps[0]);
                            }
                            if let Some(mspt) = parsed.mspt {
                                st.stats.mspt = Some(mspt[0]);
                            }
                            st.stats.players_online = inner.online.load(Relaxed);
                            st.stats.players_max = inner.max_players.load(Relaxed);
                            st.stats.sampled_at_ms = now_ms();
                        }
                        let seq = inner.ring.push(stream, line.clone(), plain.clone());
                        bus.publish(PanelEvent::Log(LogLine {
                            seq,
                            at_ms: now_ms(),
                            stream,
                            raw: line,
                            plain,
                        }));
                    }
                    Err(e) => {
                        tracing::debug!(error = %e, "console pipe closed");
                        break;
                    }
                }
            }
        });
    }

    // -- public API ---------------------------------------------------------

    pub fn id(&self) -> ServerId {
        self.inner.server_id
    }

    pub fn ring(&self) -> &LogRing {
        &self.inner.ring
    }

    pub fn status(&self) -> ServerStatus {
        self.inner.status_now()
    }

    /// The crash report from the most recent crash, if the JVM wrote one.
    pub async fn crash_report(&self) -> Option<PathBuf> {
        latest_crash_report(&self.inner).await
    }

    pub fn subscribe_status(&self) -> watch::Receiver<ServerStatus> {
        self.inner.status.subscribe()
    }

    pub async fn set_status(&self, status: ServerStatus, reason: Option<&str>) {
        self.inner.set_and_publish(status, reason, &self.bus).await;
    }

    /// Type a line into the server console. Also echoed into the ring so the
    /// operator sees what the *panel* sent (scheduler commands included).
    pub async fn send_command(&self, cmd: &str) -> Result<(), CoreError> {
        let trimmed = cmd.trim().trim_end_matches(['\n', '\r']);
        if trimmed.is_empty() {
            return Ok(());
        }
        let tx = self.inner.stdin_tx.lock().await.clone();
        match tx {
            Some(tx) => tx
                .send(trimmed.to_string())
                .map_err(|_| CoreError::InvalidState("the server is not accepting input".into()))?,
            None => {
                return Err(CoreError::InvalidState(
                    "the server is not running, so there is no console to type into".into(),
                ))
            }
        }
        let line = format!("> {trimmed}");
        let seq = self
            .inner
            .ring
            .push(LogStream::System, line.clone(), line.clone());
        self.bus.publish(PanelEvent::Log(LogLine {
            seq,
            at_ms: now_ms(),
            stream: LogStream::System,
            raw: line,
            plain: line,
        }));
        Ok(())
    }

    /// `/save-off`, used by live backups.
    pub async fn save_off(&self) {
        self.inner.save_disabled.store(true, Relaxed);
        {
            let mut st = self.inner.state.lock().await;
            st.stats.sampled_at_ms = now_ms();
        }
        let _ = self.send_command("save-off").await;
    }

    pub async fn save_on(&self) {
        self.inner.save_disabled.store(false, Relaxed);
        let _ = self.send_command("save-on").await;
    }

    /// `save-all flush` and wait (bounded) for the "saved" line. `true` means the
    /// flush completed and a copy of the world is consistent.
    pub async fn flush_save(&self, timeout: Duration) -> bool {
        if self.status() != ServerStatus::Running {
            return false;
        }
        // 1.20.5+ accepts `save-all flush`; older builds reject the argument, so
        // send the bare form as a fallback (both are cheap).
        if self.send_command("save-all flush").await.is_err() {
            return false;
        }
        if tokio::time::timeout(timeout / 2, self.inner.save_done.notified())
            .await
            .is_ok()
        {
            return true;
        }
        let _ = self.send_command("save-all").await;
        tokio::time::timeout(timeout, self.inner.save_done.notified())
            .await
            .is_ok()
    }

    /// Ask the server to stop itself; escalates to signals if it will not listen.
    pub async fn graceful_stop(&self, timeout: Duration) -> Result<ExitInfo, CoreError> {
        if !self.status().is_running() && !self.status().is_busy() {
            let st = self.inner.state.lock().await;
            return Ok(st.exit.clone().unwrap_or(ExitInfo {
                code: Some(0),
                signal: None,
                reason: ExitReason::IntentionalStop,
                ran_for: Duration::ZERO,
                exit_ms: now_ms(),
            }));
        }
        self.inner.intentional.store(true, Relaxed);
        self.set_status(ServerStatus::Stopping, None).await;
        if self.send_command("stop").await.is_err() {
            return self.force_kill(Some(Duration::from_secs(10))).await;
        }
        if let Some(exit) = self.inner.wait_for_exit(timeout).await {
            return Ok(exit);
        }
        tracing::warn!(
            server = self.inner.server_id.get(),
            "graceful stop timed out, escalating"
        );
        // SIGTERM the group: one more chance to flush before SIGKILL.
        #[cfg(unix)]
        {
            let pid = self.inner.state.lock().await.pid;
            if let Some(pid) = pid {
                let _ = crate::signals::term_process_group(pid);
            }
        }
        if let Some(exit) = self.inner.wait_for_exit(Duration::from_secs(8)).await {
            return Ok(exit);
        }
        self.force_kill(Some(Duration::from_secs(15))).await
    }

    /// SIGKILL / TerminateProcess, no waiting for cooperation.
    pub async fn force_kill(&self, wait: Option<Duration>) -> Result<ExitInfo, CoreError> {
        self.inner.intentional.store(true, Relaxed);
        let waiter = self
            .inner
            .wait_for_exit(wait.unwrap_or(Duration::from_secs(15)));
        {
            let mut guard = self.inner.child.lock().await;
            if let Some(child) = guard.as_mut() {
                // `start_kill` is the non-async request; `waiter` below collects
                // the actual exit status.
                let _ = child.start_kill();
            }
        }
        #[cfg(unix)]
        {
            let pid = self.inner.state.lock().await.pid;
            if let Some(pid) = pid {
                let _ = crate::signals::kill_process_group(pid);
            }
        }
        self.set_status(ServerStatus::Killed, None).await;
        match waiter.await {
            Some(mut exit) => {
                exit.reason = ExitReason::Killed;
                Ok(exit)
            }
            None => Err(CoreError::Process(format!(
                "server {} did not exit after SIGKILL",
                self.inner.server_id
            ))),
        }
    }

    /// Restart in place with the same plan (config changes go through the
    /// manager, which rebuilds the plan first).
    pub async fn restart(&self, stop_timeout: Duration) -> Result<(), CoreError> {
        self.graceful_stop(stop_timeout).await?;
        self.inner.intentional.store(false, Relaxed);
        self.spawn().await
    }

    pub async fn state(&self) -> ProcessState {
        let st = self.inner.state.lock().await;
        ProcessState {
            pid: st.pid,
            status: self.status(),
            started_at_ms: st.started_at_ms,
            uptime_secs: st
                .started_at_ms
                .map(|t| (now_ms() - t).max(0) as u64 / 1000)
                .unwrap_or(0),
            exit_code: st.exit.as_ref().and_then(|e| e.code),
            signal: st.exit.as_ref().and_then(|e| e.signal),
            consecutive_crashes: self.inner.consecutive_crashes.load(Relaxed),
            restart_scheduled_at_ms: *self.inner.restart_at_ms.lock().await,
            pending_start: self.inner.pending_start.load(Relaxed),
            save_disabled: self.inner.save_disabled.load(Relaxed),
            last_crash_report: st.last_crash_report.clone(),
        }
    }

    pub async fn stats(&self) -> ResourceStats {
        let st = self.inner.state.lock().await;
        let mut stats = st.stats.clone();
        stats.players_online = self.inner.online.load(Relaxed);
        stats.players_max = self.inner.max_players.load(Relaxed);
        stats.java_pid = st.pid;
        stats
    }

    /// Called once per sampling tick by the monitor service. Console-derived
    /// fields (TPS/MSPT, parsed from the log) win over the poller's, so the two
    /// sources never fight each other.
    pub fn set_stats(&self, mut stats: ResourceStats) {
        let inner = &self.inner;
        if stats.players_online == 0 {
            stats.players_online = inner.online.load(Relaxed);
        }
        stats.players_max = inner.max_players.load(Relaxed);
        if let Ok(mut st) = inner.state.try_lock() {
            stats.tps = stats.tps.or(st.stats.tps);
            stats.mspt = stats.mspt.or(st.stats.mspt);
            st.stats = stats;
        }
    }

    pub async fn last_error(&self) -> Option<String> {
        self.inner.state.lock().await.last_error.clone()
    }

    pub async fn last_exit(&self) -> Option<ExitInfo> {
        self.inner.state.lock().await.exit.clone()
    }

    pub async fn note_crash_report(&self, path: PathBuf) {
        self.inner.state.lock().await.last_crash_report = Some(path);
    }

    pub async fn schedule_restart(&self, at_ms: i64) {
        *self.inner.restart_at_ms.lock().await = Some(at_ms);
    }

    pub async fn clear_restart(&self) {
        *self.inner.restart_at_ms.lock().await = None;
    }

    pub fn set_pending_start(&self, pending: bool) {
        self.inner.pending_start.store(pending, Relaxed);
    }

    pub fn consecutive_crashes(&self) -> u32 {
        self.inner.consecutive_crashes.load(Relaxed)
    }

    pub fn reset_crash_counter(&self) {
        self.inner.consecutive_crashes.store(0, Relaxed);
    }

    /// Bytes of output the ring has ever seen — non-zero means the JVM at least
    /// started talking, which distinguishes "bad jar" from "bad world".
    pub fn has_output(&self) -> bool {
        self.inner.ring.stats().total > 0
    }

    pub fn bus(&self) -> &Arc<EventBus> {
        &self.bus
    }
}

impl Inner {
    async fn set_and_publish(
        &self,
        status: ServerStatus,
        reason: Option<&str>,
        bus: &Arc<EventBus>,
    ) {
        let changed = {
            let mut current = self.status_cell_guard();
            if *current == status {
                false
            } else {
                *current = status;
                // No receivers is fine: the panel works with nobody watching SSE.
                let _ = self.status.send(status);
                true
            }
        };
        if changed {
            tracing::debug!(server = self.server_id.get(), %status, "status change");
            bus.publish(PanelEvent::Status {
                server_id: self.server_id,
                status,
                reason: reason.map(str::to_string),
            });
        }
    }

    /// Resolve with exit info, or `None` after `timeout`.
    fn status_now(&self) -> ServerStatus {
        *self.status_cell_guard()
    }

    /// Poison-tolerant: a panic elsewhere must not make every server look dead.
    fn status_cell_guard(&self) -> std::sync::MutexGuard<'_, ServerStatus> {
        self.status_cell
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    async fn wait_for_exit(self: &Arc<Self>, timeout: Duration) -> Option<ExitInfo> {
        // Already finished? Hand back the recorded status without waiting.
        if matches!(
            self.status_now(),
            ServerStatus::Stopped
                | ServerStatus::Crashed
                | ServerStatus::Killed
                | ServerStatus::NotInstalled
        ) {
            return self.state.lock().await.exit.clone();
        }
        let (tx, rx) = oneshot::channel();
        {
            let mut slot = self.exit_tx.lock().await;
            if slot.is_some() {
                // Another stop is in flight; piggyback on the status channel.
                drop(slot);
                return self.wait_status_idle(timeout).await;
            }
            *slot = Some(tx);
        }
        match tokio::time::timeout(timeout, rx).await {
            Ok(Ok(exit)) => Some(exit),
            _ => {
                let _ = self.exit_tx.lock().await.take();
                self.wait_status_idle(Duration::from_millis(50)).await
            }
        }
    }

    async fn wait_status_idle(self: &Arc<Self>, timeout: Duration) -> Option<ExitInfo> {
        let mut rx = self.status.subscribe();
        let deadline = tokio::time::sleep(timeout);
        tokio::pin!(deadline);
        loop {
            let idle = matches!(
                *rx.borrow_and_update(),
                ServerStatus::Stopped
                    | ServerStatus::Crashed
                    | ServerStatus::Killed
                    | ServerStatus::NotInstalled
            );
            if idle {
                return self.state.lock().await.exit.clone();
            }
            tokio::select! {
                changed = rx.changed() => {
                    if changed.is_err() {
                        return self.state.lock().await.exit.clone();
                    }
                }
                _ = &mut deadline => return None,
            }
        }
    }
}

async fn latest_crash_report(inner: &Arc<Inner>) -> Option<PathBuf> {
    let dir = inner.plan.lock().await.cwd.join("crash-reports");
    newest_crash_report(&dir)
}

/// Newest `crash-reports/crash-*.txt` by mtime.
pub fn newest_crash_report(dir: &Path) -> Option<PathBuf> {
    let mut newest: Option<(std::time::SystemTime, PathBuf)> = None;
    let Ok(entries) = std::fs::read_dir(dir) else {
        return None;
    };
    for e in entries.flatten() {
        let name = e.file_name().to_string_lossy().to_string();
        if !name.starts_with("crash-") || !name.ends_with(".txt") {
            continue;
        }
        let Ok(md) = e.metadata() else { continue };
        let Ok(mtime) = md.modified() else { continue };
        if newest.as_ref().map(|(t, _)| mtime > *t).unwrap_or(true) {
            newest = Some((mtime, e.path()));
        }
    }
    newest.map(|(_, p)| p)
}

fn signal_of(status: &std::process::ExitStatus) -> Option<i32> {
    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt;
        status.signal()
    }
    #[cfg(not(unix))]
    {
        let _ = status;
        None
    }
}

/// True for bare program names (`java`), which we resolve via `PATH` ourselves so
/// the "does java exist" check matches what `spawn` would do.
fn which_ok(program: &Path) -> bool {
    if program.parent().is_some() {
        return false; // a path that does not exist is a real error
    }
    let name = program.file_name().map(|s| s.to_string_lossy().to_string());
    let Some(name) = name else { return false };
    std::env::split_paths(&std::env::var("PATH").unwrap_or_default())
        .any(|dir| dir.join(&name).exists() || dir.join(format!("{name}.exe")).exists())
}
