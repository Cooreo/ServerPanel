//! TTL response cache and rate-limit bookkeeping.
//!
//! Modrinth, Mojang and PaperMC all publish short-lived caches; we honour them with
//! a process-local TTL map rather than a disk cache, because a panel restart should
//! not be the reason a version list is stale. `RateLimit` reads the `X-RateLimit-*`
//! headers so callers can back off *before* they get a 429.

use parking_lot::Mutex;
use std::collections::HashMap;
use std::time::{Duration, Instant};

#[derive(Debug, Clone)]
struct Entry {
    at: Instant,
    body: String,
    status: u16,
}

#[derive(Debug, Clone)]
pub struct TtlCache {
    inner: Mutex<HashMap<String, Entry>>,
    ttl: Duration,
    max_entries: usize,
    hits: std::sync::atomic::AtomicU64,
    misses: std::sync::atomic::AtomicU64,
}

impl TtlCache {
    pub fn new(ttl: Duration) -> Self {
        Self::with_capacity(ttl, 512)
    }

    pub fn with_capacity(ttl: Duration, max_entries: usize) -> Self {
        Self {
            inner: Mutex::new(HashMap::new()),
            ttl,
            max_entries: max_entries.max(8),
            hits: std::sync::atomic::AtomicU64::new(0),
            misses: std::sync::atomic::AtomicU64::new(0),
        }
    }

    pub fn ttl(&self) -> Duration {
        self.ttl
    }

    /// Fresh value for `key`, if any.
    pub fn get(&self, key: &str) -> Option<String> {
        let mut map = self.inner.lock();
        match map.get(key) {
            Some(e) if e.at.elapsed() < self.ttl => {
                self.hits.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                Some(e.body.clone())
            }
            Some(_) => {
                map.remove(key);
                self.misses.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                None
            }
            None => {
                self.misses.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                None
            }
        }
    }

    /// Fresh value *or* the stale one, for "serve-stale-while-revalidate" paths
    /// where a 60-second-old version list beats an error screen.
    pub fn get_stale_ok(&self, key: &str, max_stale: Duration) -> Option<(String, bool)> {
        let map = self.inner.lock();
        map.get(key).and_then(|e| {
            let age = e.at.elapsed();
            (age < max_stale).then(|| (e.body.clone(), age >= self.ttl))
        })
    }

    pub fn insert(&self, key: impl Into<String>, body: impl Into<String>) {
        self.insert_status(key, body, 200);
    }

    pub fn insert_status(&self, key: impl Into<String>, body: impl Into<String>, status: u16) {
        let key = key.into();
        let mut map = self.inner.lock();
        if map.len() >= self.max_entries {
            // Evict the oldest entry; good enough and O(n) on a bounded map.
            if let Some(oldest) = map
                .iter()
                .min_by_key(|(_, e)| e.at)
                .map(|(k, _)| k.clone())
            {
                map.remove(&oldest);
            }
        }
        map.insert(key, Entry { at: Instant::now(), body: body.into(), status });
    }

    pub fn invalidate_prefix(&self, prefix: &str) -> usize {
        let mut map = self.inner.lock();
        let before = map.len();
        map.retain(|k, _| !k.starts_with(prefix));
        before - map.len()
    }

    pub fn clear(&self) {
        self.inner.lock().clear();
    }

