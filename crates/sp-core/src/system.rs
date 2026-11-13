//! Portable, dependency-light system helpers.
//!
//! Everything here must be safe to call on a UI thread: no async, no blocking
//! syscalls longer than a `bind()`. Richer probes (per-process CPU, disk stats)
//! live in `sp-server::services::monitor`, which uses `sysinfo` and runs on a
//! spawn_blocking cadence.

use crate::error::CoreError;
use globset::{Glob, GlobSet, GlobSetBuilder};
use ipnet::Ipv4Net;
use std::io::ErrorKind;
use std::net::{IpAddr, Ipv4Addr, SocketAddr, TcpListener};
use std::path::Path;

/// Logical CPUs available to the panel (respects cgroup/affinity limits).
pub fn cpu_count() -> usize {
    std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(1)
}

/// True when `addr` can be bound right now.
///
/// We bind with the *same* address the server would use, because "port free on
/// 0.0.0.0" and "port free on 127.0.0.1" are different questions.
pub fn is_port_free(addr: SocketAddr) -> bool {
    match TcpListener::bind(addr) {
        Ok(_l) => true,
        Err(e) if matches!(e.kind(), ErrorKind::AddrInUse | ErrorKind::PermissionDenied) => false,
        // Any other error (e.g. the address is not local) is not a conflict —
        // report free and let the real bind produce the authoritative error.
        Err(_) => true,
    }
}

/// Find a free port at or after `start`, scanning up to `span` ports.
pub fn first_free_port(bind: IpAddr, start: u16, span: u16) -> Option<u16> {
    (0..span).find_map(|delta| {
        let port = start.checked_add(delta)?;
        is_port_free(SocketAddr::new(bind, port)).then_some(port)
    })
}

/// Errors out with an actionable message rather than silently picking a port.
pub fn require_free_port(bind: IpAddr, port: u16) -> Result<(), CoreError> {
    if is_port_free(SocketAddr::new(bind, port)) {
        Ok(())
    } else {
        Err(CoreError::conflict(format!(
            "port {port} is already in use on {bind} — pick another, or stop the program holding it"
        )))
    }
}

/// IPv4 networks attached to this host, used to explain "who can reach me".
///
/// `if-addrs` is used by `sp-server` for the interface list; here we only need
/// the "is this address in my LAN" question for the network page, which we answer
/// from the probe socket below (no dependency, no enumeration).
/// Best-guess "the address other machines would reach me on".
pub fn primary_local_ip() -> Option<IpAddr> {
    // Opening a UDP socket does not send anything; it just makes the kernel pick
    // the source address for the default route.
    let socket = std::net::UdpSocket::bind("0.0.0.0:0").ok()?;
    socket.connect("1.1.1.1:80").ok()?;
    match socket.local_addr().ok()?.ip() {
        IpAddr::V4(v4) if !v4.is_loopback() => Some(IpAddr::V4(v4)),
        IpAddr::V6(v6) if !v6.is_loopback() => Some(IpAddr::V6(v6)),
        _ => None,
    }
}

/// Does `ip` fall inside `cidr` (e.g. `192.168.1.0/24`)?
pub fn in_subnet(ip: Ipv4Addr, cidr: &str) -> bool {
    match cidr.parse::<Ipv4Net>() {
        Ok(net) => net.contains(&ip),
        Err(_) => false,
    }
}

/// `is_local` is what gates the "no login needed from this machine" affordances.
pub fn is_loopback(ip: &IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => v4.is_loopback(),
        IpAddr::V6(v6) => v6.is_loopback(),
    }
}

/// Reserved / non-routable targets a "connect" action must refuse.
pub fn is_private_or_reserved(ip: &IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => {
            v4.is_loopback()
                || v4.is_private()
                || v4.is_link_local()
                || v4.is_unspecified()
                || v4.is_broadcast()
                || v4.is_documentation()
                || v4.is_multicast()
        }
        IpAddr::V6(v6) => v6.is_loopback() || v6.is_unspecified() || v6.is_unicast_link_local(),
    }
}

/// Recursive directory size, skipping symlinks (never follows them).
pub fn dir_size(path: &Path) -> u64 {
    let mut total = 0u64;
    for entry in walkdir::WalkDir::new(path)
        .follow_links(false)
        .max_depth(64)
        .into_iter()
        .filter_map(|e| e.ok())
    {
        if entry.file_type().is_file() {
            total = total.saturating_add(
                entry
                    .metadata()
                    .map(|m| m.len())
                    .unwrap_or(0),
            );
        }
    }
    total
}

/// Compile a list of shell-style ignore patterns (`logs/*.gz`, `**/*.tmp`) into a
/// matcher. Empty input yields a matcher that never matches.
pub fn glob_matcher(patterns: &[String]) -> Result<Option<GlobSet>, CoreError> {
    if patterns.is_empty() {
        return Ok(None);
    }
    let mut builder = GlobSetBuilder::new();
    for p in patterns {
        for glob in normalise_globs(p) {
            let g = Glob::new(&glob).map_err(|e| {
                CoreError::validation("exclude_patterns", format!("`{p}` is not a valid pattern: {e}"))
            })?;
            builder.add(g);
        }
    }
    builder.build().map(Some).map_err(|e| CoreError::other(e.to_string()))
}

