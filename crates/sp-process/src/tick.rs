//! Console-line intelligence: TPS/MSPT, player counts, and lifecycle events.
//!
//! There is no stable machine-readable stream from a Minecraft server, so the
//! panel parses the human log. Everything here is written to *degrade to None*:
//! a server type we have not seen produce a line for simply reports `None` and
//! the UI renders a dash rather than a lie. When `spark` is installed, the
//! service layer prefers its numbers (see `docs/monitoring.md`).

use regex::Regex;
use std::sync::OnceLock;

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum ConsoleEvent {
    /// `Done (N.NNs)! For help, type "help"`
    BootDone,
    SaveStart,
    SaveDone,
    PlayerJoin,
    PlayerLeave,
    Crash,
    OutOfMemory,
    Reload,
    Stopping,
    /// `[Server thread/ERROR]`-style line worth surfacing in the UI.
    Error,
    Warning,
    Kick,
    WhitelistChange,
}

/// What we could learn from one line. All fields optional; most lines produce
/// nothing at all and are cheap.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ParsedLine {
    pub tps: Option<[f32; 3]>,
    pub mspt: Option<[f32; 3]>,
    pub players: Option<(u32, u32)>,
    pub player: Option<String>,
    pub event: Option<ConsoleEvent>,
    /// Milliseconds of lag from "Can't keep up!" lines.
    pub lag_ms: Option<u64>,
}

/// One lazily-compiled, process-lifetime regex.
///
/// `OnceLock` rather than a shared cache: it is sound (the value never moves),
/// lock-free after first use, and each pattern is named at its use site — which
/// matters because these run for every single console line.
macro_rules! pattern {
    ($name:ident, $re:expr) => {
        static $name: OnceLock<Regex> = OnceLock::new();
        #[inline]
        fn $name() -> &'static Regex {
            $name.get_or_init(|| {
                Regex::new($re).unwrap_or_else(|e| panic!("invalid pattern {:?}: {e}", $re))
            })
        }
    };
}

pattern!(RE_TPS, r"TPS[^\d-]*((?:\d+\.\d+|\d+)(?:\s*,\s*\d+\.\d+)*)");
pattern!(RE_MSPT, r"MSPT[^\d-]*((?:\d+\.\d+|\d+)(?:\s*,\s*\d+\.\d+)*)");
pattern!(RE_PLAYERS, r"There are (\d+) of a max(?:imum)? of (\d+) players online");
pattern!(RE_LOST, r"([A-Za-z0-9_]{1,16}) lost connection");
pattern!(RE_JOIN, r"([A-Za-z0-9_]{1,16}) (?:joined the game|has joined the game|entered the game)");
pattern!(RE_LEAVE, r"([A-Za-z0-9_]{1,16}) left the game");
pattern!(RE_CRASH_SAVED, r"This crash report has been saved to");
pattern!(RE_DONE, r"Done \([\d.,]+\s*s\w*\)!");
pattern!(RE_LAG, r"Can't keep up!.*?Running (\d+)ms or (\d+) ticks behind");
pattern!(RE_RELOAD, r"Reloading(?: \w+)?|Reload complete");
pattern!(RE_STOP, r"Stopping server|Closing Minecraft Server|stopping\.\.\.");

/// Three floats from a `19.98, 19.99, 20.0` capture group.
fn triple(text: &str) -> Option<[f32; 3]> {
    let nums: Vec<f32> = text
        .split(',')
        .filter_map(|p| p.trim().parse::<f32>().ok())
        .take(3)
        .collect();
    match nums.first() {
        Some(_) => {
            let mut out = [nums[0]; 3];
            for (i, v) in nums.iter().enumerate() {
                out[i] = *v;
            }
            Some(out)
        }
        None => None,
    }
}