    pub fn len(&self) -> usize {
        self.inner.lock().len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    pub fn stats(&self) -> CacheStats {
        use std::sync::atomic::Ordering::Relaxed;
        CacheStats {
            hits: self.hits.load(Relaxed),
            misses: self.misses.load(Relaxed),
            entries: self.len(),
        }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct CacheStats {
    pub hits: u64,
    pub misses: u64,
    pub entries: usize,
}

impl CacheStats {
    pub fn hit_ratio(&self) -> f64 {
        let total = self.hits + self.misses;
        if total == 0 {
            0.0
        } else {
            self.hits as f64 / total as f64
        }
    }
}

/// Parsed `X-RateLimit-*` / `Retry-After` state for one upstream.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct RateLimit {
    pub limit: Option<u32>,
    pub remaining: Option<u32>,
    /// Unix seconds when the window resets.
    pub reset_at_epoch: Option<u64>,
}

impl RateLimit {
    /// Header name lookup is case-insensitive in `http::HeaderMap`, so this works
    /// for Modrinth (`X-Ratelimit-Remaining`) and any other spelling.
    pub fn from_headers(headers: &reqwest::header::HeaderMap) -> Self {
        let get = |name: &str| {
            headers
                .get(name)
                .and_then(|v| v.to_str().ok())
                .and_then(|v| v.trim().parse::<u32>().ok())
        };
        let get_u64 = |name: &str| {
            headers
                .get(name)
                .and_then(|v| v.to_str().ok())
                .and_then(|v| v.trim().parse::<u64>().ok())
        };
        Self {
            limit: get("x-ratelimit-limit").or_else(|| get("x-rate-limit-limit")),
            remaining: get("x-ratelimit-remaining").or_else(|| get("x-rate-limit-remaining")),
            reset_at_epoch: get_u64("x-ratelimit-reset").or_else(|| get_u64("x-rate-limit-reset")),
        }
    }

    pub fn is_exhausted(&self) -> bool {
        matches!(self.remaining, Some(0))
    }

    /// Seconds until the window resets, if we know.
    pub fn retry_in(&self) -> Option<Duration> {
        let reset = self.reset_at_epoch?;
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        Some(Duration::from_secs(reset.saturating_sub(now).min(300)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn caches_until_ttl_expires() {
        let c = TtlCache::new(Duration::from_millis(20));
        assert!(c.is_empty());
        c.insert("a", "1");
        assert_eq!(c.get("a").as_deref(), Some("1"));
        assert_eq!(c.get("b"), None);
        std::thread::sleep(Duration::from_millis(35));
        assert_eq!(c.get("a"), None, "expired entries must not be served");
        let s = c.stats();
        assert_eq!((s.hits, s.misses), (1, 2));
        assert!(s.hit_ratio() > 0.3 && s.hit_ratio() < 0.5);
    }

    #[test]
    fn stale_fallback_is_bounded() {
        let c = TtlCache::with_capacity(Duration::from_millis(1), 16);
        c.insert("versions", "[1,2]");
        std::thread::sleep(Duration::from_millis(10));
        let (body, stale) = c.get_stale_ok("versions", Duration::from_secs(60)).expect("stale hit");
        assert!(stale, "past TTL but within max_stale it is served as stale");
        assert_eq!(body, "[1,2]");
        assert!(c.get_stale_ok("versions", Duration::ZERO).is_none());
        assert!(c.get_stale_ok("missing", Duration::from_secs(60)).is_none());
    }

    #[test]
    fn eviction_and_invalidation() {
        let c = TtlCache::with_capacity(Duration::from_secs(60), 8);
        for i in 0..12 {
            c.insert(format!("modrinth:search:{i}"), format!("body{i}"));
        }
        assert!(c.len() <= 8, "capacity is enforced, got {}", c.len());
        let before = c.len();
        assert_eq!(c.invalidate_prefix("modrinth:search:"), before);
        assert!(c.is_empty());
        assert_eq!(c.invalidate_prefix("nothing-matches"), 0);
    }

    #[test]
    fn clears_entirely() {
        let c = TtlCache::new(Duration::from_secs(5));
        c.insert("x", "y");
        c.clear();
        assert_eq!(c.len(), 0);
    }

    #[test]
    fn rate_limit_headers_are_parsed_case_insensitively() {
        let mut h = reqwest::header::HeaderMap::new();
        h.insert("x-ratelimit-limit", "100".parse().unwrap());
        h.insert("X-RateLimit-Remaining".parse().unwrap(), "3".parse().unwrap());
        h.insert("x-ratelimit-reset", "99999999999".parse().unwrap());
        let rl = RateLimit::from_headers(&h);
        assert_eq!(rl.limit, Some(100));
        assert_eq!(rl.remaining, Some(3));
        assert_eq!(rl.reset_at_epoch, Some(99_999_999_999));
        assert!(!rl.is_exhausted());
        assert!(rl.retry_in().is_some(), "far-future reset is clamped, not dropped");

        let mut h2 = reqwest::header::HeaderMap::new();
        h2.insert("x-rate-limit-remaining", "0".parse().unwrap());
        let rl2 = RateLimit::from_headers(&h2);
        assert!(rl2.is_exhausted());
        assert_eq!(rl2.retry_in(), None, "no reset header means no advice");
        assert_eq!(RateLimit::from_headers(&Default::default()), RateLimit::default());
    }

    #[test]
    fn retry_in_never_waits_forever() {
        let rl = RateLimit {
            limit: Some(60),
            remaining: Some(0),
            reset_at_epoch: Some(u64::MAX / 2),
        };
        assert!(rl.retry_in().unwrap() <= Duration::from_secs(300));
        let past = RateLimit {
            limit: None,
            remaining: None,
            reset_at_epoch: Some(1),
        };
        assert_eq!(past.retry_in(), Some(Duration::ZERO));
    }

    #[test]
    fn statuses_are_recorded() {
        let c = TtlCache::new(Duration::from_secs(60));
        c.insert_status("k", "v", 404);
        assert_eq!(c.get("k").as_deref(), Some("v"));
        let map = c.inner.lock();
        assert_eq!(map.get("k").unwrap().status, 404);
    }
}
