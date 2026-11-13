//! Progress reporting with rate/ETA estimation and coalescing.
//!
//! Download loops call [`ProgressReporter::bytes`] for every 64 KiB chunk — that
//! would be ~50k events/min at 50 MB/s and would drown the websocket. The
//! reporter therefore rate-limits to one event per `interval` (default 120 ms)
//! but always emits the first and the last event of a job, so the UI never shows
//! a stale spinner after completion.

use crate::event::EventBus;
use crate::ids::ServerId;
use crate::model::{now_ms, PanelEvent, ProgressEvent, ProgressPhase};
use std::sync::Arc;
use std::time::Duration;

/// EWMA smoothing constant for throughput: 0.2 keeps the number stable while
/// still reacting to a real speed change within ~5 samples.
const RATE_ALPHA: f64 = 0.2;

#[derive(Debug, Clone)]
pub struct ProgressReporter {
    inner: Arc<Inner>,
}

#[derive(Debug)]
struct Inner {
    job_id: String,
    server_id: Option<ServerId>,
    bus: Option<Arc<EventBus>>,
    state: parking_lot::Mutex<State>,
}

#[derive(Debug)]
struct State {
    phase: ProgressPhase,
    last_emit_ms: i64,
    interval: Duration,
    bytes_done: u64,
    bytes_total: Option<u64>,
    rate_bps: Option<f64>,
    started_at_ms: i64,
    last_tick_ms: i64,
    finished: bool,
    message: String,
}

impl ProgressReporter {
    pub fn new(job_id: impl Into<String>, server_id: Option<ServerId>, phase: ProgressPhase, bus: Arc<EventBus>) -> Self {
        Self::build(job_id, server_id, phase, Some(bus), Duration::from_millis(120))
    }

    /// A reporter that only accumulates (used in tests and for nested jobs whose
    /// output is folded into a parent job).
    pub fn silent(job_id: impl Into<String>, server_id: Option<ServerId>, phase: ProgressPhase) -> Self {
        Self::build(job_id, server_id, phase, None, Duration::from_millis(120))
    }

    fn build(
        job_id: impl Into<String>,
        server_id: Option<ServerId>,
        phase: ProgressPhase,
        bus: Option<Arc<EventBus>>,
        interval: Duration,
    ) -> Self {
        let now = now_ms();
        Self {
            inner: Arc::new(Inner {
                job_id: job_id.into(),
                server_id,
                bus,
                state: parking_lot::Mutex::new(State {
                    phase,
                    last_emit_ms: 0,
                    interval,
                    bytes_done: 0,
                    bytes_total: None,
                    rate_bps: None,
                    started_at_ms: now,
                    last_tick_ms: now,
                    finished: false,
                    message: phase_label(phase).to_string(),
                }),
            }),
        }
    }

    pub fn job_id(&self) -> &str {
        &self.inner.job_id
    }

    pub fn set_bytes_per_event(&self, interval: Duration) {
        self.inner.state.lock().interval = interval;
    }

    /// Announce a phase transition (Download → Verify → Install). Always emitted;
    /// the UI animates between phases.
    pub fn set_phase(&self, phase: ProgressPhase) {
        {
            let mut st = self.inner.state.lock();
            st.phase = phase;
            st.message = phase_label(phase).to_string();
            st.last_emit_ms = 0; // force the next sample to emit
        }
        self.emit_current(None, false, None);
    }

    /// Free-form status line ("Resolving Paper builds…"). Always emitted.
    pub fn message(&self, msg: impl Into<String>) {
        let msg = msg.into();
        self.inner.state.lock().message = msg.clone();
        self.emit_current(Some(msg), false, None);
    }

    /// Announce the total size as soon as it is known (from Content-Length).
    pub fn total(&self, bytes: u64) {
        self.inner.state.lock().bytes_total = Some(bytes);
    }