/// A backup exclusion is a *list* of patterns, and users type them in several
/// dialects. We expand each pattern so all of them behave as expected:
///
/// * `crash-reports` → that directory *and* everything under it, at any depth
/// * `logs/*.gz`     → itself and the same shape nested one level down
/// * `world/`        → the trailing slash explicitly means "directory contents"
/// * `**/*.tmp`      → left alone (already absolute in glob terms)
fn normalise_globs(pattern: &str) -> Vec<String> {
    let raw = pattern.trim().replace('\\', "/");
    let raw = raw.trim_start_matches("./").to_string();
    let dir_explicit = raw.ends_with('/');
    let p = raw.trim_end_matches('/').to_string();
    if p.is_empty() {
        return Vec::new();
    }
    let mut out = Vec::with_capacity(4);
    if p.contains("**") {
        out.push(p.clone());
        return out;
    }
    if p.contains('/') {
        out.push(p.clone());
        out.push(format!("**/{p}"));
    } else {
        out.push(format!("**/{p}"));
    }
    // A pattern naming a directory excludes its whole subtree.
    let names_a_dir = dir_explicit || !p.contains('.') && !p.contains('*') && !p.contains('?');
    if names_a_dir {
        out.push(format!("**/{p}/**"));
        if p.contains('/') {
            out.push(format!("{p}/**"));
        }
    }
    out
}

/// `1234567` → `1.2 MB` (binary units, one decimal, no space before the unit is
/// intentional so the UI can render it in a monospaced column).
pub fn format_bytes(bytes: u64) -> String {
    const UNITS: [&str; 6] = ["B", "KiB", "MiB", "GiB", "TiB", "PiB"];
    let mut v = bytes as f64;
    let mut i = 0;
    while v >= 1024.0 && i + 1 < UNITS.len() {
        v /= 1024.0;
        i += 1;
    }
    if i == 0 {
        format!("{bytes} B")
    } else if v >= 100.0 {
        format!("{v:.0} {}", UNITS[i])
    } else {
        format!("{v:.1} {}", UNITS[i])
    }
}

/// `75` seconds → `1m 15s`; used in CLI output and notification text.
pub fn format_duration(secs: u64) -> String {
    if secs < 60 {
        return format!("{secs}s");
    }
    let d = secs / 86_400;
    let h = (secs % 86_400) / 3_600;
    let m = (secs % 3_600) / 60;
    let s = secs % 60;
    let mut out = String::new();
    if d > 0 {
        out.push_str(&format!("{d}d "));
    }
    if d > 0 || h > 0 {
        out.push_str(&format!("{h}h "));
    }
    if d == 0 {
        out.push_str(&format!("{m}m {s}s"));
    }
    out.trim().to_string()
}

/// Percentage with clamping, for the UI's gauges.
pub fn percent(part: u64, total: u64) -> f64 {
    if total == 0 {
        0.0
    } else {
        ((part as f64 / total as f64) * 100.0).clamp(0.0, 100.0)
    }
}

