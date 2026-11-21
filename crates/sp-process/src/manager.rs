//! The supervisor: start/stop/restart/kill, crash policy, per-server fairness.
//!
//! One task per running instance watches the status `watch` channel and implements
//! [`RestartPolicy`] with linear backoff. Restart decisions live here (not in
//! `Instance`) because they need the policy, the bus, and a place to record that a
//! restart is pending — which is exactly what the UI shows a countdown for.

use crate::instance::{ExitInfo, Instance, LaunchPlan};
use async_trait::async_trait;
use dashmap::DashMap;
use sp_core::event::EventBus;
use sp_core::ids::ServerId;
use sp_core::model::*;
use sp_core::CoreError;
use std::sync::atomic::Ordering::Relaxed;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{Mutex, Semaphore};

#[derive(Debug, Clone)]
pub struct ManagerPolicy {
    /// Lines kept per server for reconnect replay.
    pub ring_capacity: usize,
    /// How long to wait for the boot marker before declaring `Running`.
    pub boot_timeout: Duration,
    /// Concurrent `java` spawns allowed (spawning is I/O-heavy; 3 is gentle on HDDs).
    pub max_concurrent_starts: usize,
    /// Grace period for `stop` before SIGTERM/SIGKILL.
    pub stop_timeout: Duration,
    /// Give up auto-restarting after this many *consecutive* crashes even when the
    /// policy says `Always` (a safety net so a broken jar cannot hot-loop).
    pub max_unconditional_restarts: u32,
    /// Minimum uptime before a crash counts as "ran, then died" (restarts free).
    pub stable_after: Duration,
}

impl Default for ManagerPolicy {
    fn default() -> Self {
        Self {
            ring_capacity: 4_000,
            boot_timeout: Duration::from_secs(180),
            max_concurrent_starts: 3,
            stop_timeout: Duration::from_secs(60),
            max_unconditional_restarts: 5,
            stable_after: Duration::from_secs(120),
        }
    }
}

/// Notifications the process layer emits upward. Implemented in `sp-server`, where
/// the database and the notifiers live.
#[async_trait]
pub trait ProcessDelegate: Send + Sync + 'static {
    /// Every terminal transition (`Stopped`, `Crashed`, `Killed`) plus whether the
    /// supervisor intends to bring the server back.
    async fn on_exit(&self, server_id: ServerId, exit: ExitInfo, will_restart: bool);
    /// A restart attempt itself failed.
    async fn on_restart_failed(&self, server_id: ServerId, error: String);
}

#[derive(Debug, Default)]
pub struct NullDelegate;

#[async_trait]
impl ProcessDelegate for NullDelegate {
    async fn on_exit(&self, _server_id: ServerId, _exit: ExitInfo, _will_restart: bool) {}
    async fn on_restart_failed(&self, _server_id: ServerId, _error: String) {}
}

#[derive(Debug)]
struct Inner {
    instances: DashMap<ServerId, Arc<Instance>>,
    supervisors: DashMap<ServerId, tokio::task::JoinHandle<()>>,
    starts: Semaphore,
    stopping_all: std::sync::atomic::AtomicBool,
    policy: ManagerPolicy,
    bus: Arc<EventBus>,
    delegate: Arc<dyn ProcessDelegate>,
    shutdown: Mutex<Option<tokio::sync::broadcast::Sender<()>>>,
}

#[derive(Debug, Clone)]
pub struct ProcessManager {
    inner: Arc<Inner>,
}

impl ProcessManager {
    pub fn new(
        bus: Arc<EventBus>,
        policy: ManagerPolicy,
        delegate: Arc<dyn ProcessDelegate>,
    ) -> Self {
        let starts = policy.max_concurrent_starts.max(1);
        Self {
            inner: Arc::new(Inner {
                instances: DashMap::new(),
                supervisors: DashMap::new(),
                starts: Semaphore::new(starts),
                stopping_all: std::sync::atomic::AtomicBool::new(false),
                policy,
                bus,
                delegate,
                shutdown: Mutex::new(None),
            }),
        }
    }