/// Parse one already-stripped console line.
pub fn parse(plain: &str) -> ParsedLine {
    let mut out = ParsedLine::default();

    // --- TPS: Paper "TPS from last 1m, 5m, 15m: …", Fabric/spark "TPS: 20.0",
    //     Velocity "TPS: 19.9 (1m), 20.0 (5m)" --------------------------------
    if let Some(caps) = RE_TPS().captures(plain) {
        out.tps = triple(&caps[1]);
    }
    if let Some(caps) = RE_MSPT().captures(plain) {
        out.mspt = triple(&caps[1]);
    }
    if let Some(caps) = RE_PLAYERS().captures(plain) {
        if let (Some(online), Some(max)) = (caps[1].parse::<u32>().ok(), caps[2].parse::<u32>().ok()) {
            out.players = Some((online, max));
        }
    }

    // --- join / leave --------------------------------------------------------
    if let Some(caps) = RE_LOST().captures(plain) {
        out.player = Some(caps[1].to_string());
        out.event = Some(ConsoleEvent::PlayerLeave);
        return out;
    }
    if let Some(caps) = RE_JOIN().captures(plain) {
        out.player = Some(caps[1].to_string());
        out.event = Some(ConsoleEvent::PlayerJoin);
        return out;
    }
    if let Some(caps) = RE_LEAVE().captures(plain) {
        out.player = Some(caps[1].to_string());
        out.event = Some(ConsoleEvent::PlayerLeave);
        return out;
    }

    // --- lifecycle -----------------------------------------------------------
    if plain.contains("---- Minecraft Crash Report ----") || RE_CRASH_SAVED().is_match(plain) {
        out.event = Some(ConsoleEvent::Crash);
        return out;
    }
    if plain.contains("java.lang.OutOfMemoryError") || plain.contains("GC overhead limit exceeded") {
        out.event = Some(ConsoleEvent::OutOfMemory);
        return out;
    }
    if RE_DONE().is_match(plain) || (plain.contains("Done (") && plain.contains("For help, type")) {
        out.event = Some(ConsoleEvent::BootDone);
        return out;
    }
    if let Some(caps) = RE_LAG().captures(plain) {
        out.lag_ms = caps[1].parse().ok();
        out.event = Some(ConsoleEvent::Warning);
        return out;
    }
    if plain.contains("Saving fields")
        || plain.contains("Saving players")
        || plain.contains("Saving chunk data")
        || plain.contains("Saving stats")
    {
        out.event = Some(ConsoleEvent::SaveStart);
        return out;
    }
    if plain.contains("All dimensions are saved") || plain.contains("Finished saving") {
        out.event = Some(ConsoleEvent::SaveDone);
        return out;
    }
    if RE_STOP().is_match(plain) {
        out.event = Some(ConsoleEvent::Stopping);
        return out;
    }
    if RE_RELOAD().is_match(plain) {
        out.event = Some(ConsoleEvent::Reload);
        return out;
    }
    if plain.contains("for disconnection") || plain.contains("kicked ") || plain.contains("Kicking ") {
        out.event = Some(ConsoleEvent::Kick);
        return out;
    }
    if plain.contains("Whitelisting ") || plain.contains("Un-whitelisting ") || plain.contains("Made ") && plain.contains(" op") {
        out.event = Some(ConsoleEvent::WhitelistChange);
        return out;
    }
    if plain.contains("/ERROR]") || plain.contains("[ERROR]") || plain.contains("SEVERE") {
        out.event = Some(ConsoleEvent::Error);
        return out;
    }
    if plain.contains("/WARN]") || plain.contains("[WARN]") {
        out.event = Some(ConsoleEvent::Warning);
        return out;
    }
    out
}

/// `19.98, 19.99, 20.0` → green/amber/red for the UI gauge.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Health {
    Good,
    Fair,
    Poor,
    Critical,
}

pub fn tps_health(tps: f32) -> Health {
    if tps >= 19.5 {
        Health::Good
    } else if tps >= 18.0 {
        Health::Fair
    } else if tps >= 12.0 {
        Health::Poor
    } else {
        Health::Critical
    }
}