    /// Record `n` bytes and maybe emit an event.
    pub fn bytes(&self, n: u64) {
        let due = {
            let mut st = self.inner.state.lock();
            let now = now_ms();
            st.bytes_done = st.bytes_done.saturating_add(n);
            let window_ms = (now - st.last_tick_ms).max(1);
            st.last_tick_ms = now;
            let instant = (n as f64 * 1000.0) / window_ms as f64;
            st.rate_bps = Some(match st.rate_bps {
                Some(prev) if prev > 0.0 => prev * (1.0 - RATE_ALPHA) + instant * RATE_ALPHA,
                _ => {
                    let elapsed = (now - st.started_at_ms).max(1) as f64 / 1000.0;
                    st.bytes_done as f64 / elapsed
                }
            });
            if now - st.last_emit_ms >= st.interval.as_millis() as i64 {
                st.last_emit_ms = now;
                true
            } else {
                false
            }
        };
        if due {
            self.emit_current(None, false, None);
        }
    }

    /// Count-based progress ("37 of 150 mods installed").
    pub fn fraction(&self, value: f64, msg: impl Into<String>) {
        let msg = msg.into();
        self.inner.state.lock().message = msg.clone();
        let mut ev = self.snapshot(Some(msg));
        ev.fraction = Some(value.clamp(0.0, 1.0));
        self.publish(ev);
    }

    /// Terminal success event at 100 %.
    pub fn complete(&self, msg: impl Into<String>) {
        let msg = msg.into();
        {
            let mut st = self.inner.state.lock();
            st.finished = true;
            st.message = msg.clone();
        }
        self.emit_current(Some(msg), true, None);
    }

    /// Terminal failure event; the UI shows the message inline on the card.
    pub fn fail(&self, err: &crate::CoreError) {
        self.inner.state.lock().finished = true;
        self.emit_current(None, true, Some(err.to_string()));
    }

    pub fn is_finished(&self) -> bool {
        self.inner.state.lock().finished
    }

    pub fn snapshot(&self, msg: Option<String>) -> ProgressEvent {
        let st = self.inner.state.lock();
        let eta_secs = match (st.bytes_total, st.rate_bps) {
            (Some(total), Some(rate)) if rate > 1.0 && total >= st.bytes_done => {
                Some((total - st.bytes_done) as f64 / rate)
            }
            _ => None,
        };
        ProgressEvent {
            job_id: self.inner.job_id.clone(),
            server_id: self.inner.server_id,
            phase: st.phase,
            fraction: st.bytes_total.map(|t| {
                if t == 0 {
                    1.0
                } else {
                    (st.bytes_done as f64 / t as f64).min(1.0)
                }
            }),
            bytes_done: st.bytes_done,
            bytes_total: st.bytes_total,
            rate_bps: st.rate_bps,
            eta_secs,
            message: msg.unwrap_or_else(|| st.message.clone()),
            at_ms: now_ms(),
            finished: st.finished,
            error: None,
        }
    }

    fn emit_current(&self, msg: Option<String>, force: bool, error: Option<String>) {
        let mut ev = self.snapshot(msg);
        ev.error = error;
        if force {
            ev.finished = true;
            ev.fraction = Some(1.0);
        }
        self.publish(ev);
    }

    fn publish(&self, event: ProgressEvent) {
        if let Some(bus) = &self.inner.bus {
            bus.publish(PanelEvent::Progress(event));
        }
    }
}

pub fn phase_label(phase: ProgressPhase) -> &'static str {
    match phase {
        ProgressPhase::Resolve => "Resolving version",
        ProgressPhase::Download => "Downloading",
        ProgressPhase::Verify => "Verifying checksums",
        ProgressPhase::Extract => "Extracting",
        ProgressPhase::Build => "Building",
        ProgressPhase::Install => "Installing",
        ProgressPhase::Backup => "Backing up",
        ProgressPhase::Restore => "Restoring",
        ProgressPhase::JavaInstall => "Installing Java runtime",
        ProgressPhase::SelfUpdate => "Updating ServerPanel",
    }
}