    pub fn policy(&self) -> &ManagerPolicy {
        &self.inner.policy
    }

    pub fn bus(&self) -> &Arc<EventBus> {
        &self.inner.bus
    }

    /// Number of instances currently under supervision (live or mid-shutdown).
    pub fn running_count(&self) -> usize {
        self.inner.instances.len()
    }

    pub fn get(&self, id: ServerId) -> Option<Arc<Instance>> {
        self.inner.instances.get(&id).map(|i| i.clone())
    }

    pub fn list_ids(&self) -> Vec<ServerId> {
        self.inner.instances.iter().map(|e| *e.key()).collect()
    }

    pub fn is_managed(&self, id: ServerId) -> bool {
        self.inner.instances.contains_key(&id)
    }

    /// Launch (or relaunch) a server. Returns the live instance.
    ///
    /// A `start` on an already-running server is an error, not a no-op: the UI
    /// would otherwise look like it did something.
    pub async fn start(
        &self,
        server_id: ServerId,
        plan: LaunchPlan,
        restart: RestartPolicy,
    ) -> Result<Arc<Instance>, CoreError> {
        if let Some(existing) = self.get(server_id) {
            if existing.status().is_running() || existing.status().is_busy() {
                return Err(CoreError::InvalidState(format!(
                    "server {server_id} is already {}",
                    existing.status()
                )));
            }
        }
        // Concurrency budget: the permit is released as soon as the JVM is spawned
        // (not when it finishes booting), because the expensive part is exec + the
        // initial world scan contending for the disk.
        let permit = self
            .inner
            .starts
            .acquire()
            .await
            .map_err(|_| CoreError::Cancelled)?;
        let instance = match Instance::start(
            server_id,
            plan,
            self.inner.policy.ring_capacity,
            self.inner.bus.clone(),
            self.inner.policy.boot_timeout,
        )
        .await
        {
            Ok(i) => i,
            Err(e) => {
                drop(permit);
                return Err(e);
            }
        };
        drop(permit);

        self.inner.instances.insert(server_id, instance.clone());
        self.publish_status(server_id, ServerStatus::Starting, None);
        self.spawn_supervisor(instance.clone(), restart);
        Ok(instance)
    }

    fn spawn_supervisor(&self, instance: Arc<Instance>, restart: RestartPolicy) {
        let id = instance.id();
        let inner = self.inner.clone();
        let handle = tokio::spawn(async move {
            let mut rx = instance.subscribe_status();
            loop {
                if rx.changed().await.is_err() {
                    break; // instance dropped
                }
                let status = *rx.borrow_and_update();
                if status != ServerStatus::Crashed {
                    continue;
                }
                let exit = match instance.last_exit().await {
                    Some(e) => e,
                    None => continue,
                };
                let crashes = instance.consecutive_crashes();
                let stable = exit.ran_for >= inner.policy.stable_after;
                let will_restart = match restart.mode {
                    RestartMode::Never | RestartMode::ScheduledOnly => false,
                    RestartMode::Always => crashes <= inner.policy.max_unconditional_restarts,
                    RestartMode::OnCrash => crashes <= restart.max_retries,
                } && !inner.stopping_all.load(Relaxed);
                inner.delegate.on_exit(id, exit.clone(), will_restart).await;
                if !will_restart {
                    if crashes > 1 && !stable {
                        tracing::warn!(
                            server = id.get(),
                            crashes,
                            "auto-restart disabled: repeated fast crashes"
                        );
                    }
                    continue;
                }
                let delay = restart.delay_for(crashes);
                let at = now_ms() + delay.as_millis() as i64;
                instance.schedule_restart(at).await;
                inner.bus.publish(PanelEvent::Progress(ProgressEvent {
                    job_id: format!("restart:{id}"),
                    server_id: Some(id),
                    phase: ProgressPhase::Install,
                    fraction: Some(0.0),
                    bytes_done: 0,
                    bytes_total: None,
                    rate_bps: None,
                    eta_secs: Some(delay.as_secs_f64()),
                    message: format!(
                        "Crashed ({}). Restarting in {} s (attempt {crashes}).",
                        exit.describe(),
                        delay.as_secs()
                    ),
                    at_ms: now_ms(),
                    finished: false,
                    error: None,
                }));
                tokio::select! {
                    _ = tokio::time::sleep(delay) => {}
                    _ = wait_stop_signal(&inner) => { instance.clear_restart().await; break }
                }
                instance.clear_restart().await;
                if inner.stopping_all.load(Relaxed) {
                    break;
                }
                match instance.restart(inner.policy.stop_timeout).await {
                    Ok(()) => {
                        instance.reset_crash_counter();
                        inner.bus.publish(PanelEvent::Invalidate {
                            key: vec!["servers".into(), id.get().to_string().into()],
                        });
                    }
                    Err(e) => {
                        tracing::error!(server = id.get(), error = %e, "restart failed");
                        inner.delegate.on_restart_failed(id, e.to_string()).await;
                        break;
                    }
                }
            }
        });
        if let Some(old) = self.inner.supervisors.insert(id, handle) {
            old.abort();
        }
    }

