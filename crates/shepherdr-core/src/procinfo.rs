//! macOS-specific process and boot-time lookups.
//!
//! Provides only the OS-side primary information used by [`crate::state`]'s orphan cleanup.
//! Calls `sysctl(3)` and the `libproc.h` interfaces `proc_pidinfo`/`proc_pidpath` directly
//! through `libc`, keeping the dependency to `libc` alone (the structs and constants for both
//! interfaces are already exposed directly by the `libc` crate, so no extra wrapper crate is
//! needed).

use std::ffi::OsString;
use std::os::unix::ffi::OsStringExt as _;
use std::path::PathBuf;
use std::{io, mem, ptr};

use libc::{c_void, proc_bsdinfo, timeval};

/// `mib` for reading `kern.boottime` (`struct timeval`: the time the kernel booted) via
/// `sysctl(2)`.
const BOOTTIME_MIB: [libc::c_int; 2] = [libc::CTL_KERN, libc::KERN_BOOTTIME];

/// A point in time as the kernel reports it: seconds since the Unix epoch, plus a microsecond
/// remainder.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Timestamp {
    pub sec: u64,
    pub usec: u64,
}

impl From<timeval> for Timestamp {
    fn from(value: timeval) -> Self {
        // A `timeval` from `kern.boottime` or `proc_bsdinfo` is always at or after the Unix
        // epoch and always has `0 <= tv_usec < 1_000_000` on a running system.
        #[expect(
            clippy::cast_sign_loss,
            reason = "boot/process start times are always after the Unix epoch"
        )]
        let sec = value.tv_sec as u64;
        #[expect(
            clippy::cast_sign_loss,
            reason = "tv_usec is always within 0..1_000_000"
        )]
        let usec = value.tv_usec as u64;
        Self { sec, usec }
    }
}

/// The identity of a running process, needed to tell apart a pid/pgid reassignment.
pub(crate) struct RunningProcess {
    /// Process group ID (`pbi_pgid`).
    pub pgid: u32,
    /// Start time (`pbi_start_tvsec`/`pbi_start_tvusec`). Fixed at `fork` and unaffected by a
    /// later `exec`.
    pub start_time: Timestamp,
    /// Absolute path to the executable.
    pub exe_path: PathBuf,
}

/// Reads the OS boot time from `kern.boottime`.
///
/// # Errors
///
/// Returns an error when the `sysctl(2)` call fails.
pub(crate) fn boot_time() -> io::Result<Timestamp> {
    let mut mib = BOOTTIME_MIB;
    let mut value: timeval = unsafe { mem::zeroed() };
    let mut size = mem::size_of::<timeval>();
    #[expect(
        clippy::cast_possible_truncation,
        reason = "mib always has exactly 2 elements, far below u32::MAX"
    )]
    let mib_len = mib.len() as u32;
    // SAFETY: `mib` is a valid 2-element mib for `kern.boottime`; `value`/`size` describe an
    // output buffer exactly sized for its `struct timeval` result, and no new value is set
    // (`newp` is null, `newlen` is 0).
    let result = unsafe {
        libc::sysctl(
            mib.as_mut_ptr(),
            mib_len,
            (&raw mut value).cast::<c_void>(),
            &raw mut size,
            ptr::null_mut(),
            0,
        )
    };
    if result != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(value.into())
}

/// Looks up the identity of the process running at `pid`. Returns `None` when there is no such
/// process, or when its information cannot be retrieved. Retrieval failure need not be told
/// apart from "already gone": the cleanup side's response (do not match, do nothing) is the same
/// either way.
pub(crate) fn running_process(pid: i32) -> Option<RunningProcess> {
    let mut info: proc_bsdinfo = unsafe { mem::zeroed() };
    #[expect(
        clippy::cast_possible_truncation,
        clippy::cast_possible_wrap,
        reason = "size_of::<proc_bsdinfo>() is a small compile-time constant, far below i32::MAX"
    )]
    let buffer_size = mem::size_of::<proc_bsdinfo>() as i32;
    // SAFETY: `info` is sized exactly for `PROC_PIDTBSDINFO`'s `struct proc_bsdinfo`.
    let result = unsafe {
        libc::proc_pidinfo(
            pid,
            libc::PROC_PIDTBSDINFO,
            0,
            (&raw mut info).cast::<c_void>(),
            buffer_size,
        )
    };
    // A non-positive return means failure (including "no such process"); see
    // <https://github.com/andrewdavidmackenzie/libproc-rs> for the same convention applied to
    // this same undocumented interface.
    if result <= 0 {
        return None;
    }

    let exe_path = pid_path(pid)?;
    Some(RunningProcess {
        pgid: info.pbi_pgid,
        start_time: Timestamp {
            sec: info.pbi_start_tvsec,
            usec: info.pbi_start_tvusec,
        },
        exe_path,
    })
}

/// Looks up `pid`'s executable path via `proc_pidpath`. Returns `None` when it cannot be
/// retrieved.
fn pid_path(pid: i32) -> Option<PathBuf> {
    // `PROC_PIDPATHINFO_MAXSIZE` is a positive compile-time constant (4096); clippy already
    // const-evaluates the cast below and does not flag it as a possible sign loss.
    let capacity = libc::PROC_PIDPATHINFO_MAXSIZE as usize;
    let mut buffer = vec![0_u8; capacity];
    #[expect(
        clippy::cast_possible_truncation,
        reason = "buffer length is PROC_PIDPATHINFO_MAXSIZE (4096), far below u32::MAX"
    )]
    let buffer_len = buffer.len() as u32;
    // SAFETY: `buffer` is exactly `PROC_PIDPATHINFO_MAXSIZE` bytes, the documented maximum.
    let result =
        unsafe { libc::proc_pidpath(pid, buffer.as_mut_ptr().cast::<c_void>(), buffer_len) };
    if result <= 0 {
        return None;
    }
    #[expect(
        clippy::cast_sign_loss,
        reason = "checked to be positive (and at most buffer.len()) just above"
    )]
    let len = result as usize;
    buffer.truncate(len);
    Some(PathBuf::from(OsString::from_vec(buffer)))
}

#[cfg(test)]
mod tests {
    use std::process;
    use std::time::{SystemTime, UNIX_EPOCH};

    use super::*;

    #[test]
    fn positive_boot_time_returns_a_plausible_past_timestamp() {
        // When the OS boot time is read
        let result = boot_time();

        // Then it succeeds with a timestamp strictly after the epoch and no later than now
        let boot = result.expect("sysctl should succeed");
        let now_secs = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock should be after the epoch")
            .as_secs();
        assert!(boot.sec > 0);
        assert!(boot.sec <= now_secs);
    }

    #[test]
    fn positive_running_process_finds_the_current_process() {
        // Given this test process's own pid
        #[expect(
            clippy::cast_possible_wrap,
            reason = "a real OS pid never approaches i32::MAX, so this never actually wraps"
        )]
        let pid = process::id() as i32;

        // When its process information is queried
        let result = running_process(pid);

        // Then it is found, with an executable path that actually exists on disk
        let found = result.expect("the current process should be found");
        assert!(found.exe_path.is_absolute());
        assert!(found.exe_path.exists());
        assert!(found.pgid > 0);
    }

    #[test]
    fn negative_running_process_returns_none_for_a_pid_that_does_not_exist() {
        // Given a pid that cannot correspond to any running process
        let pid = i32::MAX;

        // When its process information is queried
        let result = running_process(pid);

        // Then nothing is found
        assert!(result.is_none());
    }
}
