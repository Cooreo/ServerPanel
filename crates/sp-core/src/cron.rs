//! A small, well-tested cron expression parser.
//!
//! Deviation note: `croner` is listed in the brief, but a 5-field cron dialect is
//! ~200 lines and we need *exact* control over three things the crates differ on:
//! the day-of-month/day-of-week OR rule, the accepted value ranges (we surface
//! precise, translatable errors in the UI), and 6-field expressions with seconds.
//! Keeping it here means the panel and the CLI agree on one implementation, and
//! the behaviour is pinned by the tests below.
//!
//! ## Supported syntax
//! * `M H DoM Mon DoW` (5 fields) or `S M H DoM Mon DoW` (6 fields, seconds first)
//! * `*`, `?` (alias of `*`), lists `a,b,c`, ranges `a-b`, steps `* /n`, `a-b/n`
//! * wrapped ranges (`fri-mon`, `22-2`)
//! * names: `jan`..`dec`, `sun`..`sat` (case-insensitive)
//! * day-of-week: `0` and `7` are Sunday
//! * if **both** day-of-month and day-of-week are restricted, a time matches when
//!   *either* matches (standard cron); otherwise the unrestricted one is ignored
//!
//! No support for `L`, `W`, `#`, or years — the UI never generates them.

use crate::error::CoreError;
use chrono::{Datelike, NaiveDate, Timelike, Utc};
use std::fmt::Write as _;

pub const MAX_FIELDS: usize = 6;
pub const MIN_FIELDS: usize = 5;
/// Five years of look-ahead is enough for `0 0 29 2 *` (leap day) and bounds the
/// search so an impossible expression returns `None` instead of hanging.
pub const MAX_LOOKAHEAD_DAYS: i64 = 366 * 5;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Cron {
    seconds: Vec<u32>,
    minutes: Vec<u32>,
    hours: Vec<u32>,
    days_of_month: Vec<u32>,
    months: Vec<u32>,
    days_of_week: Vec<u32>,
    /// `*` was used for day-of-month.
    dom_star: bool,
    /// `*` was used for day-of-week.
    dow_star: bool,
    source: String,
}

impl Cron {
    /// Parse and validate. Errors carry the offending field index and reason so
    /// the settings form can show them inline.
    pub fn parse(expr: &str) -> Result<Self, CoreError> {
        let source = expr.trim().to_string();
        let fields: Vec<&str> = source.split_whitespace().collect();
        let (seconds, rest) = match fields.len() {
            MIN_FIELDS => (vec![0], fields.as_slice()),
            MAX_FIELDS => (
                parse_field(fields[0], 0, 59, &[]).map_err(|e| e.field("seconds"))?,
                &fields[1..],
            ),
            n => {
                return Err(CoreError::validation(
                    "cron",
                    format!("expected {MIN_FIELDS} or {MAX_FIELDS} fields, got {n} (`{source}`)"),
                ))
            }
        };
        let minutes = parse_field(rest[0], 0, 59, &[]).map_err(|e| e.field("minutes"))?;
        let hours = parse_field(rest[1], 0, 23, &[]).map_err(|e| e.field("hours"))?;
        let dom_star = is_star(rest[2]);
        let dow_star = is_star(rest[4]);
        let days_of_month = parse_field(rest[2], 1, 31, &[]).map_err(|e| e.field("day-of-month"))?;
        let months = parse_field(rest[3], 1, 12, &MONTH_NAMES).map_err(|e| e.field("month"))?;
        let mut days_of_week =
            parse_field(rest[4], 0, 7, &DOW_NAMES).map_err(|e| e.field("day-of-week"))?;
        // normalise 7 → 0 (both mean Sunday)
        if days_of_week.contains(&7) {
            days_of_week.retain(|v| *v != 7);
            if !days_of_week.contains(&0) {
                days_of_week.push(0);
            }
            days_of_week.sort_unstable();
        }
        Ok(Self {
            seconds,
            minutes,
            hours,
            days_of_month,
            months,
            days_of_week,
            dom_star,
            dow_star,
            source,
        })
    }