    pub async fn stop(&self, server_id: ServerId, force: bool) -> Result<ExitInfo, CoreError> {
        let instance = self.instance_or_err(server_id)?;
        let result = if force {
            instance.force_kill(Some(Duration::from_secs(10))).await
        } else {
            instance.graceful_stop(self.inner.policy.stop_timeout).await
        };
        self.detach_if_idle(server_id).await;
        result
    }

    pub async fn kill(&self, server_id: ServerId) -> Result<ExitInfo, CoreError> {
        let instance = self.instance_or_err(server_id)?;
        let out = instance.force_kill(Some(Duration::from_secs(15))).await;
        self.detach_if_idle(server_id).await;
        out
    }

    pub async fn restart(
        &self,
        server_id: ServerId,
        plan: LaunchPlan,
        restart: RestartPolicy,
    ) -> Result<Arc<Instance>, CoreError> {
        if let Some(instance) = self.get(server_id) {
            if instance.status().is_running() || instance.status().is_busy() {
                instance
                    .graceful_stop(self.inner.policy.stop_timeout)
                    .await?;
            }
            self.detach_if_idle(server_id).await;
        }
        self.start(server_id, plan, restart).await.map(|i| {
            let _ = restart;
            i
        })
    }

    pub async fn send_command(&self, server_id: ServerId, cmd: &str) -> Result<(), CoreError> {
        self.instance_or_err(server_id)?.send_command(cmd).await
    }

    /// Live state for the API. `None` means the server is not running *and* no
    /// instance record exists (panel restarted), which the UI renders as "stopped".
    pub async fn state_of(&self, server_id: ServerId) -> Option<ProcessState> {
        self.get(server_id).map(|i| i.state().await)
    }

    pub async fn stats_of(&self, server_id: ServerId) -> Option<ResourceStats> {
        self.get(server_id).map(|i| i.stats().await)
    }

    /// Apply a config change to a *running* server: remember the new plan so the
    /// next start (or scheduled restart) uses it, without touching the live JVM.
    pub async fn update_plan(
        &self,
        server_id: ServerId,
        plan: LaunchPlan,
    ) -> Result<(), CoreError> {
        let instance = self.instance_or_err(server_id)?;
        instance.replace_plan(plan).await;
        Ok(())
    }

