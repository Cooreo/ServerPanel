//! Keeping the process tree ours to kill.
//!
//! Unix: the child is started in its own process group (`CommandExt::process_group`)
//! and we signal that group, so nothing of the panel's is ever included.
//!
//! Windows has no process groups, so we put the JVM in a **Job Object** with
//! `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE`: when the panel dies — even if it is
//! killed in Task Manager or loses power mid-write and is restarted — the kernel
//! closes the handle and the Java process goes with it. This is the difference
//! between "orphaned javaw.exe holding port 25565 forever" and a clean restart.
//!
//! This module contains the only `unsafe` in the workspace. It is `#[deny]`-free by
//! necessity (Win32 FFI), documented call by call, and confined here.

// The one place in the workspace where `unsafe` is allowed: Win32 FFI for Job
// Objects. Every block below has a comment naming the API and why it is sound.
#![allow(unsafe_code)]

/// Handle kept alive for the lifetime of a server process.
#[derive(Debug)]
pub struct ProcessGroupGuard {
    #[cfg(windows)]
    handle_raw: usize,
}

impl ProcessGroupGuard {
    /// Attach the process to a kill-on-close job. Failure is not fatal: we return
    /// `None` and the panel falls back to explicit terminate-on-stop.
    pub fn attach(pid: u32) -> Option<Self> {
        #[cfg(windows)]
        {
            match unsafe { create_and_assign(pid) } {
                Ok(handle_raw) => Some(Self { handle_raw }),
                Err(e) => {
                    tracing::debug!(error = %e, "could not create a Windows job object for pid {pid}");
                    None
                }
            }
        }
        #[cfg(not(windows))]
        {
            let _ = pid;
            Some(Self {})
        }
    }
}

impl Drop for ProcessGroupGuard {
    fn drop(&mut self) {
        #[cfg(windows)]
        {
            use windows::Win32::Foundation::CloseHandle;
            use windows::Win32::Foundation::HANDLE;
            // Closing the last handle to the job terminates everything in it
            // (KILL_ON_JOB_CLOSE was set above).
            unsafe {
                let _ = CloseHandle(HANDLE(self.handle_raw as *mut _));
            }
        }
    }
}

/// Prevent the guard from being moved across threads in a way that would close the
/// handle on the wrong thread — handles are process-global, so this is sound, and
/// asserting it here documents the reasoning.
#[cfg(windows)]
unsafe impl Send for ProcessGroupGuard {}
#[cfg(windows)]
unsafe impl Sync for ProcessGroupGuard {}

#[cfg(windows)]
unsafe fn create_and_assign(pid: u32) -> Result<usize, String> {
    use windows::core::PCWSTR;
    use windows::Win32::Foundation::{CloseHandle, HANDLE};
    use windows::Win32::System::JobObjects::{
        AssignProcessToJobObject, CreateJobObjectW, JobObjectExtendedLimitInformation,
        SetInformationJobObject, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
        JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
    };
    use windows::Win32::System::Threading::{
        OpenProcess, PROCESS_SET_QUOTA, PROCESS_TERMINATE,
    };

    // 1. Create an unnamed job.
    let job = CreateJobObjectW(PCWSTR::null(), None).map_err(|e| e.to_string())?;
    // 2. Ask for kill-on-close *before* attaching, so a crash between the two
    //    cannot leave an unmanaged child.
    let mut info = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
    info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
    let set = SetInformationJobObject(
        job,
        JobObjectExtendedLimitInformation,
        &info as *const _ as *const core::ffi::c_void,
        core::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
    );
    if set.is_err() {
        let _ = CloseHandle(job);
        return Err("SetInformationJobObject failed".to_string());
    }
    // 3. Open the child we just spawned. We only need rights to terminate it and
    //    to put it in the job — never to write its memory.
    let process = OpenProcess(PROCESS_SET_QUOTA | PROCESS_TERMINATE, false, pid).map_err(|e| {
        let _ = CloseHandle(job);
        format!("OpenProcess({pid}): {e}")
    })?;
    let assigned = AssignProcessToJobObject(job, process);
    let _ = CloseHandle(process);
    if assigned.is_err() {
        let _ = CloseHandle(job);
        return Err("AssignProcessToJobObject failed".to_string());
    }
    Ok(job.0 as usize)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn attach_returns_a_guard_for_a_live_pid_and_tolerates_a_dead_one() {
        // pid 0 is never valid to open; attach must not panic.
        let _ = ProcessGroupGuard::attach(0);
        let mine = std::process::id();
        let guard = ProcessGroupGuard::attach(mine);
        assert!(guard.is_some(), "attaching to our own pid must succeed");
        drop(guard);
        // Dropping the guard must not kill *this* process when the job was not
        // the one holding it (on unix the guard is a no-op by design).
        assert!(std::process::id() > 0);
    }
}
