//! Bounded console buffer with monotonic sequence numbers.
//!
//! The ring is the console's source of truth for "what did I miss while my socket
//! was down": a subscriber sends its last seen `seq` on connect and we replay from
//! there. When the subscriber is behind by more than the capacity, we tell it to
//! refetch from the on-disk log instead of lying with partial data.

use parking_lot::Mutex;
use sp_core::model::{now_ms, LogLine, LogStream};
use std::collections::VecDeque;

#[derive(Debug, Default)]
struct Inner {
    lines: VecDeque<LogLine>,
    next_seq: u64,
    dropped_before: u64,
    total: u64,
}

#[derive(Debug, Clone)]
pub struct LogRing {
    inner: Arc<Mutex<Inner>>,
    capacity: usize,
}

use std::sync::Arc;

impl LogRing {
    pub fn new(capacity: usize) -> Self {
        Self {
            inner: Arc::new(Mutex::new(Inner::default())),
            capacity: capacity.clamp(64, 100_000),
        }
    }

    pub fn with_capacity(capacity: usize) -> Self {
        Self::new(capacity)
    }

    /// Append one line and return its sequence number.
    pub fn push(&self, stream: LogStream, raw: String, plain: String) -> u64 {
        let mut inner = self.inner.lock();
        inner.next_seq += 1;
        let seq = inner.next_seq;
        inner.total += 1;
        inner.lines.push_back(LogLine {
            seq,
            at_ms: now_ms(),
            stream,
            raw,
            plain,
        });
        while inner.lines.len() > self.capacity {
            if let Some(removed) = inner.lines.pop_front() {
                inner.dropped_before = removed.seq + 1;
            }
        }
        seq
    }

    /// Replay everything strictly after `after_seq`.
    ///
    /// `Err(needed)` when the caller is too far behind — the UI should then refetch
    /// the log tail from `GET /api/servers/{id}/console/log`.
    pub fn after(&self, after_seq: u64) -> Result<Vec<LogLine>, u64> {
        let inner = self.inner.lock();
        let earliest = inner.lines.front().map(|l| l.seq).unwrap_or(0);
        if after_seq + 1 < earliest && after_seq != 0 {
            return Err(earliest);
        }
        Ok(inner
            .lines
            .iter()
            .filter(|l| l.seq > after_seq)
            .cloned()
            .collect())
    }

    /// Last `n` lines, oldest first — used for the initial console paint.
    pub fn tail(&self, n: usize) -> Vec<LogLine> {
        let inner = self.inner.lock();
        let start = inner.lines.len().saturating_sub(n.max(1));
        inner.lines.iter().skip(start).cloned().collect()
    }