    /// Stop everything, bounded, in reverse start order. Called on shutdown; a
    /// hung server must not block the panel from exiting.
    pub async fn shutdown_all(&self, total: Duration) -> ShutdownReport {
        self.inner.stopping_all.store(true, Relaxed);
        let ids: Vec<ServerId> = self.inner.instances.iter().map(|e| *e.key()).collect();
        let mut report = ShutdownReport {
            stopped: 0,
            killed: 0,
            failed: Vec::new(),
        };
        let deadline = tokio::time::Instant::now() + total;
        for id in ids.iter().rev() {
            let Some(instance) = self.get(*id) else {
                continue;
            };
            if !instance.status().is_running() && !instance.status().is_busy() {
                continue;
            }
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            if remaining.is_zero() {
                let _ = instance.force_kill(Some(Duration::from_secs(2))).await;
                report.killed += 1;
                continue;
            }
            let graceful = remaining / 2;
            match tokio::time::timeout(graceful, instance.graceful_stop(graceful)).await {
                Ok(Ok(_)) => report.stopped += 1,
                Ok(Err(e)) => {
                    // Escalate once, quietly: the user is leaving; a stuck JVM must
                    // not outlive the panel.
                    match instance.force_kill(Some(Duration::from_secs(5))).await {
                        Ok(_) => report.killed += 1,
                        Err(e2) => report.failed.push(format!("{id}: {e2} ({e})")),
                    }
                }
                Err(_) => match instance.force_kill(Some(Duration::from_secs(5))).await {
                    Ok(_) => report.killed += 1,
                    Err(e) => report.failed.push(format!("{id}: {e}")),
                },
            }
        }
        for (_, handle) in self.inner.supervisors.iter_mut() {
            handle.abort();
        }
        self.inner.supervisors.clear();
        report
    }

    pub async fn resume_shutdown_policy(&self) {
        self.inner.stopping_all.store(false, Relaxed);
    }

    async fn detach_if_idle(&self, server_id: ServerId) {
        let Some(instance) = self.get(server_id) else {
            return;
        };
        if instance.status().is_running() || instance.status().is_busy() {
            return;
        }
        if let Some((_, inst)) = self.inner.instances.remove(&server_id) {
            // `inst` drops here; the Child handle it holds is *not* kill-on-drop by
            // design (see instance.rs), so the JVM is unaffected by detaching.
            drop(inst);
        }
        if let Some((_, handle)) = self.inner.supervisors.remove(&server_id) {
            handle.abort();
        }
        self.publish_status(server_id, ServerStatus::Stopped, None);
    }

    fn publish_status(&self, server_id: ServerId, status: ServerStatus, reason: Option<&str>) {
        self.inner.bus.publish(PanelEvent::Status {
            server_id,
            status,
            reason: reason.map(str::to_string),
        });
    }

    fn instance_or_err(&self, server_id: ServerId) -> Result<Arc<Instance>, CoreError> {
        self.get(server_id).ok_or_else(|| {
            CoreError::InvalidState(format!(
                "server {server_id} is not running — start it first"
            ))
        })
    }
}

async fn wait_stop_signal(inner: &Arc<Inner>) {
    let mut rx = match inner.shutdown.lock().await.as_ref() {
        Some(tx) => tx.subscribe(),
        None => {
            // No shutdown channel installed: sleep forever so the caller's select!
            // only resolves on a real signal.
            std::future::pending::<()>().await;
            return;
        }
    };
    let _ = rx.recv().await;
}

#[derive(Debug, Clone, Default)]
pub struct ShutdownReport {
    pub stopped: usize,
    pub killed: usize,
    pub failed: Vec<String>,
}

impl ShutdownReport {
    pub fn is_clean(&self) -> bool {
        self.failed.is_empty() && self.killed == 0
    }
    pub fn summary(&self) -> String {
        if self.is_clean() {
            format!("{} server(s) stopped gracefully", self.stopped)
        } else {
            format!(
                "{} stopped, {} force-killed{}{}",
                self.stopped,
                self.killed,
                if self.failed.is_empty() {
                    String::new()
                } else {
                    format!(", {} failed: {}", self.failed.len(), self.failed.join("; "))
                }
            )
        }
    }
}