/// Human-readable rate, for logs and CLI output (the frontend has its own
/// `formatBytes` for the UI).
pub fn format_rate(bytes_per_sec: f64) -> String {
    const UNITS: [&str; 5] = ["B/s", "kB/s", "MB/s", "GB/s", "TB/s"];
    let mut v = bytes_per_sec.max(0.0);
    let mut i = 0;
    while v >= 1024.0 && i + 1 < UNITS.len() {
        v /= 1024.0;
        i += 1;
    }
    if i == 0 {
        format!("{v:.0} {}", UNITS[i])
    } else {
        format!("{v:.1} {}", UNITS[i])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn reporter() -> (ProgressReporter, Arc<EventBus>) {
        let bus = Arc::new(EventBus::new(1024));
        let r = ProgressReporter::new("job-1", None, ProgressPhase::Download, bus.clone());
        // 0 interval would defeat the throttle test; 120 ms is the default.
        (r, bus)
    }

    #[tokio::test]
    async fn throttles_intermediate_events_but_keeps_endpoints() {
        let (r, bus) = reporter();
        let mut sub = bus.subscribe();
        r.message("starting");
        for _ in 0..1000 {
            r.bytes(64 * 1024);
        }
        r.complete("done");

        let mut msgs = Vec::new();
        while let Ok(ev) = tokio::time::timeout(Duration::from_millis(30), sub.recv()).await {
            if let PanelEvent::Progress(p) = ev.event {
                msgs.push(p);
            }
        }
        assert!(msgs.len() >= 2, "expected first + last at minimum, got {}", msgs.len());
        assert!(msgs.len() < 1000, "expected coalescing, got {} events", msgs.len());
        let last = msgs.last().unwrap();
        assert!(last.finished);
        assert_eq!(last.message, "done");
        assert_eq!(last.fraction, Some(1.0));
        assert_eq!(msgs.first().unwrap().message, "starting");
    }

    #[tokio::test]
    async fn fraction_and_totals_track() {
        let (r, _bus) = reporter();
        r.total(1000);
        r.bytes(250);
        let snap = r.snapshot(None);
        assert_eq!(snap.fraction, Some(0.25));
        assert_eq!(snap.bytes_total, Some(1000));
        r.bytes(1000); // overshoot is clamped, never > 1.0
        assert_eq!(r.snapshot(None).fraction, Some(1.0));
    }

    #[tokio::test]
    async fn eta_shrinks_as_bytes_land() {
        let (r, _bus) = reporter();
        r.set_bytes_per_event(Duration::from_millis(0));
        r.total(10_000_000);
        for _ in 0..50 {
            r.bytes(100_000);
        }
        let a = r.snapshot(None).eta_secs.expect("rate known");
        for _ in 0..200 {
            r.bytes(100_000);
        }
        let b = r.snapshot(None).eta_secs.expect("rate known");
        assert!(b < a, "ETA should shrink: {a} -> {b}");
    }

    #[tokio::test]
    async fn fail_marks_finished_and_carries_error() {
        let (r, bus) = reporter();
        let mut sub = bus.subscribe();
        r.total(10);
        r.fail(&crate::CoreError::not_found("paper build 999"));
        let ev = sub.recv().await.unwrap();
        let PanelEvent::Progress(p) = ev.event else {
            panic!("expected a progress event")
        };
        assert!(p.finished);
        assert!(p.error.unwrap().contains("paper build 999"));
    }

    #[tokio::test]
    async fn phase_change_is_emitted_and_remembered() {
        let (r, bus) = reporter();
        let mut sub = bus.subscribe();
        r.set_phase(ProgressPhase::Verify);
        let ev = sub.recv().await.unwrap();
        let PanelEvent::Progress(p) = ev.event else {
            panic!("expected progress")
        };
        assert_eq!(p.phase, ProgressPhase::Verify);
        assert_eq!(p.message, "Verifying checksums");
    }

    #[tokio::test]
    async fn silent_reporter_never_panics_without_bus() {
        let r = ProgressReporter::silent("j", None, ProgressPhase::Verify);
        r.total(4);
        r.bytes(4);
        r.set_phase(ProgressPhase::Install);
        r.fraction(0.5, "half");
        r.complete("ok");
        r.message("still fine");
        assert!(r.is_finished());
    }

    #[test]
    fn rate_formatting() {
        assert_eq!(format_rate(512.0), "512 B/s");
        assert_eq!(format_rate(2.5 * 1024.0 * 1024.0), "2.5 MB/s");
        assert_eq!(format_rate(-1.0), "0 B/s");
    }
}