/// Bind a listener for the panel itself, honouring `port_auto_increment`.
pub fn bind_listener(bind: IpAddr, port: u16, auto_increment: bool) -> Result<(TcpListener, u16), CoreError> {
    let mut last_err: Option<std::io::Error> = None;
    for delta in 0..if auto_increment { 20 } else { 1 } {
        let Some(p) = port.checked_add(delta) else { break };
        match TcpListener::bind(SocketAddr::new(bind, p)) {
            Ok(l) => return Ok((l, p)),
            Err(e) => last_err = Some(e),
        }
    }
    Err(CoreError::Io(last_err.unwrap_or_else(|| {
        std::io::Error::new(ErrorKind::AddrInUse, "no free port found")
    })))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::{Ipv4Addr, Ipv6Addr};

    #[test]
    fn cpu_count_is_sane() {
        assert!(cpu_count() >= 1);
    }

    #[test]
    fn bound_port_is_not_free() {
        let l = TcpListener::bind(SocketAddr::from((Ipv4Addr::LOCALHOST, 0))).unwrap();
        let addr = l.local_addr().unwrap();
        assert!(!is_port_free(addr), "an open listener must read as a conflict");
        drop(l);
        assert!(is_port_free(addr), "after drop the port is free again");
    }

    #[test]
    fn port_scan_finds_a_gap() {
        let l = TcpListener::bind(SocketAddr::from((Ipv4Addr::LOCALHOST, 0))).unwrap();
        let taken = l.local_addr().unwrap().port();
        let found = first_free_port(IpAddr::V4(Ipv4Addr::LOCALHOST), taken, 16).unwrap();
        assert!(found > taken && found <= taken + 16, "{taken} → {found}");
        assert!(require_free_port(IpAddr::V4(Ipv4Addr::LOCALHOST), taken).is_err());
        assert!(require_free_port(IpAddr::V4(Ipv4Addr::LOCALHOST), found).is_ok());
    }

    #[test]
    fn port_scan_gives_up_gracefully() {
        assert!(first_free_port(IpAddr::V4(Ipv4Addr::LOCALHOST), u16::MAX, 1).is_some());
        // overflowing past 65535 must not panic
        assert!(first_free_port(IpAddr::V4(Ipv4Addr::LOCALHOST), 65_530, 100).is_some());
    }

    #[test]
    fn listener_binds_and_increments() {
        let (l, port) = bind_listener(IpAddr::V4(Ipv4Addr::LOCALHOST), 0, false).unwrap();
        assert!(port > 0);
        drop(l);
        // Occupy a port, then ask for it with auto-increment.
        let held = TcpListener::bind(SocketAddr::from((Ipv4Addr::LOCALHOST, 0))).unwrap();
        let held_port = held.local_addr().unwrap().port();
        let (l2, p2) = bind_listener(IpAddr::V4(Ipv4Addr::LOCALHOST), held_port, true).unwrap();
        assert!(p2 > held_port, "{held_port} was busy so we must move up");
        drop(l2);
        assert!(bind_listener(IpAddr::V4(Ipv4Addr::LOCALHOST), held_port, false).is_err());
    }

    #[test]
    fn subnet_membership() {
        let ip = Ipv4Addr::new(192, 168, 1, 42);
        assert!(in_subnet(ip, "192.168.1.0/24"));
        assert!(!in_subnet(ip, "10.0.0.0/8"));
        assert!(!in_subnet(ip, "garbage"), "bad CIDR is simply not a match");
    }

    #[test]
    fn address_classification() {
        assert!(is_loopback(&IpAddr::V4(Ipv4Addr::LOCALHOST)));
        assert!(is_loopback(&IpAddr::V6(Ipv6Addr::LOCALHOST)));
        assert!(!is_loopback(&IpAddr::V4(Ipv4Addr::new(192, 168, 0, 1))));
        assert!(is_private_or_reserved(&IpAddr::V4(Ipv4Addr::new(10, 1, 2, 3))));
        assert!(is_private_or_reserved(&IpAddr::V4(Ipv4Addr::new(169, 254, 1, 1))));
        assert!(!is_private_or_reserved(&IpAddr::V4(Ipv4Addr::new(1, 1, 1, 1))));
    }

    #[test]
    fn dir_size_skips_symlinks_and_counts_files() {
        let t = tempfile::tempdir().unwrap();
        std::fs::write(t.path().join("a.txt"), b"12345").unwrap();
        std::fs::create_dir(t.path().join("sub")).unwrap();
        std::fs::write(t.path().join("sub/b.bin"), vec![0u8; 10]).unwrap();
        assert_eq!(dir_size(t.path()), 15);
        #[cfg(unix)]
        {
            // A symlink to a huge file must not be followed or counted.
            std::os::unix::fs::symlink("/dev/zero", t.path().join("link")).unwrap();
            assert_eq!(dir_size(t.path()), 15);
        }
        assert_eq!(dir_size(Path::new("/nonexistent-path-xyz")), 0);
    }

    #[test]
    fn glob_matcher_compiles_patterns() {
        let m = glob_matcher(&["logs/*.gz".into(), "*.tmp".into(), "crash-reports".into()])
            .unwrap()
            .unwrap();
        assert!(m.is_match("logs/2026-01-01-1.log.gz"));
        assert!(m.is_match("plugins/a.tmp"));
        assert!(m.is_match("crash-reports/crash.txt"));
        assert!(!m.is_match("server.properties"));
        assert!(!m.is_match("world/level.dat"));
        assert!(glob_matcher(&[]).unwrap().is_none(), "no patterns → no matcher");
        assert!(glob_matcher(&["[".into()]).is_err(), "invalid glob is a validation error");
        let dirs = glob_matcher(&["world/".into()]).unwrap().unwrap();
        assert!(dirs.is_match("world/level.dat"));
        assert!(dirs.is_match("world/region/r.0.0.mca"));
        assert!(!dirs.is_match("world_nether/level.dat"));
        assert!(glob_matcher(&["".into(), "  ".into()]).unwrap().is_some());
    }

    #[test]
    fn formatting_is_stable() {
        assert_eq!(format_bytes(0), "0 B");
        assert_eq!(format_bytes(999), "999 B");
        assert_eq!(format_bytes(1024), "1.0 KiB");
        assert_eq!(format_bytes(1536), "1.5 KiB");
        assert_eq!(format_bytes(3 * 1024 * 1024 * 1024), "3.0 GiB");
        assert_eq!(format_bytes(123 * 1024 * 1024), "123 MiB", "wide values drop the decimal");
        assert_eq!(format_duration(0), "0s");
        assert_eq!(format_duration(59), "59s");
        assert_eq!(format_duration(75), "1m 15s");
        assert_eq!(format_duration(3_600), "1h 0m 0s");
        assert_eq!(format_duration(5_401), "1h 30m 1s");
        assert_eq!(format_duration(90_000), "1d 1h");
    }

    #[test]
    fn percent_clamps() {
        assert_eq!(percent(50, 100), 50.0);
        assert_eq!(percent(0, 0), 0.0);
        assert_eq!(percent(200, 100), 100.0);
    }
}