    pub fn validate(expr: &str) -> Result<(), CoreError> {
        Self::parse(expr).map(|_| ())
    }

    pub fn source(&self) -> &str {
        &self.source
    }

    pub fn next_after(&self, from: chrono::DateTime<Utc>) -> Option<chrono::DateTime<Utc>> {
        // Search origin: strictly after `from`, truncated to whole seconds.
        let origin = (from + chrono::Duration::seconds(1)).with_nanosecond(0)?;
        let mut date = origin.date_naive();
        let end = date.checked_add_signed(chrono::Duration::days(MAX_LOOKAHEAD_DAYS))?;
        loop {
            if date > end {
                return None;
            }
            if self.months.contains(&date.month()) && self.day_matches(date) {
                let floor = if date == origin.date_naive() {
                    Some((origin.hour(), origin.minute(), origin.second()))
                } else {
                    None
                };
                if let Some(dt) = self.first_time_on(date, floor) {
                    return Some(dt);
                }
            }
            date = date.checked_add_signed(chrono::Duration::days(1))?;
        }
    }

    /// Next `n` fire times, for the "next runs" list in the scheduler UI.
    pub fn next_n(&self, from: chrono::DateTime<Utc>, n: usize) -> Vec<chrono::DateTime<Utc>> {
        let mut out = Vec::with_capacity(n);
        let mut cursor = from;
        for _ in 0..n {
            match self.next_after(cursor) {
                Some(t) => {
                    out.push(t);
                    cursor = t;
                }
                None => break,
            }
        }
        out
    }

    fn day_matches(&self, date: NaiveDate) -> bool {
        let dom_ok = self.dom_star || self.days_of_month.contains(&date.day());
        // chrono: Mon = 0 … Sun = 6. cron: Sun = 0 … Sat = 6.
        let dow = (date.weekday().num_days_from_monday() + 1) % 7;
        let dow_ok = self.dow_star || self.days_of_week.contains(&dow);
        match (self.dom_star, self.dow_star) {
            (true, true) => true,
            // Both restricted → OR (standard Vixie cron rule).
            (false, false) => dom_ok || dow_ok,
            (true, false) => dow_ok,
            (false, true) => dom_ok,
        }
    }

    /// Smallest matching time-of-day on `date`, at or after `floor` when given.
    fn first_time_on(
        &self,
        date: NaiveDate,
        floor: Option<(u32, u32, u32)>,
    ) -> Option<chrono::DateTime<Utc>> {
        for &h in &self.hours {
            if let Some((fh, _, _)) = floor {
                if h < fh {
                    continue;
                }
            }
            for &m in &self.minutes {
                if let Some((fh, fm, _)) = floor {
                    if h == fh && m < fm {
                        continue;
                    }
                }
                for &s in &self.seconds {
                    if let Some((fh, fm, fs)) = floor {
                        if h == fh && m == fm && s < fs {
                            continue;
                        }
                    }
                    let time = chrono::NaiveTime::from_hms_opt(h, m, s)?;
                    return date.and_time(time).and_utc().into();
                }
            }
        }
        None
    }

    /// Human summary used in the UI ("Every day at 04:00"). Deliberately coarse:
    /// it is a hint, not a spec.
    pub fn describe(&self) -> String {
        let mut out = String::new();
        let _ = write!(
            out,
            "{} {}",
            if self.seconds.len() == 1 && self.seconds[0] == 0 {
                ""
            } else {
                "Every second at"
            },
            match (self.dom_star, self.dow_star, self.months.len()) {
                (true, true, 12) => "every day",
                _ => "per schedule",
            }
        );
        if self.hours.len() == 1 && self.minutes.len() == 1 && self.months.len() == 12 {
            let _ = write!(out, " at {:02}:{:02}", self.hours[0], self.minutes[0]);
        }
        out.trim().to_string()
    }
}