    pub fn len(&self) -> usize {
        self.inner.lock().lines.len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    pub fn capacity(&self) -> usize {
        self.capacity
    }

    pub fn last_seq(&self) -> u64 {
        self.inner.lock().next_seq
    }

    /// Total lines ever pushed (for the "N lines dropped" notice).
    pub fn stats(&self) -> RingStats {
        let inner = self.inner.lock();
        RingStats {
            total: inner.total,
            retained: inner.lines.len(),
            first_seq: inner.lines.front().map(|l| l.seq).unwrap_or(0),
            last_seq: inner.next_seq,
            dropped_before: inner.dropped_before,
        }
    }

    /// Case-insensitive substring search over the retained buffer, newest first.
    pub fn search(&self, needle: &str, limit: usize) -> Vec<LogLine> {
        if needle.is_empty() {
            return Vec::new();
        }
        let lower = needle.to_ascii_lowercase();
        let inner = self.inner.lock();
        inner
            .lines
            .iter()
            .rev()
            .filter(|l| l.plain.to_ascii_lowercase().contains(&lower))
            .take(limit.max(1))
            .cloned()
            .collect()
    }

    pub fn clear(&self) {
        let mut inner = self.inner.lock();
        // Sequence numbers are never reused: subscribers must be able to tell a
        // cleared buffer from a "you are up to date" answer.
        inner.lines.clear();
    }
}

#[derive(Debug, Clone, Copy, Default)]
pub struct RingStats {
    pub total: u64,
    pub retained: usize,
    pub first_seq: u64,
    pub last_seq: u64,
    pub dropped_before: u64,
}

/// Strip ANSI/VT escape sequences so a line is searchable and storable as text.
/// Handles the subsets Minecraft servers actually emit (SGR colours, cursor moves,
/// the `\r` progress-bar trick, OSC titles) without a regex dependency on the hot path.
pub fn strip_ansi(input: &str) -> String {
    if !input.contains('\x1b') && !input.contains('\r') {
        return input.to_string();
    }
    let mut out = String::with_capacity(input.len());
    let mut chars = input.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\x1b' => match chars.peek() {
                Some('[') => {
                    chars.next();
                    // CSI: consume parameters until a final byte in @..~
                    while let Some(&n) = chars.peek() {
                        chars.next();
                        if ('\x40'..='\x7e').contains(&n) {
                            break;
                        }
                    }
                }
                Some(']') => {
                    chars.next();
                    // OSC: until BEL or ST
                    while let Some(&n) = chars.peek() {
                        chars.next();
                        if n == '\x07' {
                            break;
                        }
                        if n == '\x1b' {
                            chars.next();
                            chars.next();
                            break;
                        }
                    }
                }
                Some('(') | Some(')') | Some('*') | Some('>') | Some('=') => {
                    chars.next();
                    chars.next();
                }
                _ => {}
            },
            // Keep the carriage return visible as a line break: the console shows
            // the last write of a progress bar, so the newest segment wins.
            '\r' => {
                let mut rest = String::new();
                for n in chars.by_ref() {
                    if n == '\n' {
                        break;
                    }
                    rest.push(n);
                }
                out.push_str(&rest);
                out.push('\n');
            }
            _ => out.push(c),
        }
    }
    while out.ends_with('\n') {
        out.pop();
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn push(ring: &LogRing, text: &str) -> u64 {
        ring.push(
            LogStream::Stdout,
            format!("\x1b[0;32m{text}\x1b[0m"),
            text.to_string(),
        )
    }

    #[test]
    fn bounded_growth_and_order() {
        let ring = LogRing::new(100);
        for i in 0..500 {
            push(&ring, &format!("line {i}"));
        }
        assert_eq!(ring.len(), 100);
        let s = ring.stats();
        assert_eq!(s.total, 500);
        assert_eq!(s.first_seq, 401);
        assert_eq!(s.last_seq, 500);
        assert!(s.dropped_before > 0);
        let tail = ring.tail(3);
        assert_eq!(
            tail.iter().map(|l| l.plain.as_str()).collect::<Vec<_>>(),
            vec!["line 497", "line 498", "line 499"]
        );
    }

    #[test]
    fn replay_after_sequence() {
        let ring = LogRing::new(10);
        for i in 0..5 {
            push(&ring, &format!("l{i}"));
        }
        assert_eq!(ring.after(3).unwrap().len(), 2);
        assert_eq!(ring.after(5).unwrap(), vec![]);
        assert_eq!(
            ring.after(0).unwrap().len(),
            5,
            "0 means everything retained"
        );
        assert_eq!(ring.after(99).unwrap(), vec![]);
    }

    #[test]
    fn too_old_is_an_error_not_a_silent_gap() {
        let ring = LogRing::new(10);
        for i in 0..50 {
            push(&ring, "x");
        }
        let err = ring.after(1).unwrap_err();
        assert_eq!(err, ring.stats().first_seq);
        // …and a fresh subscriber (after=0) still gets the retained window.
        assert_eq!(ring.after(0).unwrap().len(), 10);
    }

    #[test]
    fn search_is_case_insensitive_and_newest_first() {
        let ring = LogRing::new(50);
        push(&ring, "Loading chunk 1");
        push(&ring, "SAVE complete");
        push(&ring, "loading chunk 2");
        let hits = ring.search("load", 10);
        assert_eq!(hits.len(), 2);
        assert_eq!(hits[0].plain, "loading chunk 2");
        assert!(ring.search("", 10).is_empty());
        assert_eq!(ring.search("LOADING", 1).len(), 1);
    }

    #[test]
    fn clear_keeps_sequence_monotonic() {
        let ring = LogRing::new(8);
        push(&ring, "a");
        ring.clear();
        assert!(ring.is_empty());
        let seq = push(&ring, "b");
        assert_eq!(seq, 2, "sequences are never reused");
        assert_eq!(ring.after(1).unwrap()[0].plain, "b");
    }

    #[test]
    fn capacity_is_clamped() {
        assert_eq!(LogRing::new(1).capacity(), 64);
        assert_eq!(LogRing::new(10_000_000).capacity(), 100_000);
        assert_eq!(LogRing::with_capacity(500).capacity(), 500);
    }

    #[test]
    fn ansi_stripping() {
        assert_eq!(strip_ansi("\x1b[0;32mgreen\x1b[0m"), "green");
        assert_eq!(strip_ansi("\x1b[1m\x1b[31mred bold\x1b[0m"), "red bold");
        assert_eq!(strip_ansi("no escapes"), "no escapes");
        assert_eq!(strip_ansi("title\x1b]0;window title\x07rest"), "titlerest");
        assert_eq!(
            strip_ansi("a\rbbbb"),
            "bbbb",
            "carriage-return progress keeps the tail"
        );
        assert_eq!(strip_ansi("\x1b[2J\x1b[Hclear"), "clear");
        assert_eq!(strip_ansi("weird\x1b(Bx"), "weirdx");
        assert_eq!(strip_ansi("done\x1b[0m\r"), "done");
        assert_eq!(strip_ansi(""), "");
    }

    #[test]
    fn shared_across_threads() {
        let ring = Arc::new(LogRing::new(1000));
        let mut handles = Vec::new();
        for t in 0..4 {
            let r = ring.clone();
            handles.push(std::thread::spawn(move || {
                for i in 0..250 {
                    r.push(LogStream::Stdout, format!("t{t} {i}"), format!("t{t} {i}"));
                }
            }));
        }
        for h in handles {
            h.join().unwrap();
        }
        assert_eq!(ring.stats().total, 1000);
        assert_eq!(ring.last_seq(), 1000);
    }
}