pub fn mspt_health(mspt: f32) -> Health {
    if mspt <= 20.0 {
        Health::Good
    } else if mspt <= 30.0 {
        Health::Fair
    } else if mspt <= 45.0 {
        Health::Poor
    } else {
        Health::Critical
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tps_triple_and_single() {
        let p = parse("[Server thread/INFO]: TPS from last 1m, 5m, 15m: 19.98, 19.99, 20.0");
        assert_eq!(p.tps, Some([19.98, 19.99, 20.0]));
        let p = parse("TPS: 20.0");
        assert_eq!(p.tps.map(|t| t[0]), Some(20.0));
        let p = parse("MSPT from last 1m, 5m, 15m: 12.5, 13.0, 14.2");
        assert_eq!(p.mspt, Some([12.5, 13.0, 14.2]));
        assert_eq!(parse("nothing to see").tps, None);
        assert_eq!(parse("TPS: not-a-number").tps, None);
    }

    #[test]
    fn player_count_lines() {
        let p = parse("There are 7 of a max of 20 players online");
        assert_eq!(p.players, Some((7, 20)));
        let p = parse("There are 0 of a maximum of 100 players online");
        assert_eq!(p.players, Some((0, 100)));
        assert_eq!(parse("players online: 5").players, None);
    }

    #[test]
    fn join_leave_kick() {
        let p = parse("Notch joined the game");
        assert_eq!(p.event, Some(ConsoleEvent::PlayerJoin));
        assert_eq!(p.player.as_deref(), Some("Notch"));
        let p = parse("Steve left the game");
        assert_eq!(p.event, Some(ConsoleEvent::PlayerLeave));
        let p = parse("Herobrine lost connection: Disconnected");
        assert_eq!(p.event, Some(ConsoleEvent::PlayerLeave));
        assert_eq!(p.player.as_deref(), Some("Herobrine"));
    }

    #[test]
    fn boot_and_shutdown_markers() {
        assert_eq!(
            parse("[Server thread/INFO]: Done (14.512s)! For help, type \"help\"").event,
            Some(ConsoleEvent::BootDone)
        );
        assert_eq!(
            parse("[Server thread/INFO]: Stopping server").event,
            Some(ConsoleEvent::Stopping)
        );
        assert_eq!(
            parse("---- Minecraft Crash Report ----").event,
            Some(ConsoleEvent::Crash)
        );
        assert_eq!(
            parse("[Server: ERROR] This crash report has been saved to: /srv/crash.txt").event,
            Some(ConsoleEvent::Crash)
        );
        assert_eq!(
            parse("java.lang.OutOfMemoryError: Java heap space").event,
            Some(ConsoleEvent::OutOfMemory)
        );
    }

    #[test]
    fn saving_and_lag() {
        assert_eq!(
            parse("[Server thread/INFO]: Saving Fields").event,
            Some(ConsoleEvent::SaveStart)
        );
        assert_eq!(
            parse("[Server thread/INFO]: ThreadedAnvilChunkStorage (DIM-1): All dimensions are saved").event,
            Some(ConsoleEvent::SaveDone)
        );
        let p = parse("[Server thread/WARN]: Can't keep up! Is the server overloaded? Running 3545ms or 70 ticks behind");
        assert_eq!(p.lag_ms, Some(3545));
        assert!(matches!(p.event, Some(ConsoleEvent::Warning)));
    }

    #[test]
    fn severity_fallbacks() {
        assert_eq!(
            parse("[Server thread/ERROR]: Could not pass event").event,
            Some(ConsoleEvent::Error)
        );
        assert_eq!(
            parse("[Server thread/WARN]: Something odd").event,
            Some(ConsoleEvent::Warning)
        );
        assert_eq!(parse("[Server thread/INFO]: Hello").event, None);
    }

    #[test]
    fn health_bands() {
        assert_eq!(tps_health(20.0), Health::Good);
        assert_eq!(tps_health(19.6), Health::Good);
        assert_eq!(tps_health(18.5), Health::Fair);
        assert_eq!(tps_health(13.0), Health::Poor);
        assert_eq!(tps_health(4.0), Health::Critical);
        assert_eq!(mspt_health(12.0), Health::Good);
        assert_eq!(mspt_health(25.0), Health::Fair);
        assert_eq!(mspt_health(40.0), Health::Poor);
        assert_eq!(mspt_health(120.0), Health::Critical);
    }

    #[test]
    fn parsing_is_cheap_enough_for_the_hot_path() {
        // A chatty modded server can emit ~10k lines; this must not blow up.
        for i in 0..10_000 {
            let _ = parse(&format!("[Server thread/INFO]: noise line {i}"));
        }
        assert_eq!(parse("world").player, None);
    }
}
