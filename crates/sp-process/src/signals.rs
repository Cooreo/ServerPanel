//! Signal delivery for the whole process group.
//!
//! On Unix a Minecraft server can leave children behind (a BuildTools run, a
//! restart wrapper, `spark` collectors), so the panel signals `-pgid` — the group
//! created by `process_group(0)` at spawn — instead of the single pid.
//! `ESRCH` is treated as success: the process is already gone, which is the goal.

/// SIGTERM to the group. No-op on Windows (there is no SIGTERM; the job object is
/// the escalation path there).
#[cfg(unix)]
pub fn term_process_group(pid: u32) -> Result<(), std::io::Error> {
    use nix::sys::signal::Signal;
    match nix::sys::signal::killpg(nix::unistd::Pid::from_raw(pid as i32), Signal::SIGTERM) {
        Ok(()) => Ok(()),
        Err(nix::errno::Errno::ESRCH) => Ok(()),
        Err(e) => Err(std::io::Error::from_raw_os_error(e as i32)),
    }
}

#[cfg(not(unix))]
pub fn term_process_group(_pid: u32) -> Result<(), std::io::Error> {
    Ok(())
}

/// SIGKILL to the group.
#[cfg(unix)]
pub fn kill_process_group(pid: u32) -> Result<(), std::io::Error> {
    use nix::sys::signal::Signal;
    match nix::sys::signal::killpg(nix::unistd::Pid::from_raw(pid as i32), Signal::SIGKILL) {
        Ok(()) => Ok(()),
        Err(nix::errno::Errno::ESRCH) => Ok(()),
        Err(e) => Err(std::io::Error::from_raw_os_error(e as i32)),
    }
}

#[cfg(not(unix))]
pub fn kill_process_group(_pid: u32) -> Result<(), std::io::Error> {
    Ok(())
}

/// Is `pid` still alive (used by the "orphan sweep" at startup).
#[cfg(unix)]
pub fn is_alive(pid: u32) -> bool {
    use nix::sys::signal::Signal;
    nix::sys::signal::kill(nix::unistd::Pid::from_raw(pid as i32), Signal::SIG0).is_ok()
}

#[cfg(not(unix))]
pub fn is_alive(pid: u32) -> bool {
    // On Windows we never keep orphan pids around (the job object reaps them), so
    // "unknown" is reported as dead and the panel simply forgets the record.
    let _ = pid;
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn signalling_nobody_is_not_an_error() {
        // pid 0 means "the process group of the caller" for kill(2) on some
        // platforms, so use an absurd pid that cannot exist.
        let huge = u32::MAX / 2;
        assert!(term_process_group(huge).is_ok(), "ESRCH must be swallowed");
        assert!(kill_process_group(huge).is_ok());
        assert!(!is_alive(huge));
    }

    #[test]
    fn our_own_pid_is_alive() {
        assert!(is_alive(std::process::id()));
    }
}