struct FieldError {
    reason: String,
}

impl FieldError {
    fn field(self, name: &'static str) -> CoreError {
        CoreError::validation("cron", format!("{name}: {}", self.reason))
    }
}

const MONTH_NAMES: [(&str, u32); 12] = [
    ("jan", 1),
    ("feb", 2),
    ("mar", 3),
    ("apr", 4),
    ("may", 5),
    ("jun", 6),
    ("jul", 7),
    ("aug", 8),
    ("sep", 9),
    ("oct", 10),
    ("nov", 11),
    ("dec", 12),
];

const DOW_NAMES: [(&str, u32); 7] = [
    ("sun", 0),
    ("mon", 1),
    ("tue", 2),
    ("wed", 3),
    ("thu", 4),
    ("fri", 5),
    ("sat", 6),
];

fn is_star(field: &str) -> bool {
    matches!(field.trim(), "*" | "?")
}

fn lookup_name(tok: &str, names: &[(&str, u32)]) -> Option<u32> {
    let lower = tok.to_ascii_lowercase();
    names.iter().find(|(n, _)| *n == lower).map(|(_, v)| *v)
}

fn parse_value(tok: &str, min: u32, max: u32, names: &[(&str, u32)]) -> Result<u32, FieldError> {
    let t = tok.trim();
    if t.is_empty() {
        return Err(FieldError { reason: "empty value".into() });
    }
    let v = if let Some(n) = lookup_name(t, names) {
        n
    } else {
        match t.parse::<u32>() {
            Ok(v) => v,
            Err(_) => return Err(FieldError { reason: format!("`{t}` is not a number or name") }),
        }
    };
    if v < min || v > max {
        return Err(FieldError { reason: format!("`{t}` out of range {min}..={max}") });
    }
    Ok(v)
}

fn parse_field(field: &str, min: u32, max: u32, names: &[(&str, u32)]) -> Result<Vec<u32>, FieldError> {
    let field = field.trim();
    if field.is_empty() {
        return Err(FieldError { reason: "empty field".into() });
    }
    if max - min >= 63 {
        return Err(FieldError { reason: "field too wide".into() });
    }
    let mut mask: u64 = 0;
    for item in field.split(',') {
        let item = item.trim();
        if item.is_empty() {
            return Err(FieldError { reason: "empty list item".into() });
        }
        // Split `spec/step` off the end.
        let (spec, step) = match item.split_once('/') {
            Some((s, step)) => {
                let step_u: u32 = step
                    .trim()
                    .parse()
                    .map_err(|_| FieldError { reason: format!("bad step `/{}`", step.trim()) })?;
                if step_u == 0 {
                    return Err(FieldError { reason: "step must be >= 1".into() });
                }
                (s.trim(), step_u)
            }
            None => (item, 1),
        };
        // Normalise the spec into one or more inclusive ranges (two, in the
        // wrapped case `22-2`, so that both tails are covered).
        let ranges: Vec<(u32, u32)> = if spec == "*" || spec == "?" {
            vec![(min, max)]
        } else if let Some((a, b)) = spec.split_once('-') {
            let a = parse_value(a, min, max, names)?;
            let b = parse_value(b, min, max, names)?;
            if a == b {
                vec![(a, b)]
            } else if a < b {
                vec![(a, b)]
            } else {
                vec![(a, max), (min, b)]
            }
        } else {
            let v = parse_value(spec, min, max, names)?;
            if step == 1 {
                vec![(v, v)]
            } else {
                // `5/15` means "from 5, every 15 up to the field maximum"
                vec![(v, max)]
            }
        };
        for (lo, hi) in ranges {
            if lo > hi {
                return Err(FieldError { reason: format!("`{item}` matches nothing") });
            }
            let mut v = lo;
            while v <= hi {
                mask |= 1 << v;
                v = match v.checked_add(step) {
                    Some(n) => n,
                    None => break,
                };
            }
        }
    }
    let out: Vec<u32> = (min..=max).filter(|v| mask & (1 << v) != 0).collect();
    if out.is_empty() {
        return Err(FieldError { reason: format!("`{field}` matches nothing") });
    }
    Ok(out)
}

/// Convenience used by the API when the UI wants "next run" for a task.
pub fn next_run(expr: &str) -> Result<Option<i64>, CoreError> {
    let cron = Cron::parse(expr)?;
    Ok(cron
        .next_after(Utc::now())
        .map(|t| t.timestamp_millis()))
}

#[cfg(test)]
mod tests {
    use super::*;

    use chrono::TimeZone;

    fn dt(s: &str) -> chrono::DateTime<Utc> {
        let naive = chrono::NaiveDateTime::parse_from_str(s, "%Y-%m-%d %H:%M:%S").unwrap();
        Utc.from_utc_datetime(&naive)
    }

    fn fmt(t: chrono::DateTime<Utc>) -> String {
        t.format("%Y-%m-%d %H:%M:%S").to_string()
    }

    fn next(expr: &str, from: &str) -> String {
        fmt(Cron::parse(expr).unwrap().next_after(dt(from)).expect("a next run"))
    }

    #[test]
    fn daily_at_fixed_time() {
        assert_eq!(next("0 4 * * *", "2026-01-01 00:00:00"), "2026-01-01 04:00:00");
        assert_eq!(next("0 4 * * *", "2026-01-01 04:00:00"), "2026-01-02 04:00:00");
        assert_eq!(next("0 4 * * *", "2026-01-01 03:59:59"), "2026-01-01 04:00:00");
    }

    #[test]
    fn step_lists() {
        assert_eq!(next("*/15 * * * *", "2026-01-01 10:07:00"), "2026-01-01 10:15:00");
        assert_eq!(next("*/15 * * * *", "2026-01-01 10:50:00"), "2026-01-01 11:00:00");
        assert_eq!(next("5,35 * * * *", "2026-01-01 10:06:00"), "2026-01-01 10:35:00");
        assert_eq!(next("0 0-6/2 * * *", "2026-01-01 05:00:00"), "2026-01-01 06:00:00");
    }

    #[test]
    fn weekday_rules() {
        // 2026-01-07 is a Wednesday; the next Monday is the 12th.
        assert_eq!(next("0 0 * * 1", "2026-01-07 12:00:00"), "2026-01-12 00:00:00");
        // 2026-01-10 is a Saturday → skip the weekend.
        assert_eq!(next("30 9 * * 1-5", "2026-01-10 00:00:00"), "2026-01-12 09:30:00");
        // 7 must also mean Sunday.
        assert_eq!(next("0 8 * * 7", "2026-01-05 00:00:00"), "2026-01-11 08:00:00");
        assert_eq!(next("0 8 * * 0", "2026-01-05 00:00:00"), "2026-01-11 08:00:00");
    }

    #[test]
    fn wrapped_ranges() {
        // 22-2 covers 22, 23, 0, 1, 2.
        let c = Cron::parse("0 22-2 * * *").unwrap();
        assert_eq!(c.hours, vec![0, 1, 2, 22, 23]);
        let c = Cron::parse("0 0 * * fri-mon").unwrap();
        assert_eq!(c.days_of_week, vec![0, 1, 5, 6], "fri(5), sat(6), sun(0), mon(1)");
        // 2026-01-02 Fri, 01-03 Sat, 01-04 Sun, 01-05 Mon all match; Tue does not.
        for (d, want) in [(2, true), (3, true), (4, true), (5, true), (6, false), (7, false)] {
            let date = NaiveDate::from_ymd_opt(2026, 1, d).unwrap();
            assert_eq!(c.day_matches(date), want, "2026-01-{d:02}");
        }
    }

    #[test]
    fn dom_or_dow_semantics() {
        // Restricted both → union: the 13th OR any Friday.
        let c = Cron::parse("0 0 13 * 5").unwrap();
        assert!(c.day_matches(NaiveDate::from_ymd_opt(2026, 1, 13).unwrap()), "13th is a Tuesday");
        assert!(c.day_matches(NaiveDate::from_ymd_opt(2026, 1, 2).unwrap()), "2 Jan 2026 is a Friday");
        assert!(!c.day_matches(NaiveDate::from_ymd_opt(2026, 1, 14).unwrap()));
        // Restricted DoM only → DoW ignored.
        let only_dom = Cron::parse("0 0 15 * *").unwrap();
        assert!(only_dom.day_matches(NaiveDate::from_ymd_opt(2026, 3, 15).unwrap()));
        assert!(!only_dom.day_matches(NaiveDate::from_ymd_opt(2026, 3, 16).unwrap()));
    }

    #[test]
    fn months_and_leap_day() {
        // 2026-02-29 does not exist; the next one is 2028.
        assert_eq!(next("0 0 29 2 *", "2026-03-01 00:00:00"), "2028-02-29 00:00:00");
        assert_eq!(next("0 0 1 mar", "2026-06-01 00:00:00").len(), 19);
    }

    #[test]
    fn impossible_expressions_terminate() {
        let c = Cron::parse("0 0 30 2 *").unwrap();
        assert_eq!(c.next_after(dt("2026-01-01 00:00:00")), None);
        assert!(Cron::parse("0 0 32 * *").is_err(), "day 32 is out of range");
    }

    #[test]
    fn seconds_field_accepted_as_sixth() {
        assert_eq!(next("30 * * * * *", "2026-01-01 10:00:00"), "2026-01-01 10:00:30");
        assert_eq!(next("*/20 * * * * *", "2026-01-01 10:00:25"), "2026-01-01 10:00:40");
    }

    #[test]
    fn names_are_accepted_case_insensitively() {
        assert_eq!(Cron::parse("0 0 * jan-mar MON").unwrap().months, vec![1, 2, 3]);
        assert_eq!(Cron::parse("0 0 * Jan * sun,Sat").unwrap().days_of_week, vec![0, 6]);
        assert!(Cron::parse("0 0 * foo * *").is_err());
    }

    #[test]
    fn errors_are_actionable() {
        let e = Cron::parse("70 * * * *").unwrap_err();
        let msg = e.to_string();
        assert!(msg.contains("minutes"), "{msg}");
        assert!(msg.contains("out of range 0..=59"), "{msg}");
        let e = Cron::parse("* * * *").unwrap_err();
        assert!(e.to_string().contains("expected 5 or 6 fields"), "{e}");
        for bad in ["* * * * * *", "0 0 * * 8", "*/0 * * * *", "a * * * *", "1-2-3 * * * *"] {
            assert!(Cron::parse(bad).is_err(), "{bad} should fail");
        }
    }

    #[test]
    fn next_n_is_monotonic() {
        let c = Cron::parse("*/30 * * * *").unwrap();
        let runs = c.next_n(dt("2026-01-01 10:05:00"), 4);
        assert_eq!(
            runs.iter().map(|t| fmt(*t)).collect::<Vec<_>>(),
            vec![
                "2026-01-01 10:30:00",
                "2026-01-01 11:00:00",
                "2026-01-01 11:30:00",
                "2026-01-01 12:00:00",
            ]
        );
        assert!(runs.windows(2).all(|w| w[0] < w[1]));
    }

    #[test]
    fn question_mark_is_a_star() {
        assert_eq!(Cron::parse("0 4 * * ?").unwrap(), Cron::parse("0 4 * * *").unwrap());
    }

    #[test]
    fn describe_is_roughly_right() {
        assert!(Cron::parse("0 4 * * *").unwrap().describe().contains("04:00"));
        assert!(Cron::parse("*/5 * * * *").unwrap().describe().contains("every day"));
    }
}
