//! Stopping a service's process group: `SIGTERM`, escalating to `SIGKILL` after a grace period.

use std::io;
use std::process::ExitStatus;
use std::time::Duration;

use nix::sys::signal::{Signal, kill, killpg};
use nix::unistd::Pid;
use thiserror::Error;
use tokio::process::Child;
use tokio::time::{Instant, sleep, timeout};

/// Grace period between `SIGTERM` and the follow-up `SIGKILL`. Fallback for `[stop]`'s
/// `grace_period` when that section or field is omitted.
pub const DEFAULT_GRACE_PERIOD: Duration = Duration::from_secs(10);

/// Polling interval used by [`stop_orphan`] while waiting for a process it does not own (and so
/// cannot `wait` on) to disappear. Short enough to notice an exit promptly without spinning the
/// loop needlessly.
const ORPHAN_POLL_INTERVAL: Duration = Duration::from_millis(50);

/// Errors raised while stopping a service.
#[derive(Debug, Error)]
pub enum StopError {
    /// Sending a signal to the service's process group failed.
    #[error("failed to send {signal} to process group {pgid}")]
    Signal {
        /// The signal that could not be sent.
        signal: Signal,
        /// The target process group ID.
        pgid: i32,
        /// The underlying errno.
        #[source]
        source: nix::Error,
    },
    /// Waiting for the child process to exit failed.
    #[error("failed to wait for the child process to exit")]
    Wait(#[source] io::Error),
    /// An orphaned process group was still alive after the post-`SIGKILL` grace period.
    #[error("orphaned process group {pgid} did not terminate")]
    OrphanNotTerminated {
        /// The process group ID that failed to terminate.
        pgid: i32,
    },
}

/// Runs the stop sequence for one service's child process.
///
/// Sends `SIGTERM` to `child`'s process group; if it has not exited within `grace_period`, sends
/// `SIGKILL` to the group and waits for `child` to exit.
///
/// # Errors
///
/// Returns an error when a signal cannot be sent to the process group, or when waiting for the
/// child fails.
pub async fn stop(child: &mut Child, grace_period: Duration) -> Result<ExitStatus, StopError> {
    let Some(pgid) = pgid_of(child) else {
        return child.wait().await.map_err(StopError::Wait);
    };

    send_signal(pgid, Signal::SIGTERM)?;
    if let Ok(result) = timeout(grace_period, child.wait()).await {
        return result.map_err(StopError::Wait);
    }

    send_signal(pgid, Signal::SIGKILL)?;
    child.wait().await.map_err(StopError::Wait)
}

/// Runs the stop sequence against an orphaned process group this app never `fork`ed as its own
/// child (see [`crate::state::cleanup`]), identified only by `pgid` — equal to the leading
/// process's pid, per the spawn contract in [`crate::spawn::spawn`].
///
/// Since the app is not this process's parent, it cannot `wait` on it; exit is instead detected
/// by polling whether the leading process still exists (`kill` with no signal: error checking
/// only, nothing delivered), at [`ORPHAN_POLL_INTERVAL`], until `grace_period` elapses. As in
/// [`stop`], `SIGTERM` is sent first; if the process is still alive once `grace_period` elapses,
/// `SIGKILL` follows, bounded by another `grace_period` to confirm it took effect (`SIGKILL`
/// cannot be caught or ignored, but this app still has no blocking way to observe it besides
/// polling).
///
/// Returns immediately, without sending any signal, if the process is already gone.
///
/// # Errors
///
/// Returns an error when a signal cannot be sent to the process group, or when the leading
/// process is still alive after the post-`SIGKILL` grace period elapses.
pub async fn stop_orphan(pgid: Pid, grace_period: Duration) -> Result<(), StopError> {
    if !process_exists(pgid) {
        return Ok(());
    }

    send_signal(pgid, Signal::SIGTERM)?;
    if wait_for_exit(pgid, grace_period).await {
        return Ok(());
    }

    send_signal(pgid, Signal::SIGKILL)?;
    if wait_for_exit(pgid, grace_period).await {
        return Ok(());
    }
    Err(StopError::OrphanNotTerminated {
        pgid: pgid.as_raw(),
    })
}

/// Polls until the process at `pid` disappears or `grace_period` elapses. Returns whether it
/// disappeared in time.
async fn wait_for_exit(pid: Pid, grace_period: Duration) -> bool {
    let deadline = Instant::now() + grace_period;
    loop {
        if !process_exists(pid) {
            return true;
        }
        let Some(remaining) = deadline.checked_duration_since(Instant::now()) else {
            return false;
        };
        sleep(ORPHAN_POLL_INTERVAL.min(remaining)).await;
    }
}

/// Whether a process exists at `pid`, via the existence-check form of `kill` (no signal is
/// actually delivered). Any error, including a permission failure, is treated as "gone": this
/// app's services all run as its own user, so a permission error against one of them would
/// itself be unexpected.
fn process_exists(pid: Pid) -> bool {
    kill(pid, None).is_ok()
}

/// The process group ID led by `child` (each service is spawned with `process_group(0)`, so the
/// group ID equals the child's PID), or `None` if it has already exited.
fn pgid_of(child: &Child) -> Option<Pid> {
    child.id().map(|pid| {
        #[expect(
            clippy::cast_possible_wrap,
            reason = "a real OS pid never approaches i32::MAX, so this never actually wraps"
        )]
        let pid = pid as i32;
        Pid::from_raw(pid)
    })
}

/// Sends `signal` to the process group `pgid`.
fn send_signal(pgid: Pid, signal: Signal) -> Result<(), StopError> {
    killpg(pgid, signal).map_err(|source| StopError::Signal {
        signal,
        pgid: pgid.as_raw(),
        source,
    })
}

#[cfg(test)]
mod tests {
    use std::os::unix::process::ExitStatusExt as _;
    use std::process::Stdio;
    use std::time::Duration;

    use rustc_hash::FxHashMap;
    use tokio::process::Command;
    use tokio::time::sleep;

    use super::*;
    use crate::config::Service;
    use crate::spawn;

    /// A short grace period so grace-period-elapsed tests do not slow down the suite.
    const SHORT_GRACE_PERIOD: Duration = Duration::from_millis(200);

    fn service(command: &[&str]) -> Service {
        Service {
            name: "test".to_owned(),
            command: command.iter().map(|&s| s.to_owned()).collect(),
            login_shell: false,
            env: FxHashMap::default(),
            cwd: None,
            enabled: true,
        }
    }

    #[tokio::test]
    async fn positive_stop_returns_promptly_when_the_child_exits_on_sigterm() {
        // Given a spawned child that exits (via the default SIGTERM disposition) well within
        // the grace period
        let svc = service(&["sleep", "30"]);
        let mut child = spawn::spawn(&svc).expect("spawn should succeed");

        // When it is stopped
        let result = stop(&mut child, Duration::from_secs(5)).await;

        // Then it exits terminated by SIGTERM
        let status = result.expect("stop should succeed");
        assert_eq!(status.signal(), Some(Signal::SIGTERM as i32));
    }

    #[tokio::test]
    async fn positive_stop_escalates_to_sigkill_when_the_child_ignores_sigterm() {
        // Given a spawned child that ignores SIGTERM. The short sleep gives the shell time to
        // actually execute the `trap` builtin before SIGTERM is sent; without it, the SIGTERM
        // can race the shell's startup and hit it while the signal is still at its default
        // (terminating) disposition.
        let svc = service(&["sh", "-c", "trap '' TERM; sleep 30"]);
        let mut child = spawn::spawn(&svc).expect("spawn should succeed");
        sleep(Duration::from_millis(200)).await;

        // When it is stopped with a short grace period
        let result = stop(&mut child, SHORT_GRACE_PERIOD).await;

        // Then the grace period elapses and it is terminated by the follow-up SIGKILL
        let status = result.expect("stop should succeed");
        assert_eq!(status.signal(), Some(Signal::SIGKILL as i32));
    }

    #[tokio::test]
    async fn negative_send_signal_fails_when_the_process_group_does_not_exist() {
        // Given a pid that cannot correspond to any running process group
        let pgid = Pid::from_raw(i32::MAX);

        // When a signal is sent to it
        let result = send_signal(pgid, Signal::SIGTERM);

        // Then it fails identifying the signal and the target pgid
        assert!(matches!(
            result,
            Err(StopError::Signal {
                signal: Signal::SIGTERM,
                pgid: i32::MAX,
                ..
            })
        ));
    }

    #[tokio::test]
    async fn positive_stop_waits_for_an_already_exited_child_without_signaling() {
        // Given a child that has already exited and been reaped
        let mut child = Command::new("true")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn should succeed");
        child.wait().await.expect("child should run");

        // When it is stopped
        let result = stop(&mut child, SHORT_GRACE_PERIOD).await;

        // Then it succeeds without attempting to signal a pid that no longer resolves
        let status = result.expect("stop should succeed");
        assert!(status.success());
    }

    #[tokio::test]
    async fn positive_stop_orphan_returns_immediately_when_the_process_is_already_gone() {
        // Given a pgid that cannot correspond to any running process group
        let pgid = Pid::from_raw(i32::MAX);

        // When the orphan stop sequence runs against it
        let result = stop_orphan(pgid, SHORT_GRACE_PERIOD).await;

        // Then it succeeds immediately, without attempting to signal a nonexistent group
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn positive_stop_orphan_terminates_promptly_when_the_process_exits_on_sigterm() {
        // Given a process spawned in its own group, not owned as this test's child in the
        // `stop_orphan` sense; something else (standing in for launchd reaping a real orphan)
        // reaps it once it exits
        let svc = service(&["sleep", "30"]);
        let mut child = spawn::spawn(&svc).expect("spawn should succeed");
        let pgid = Pid::from_raw(
            i32::try_from(child.id().expect("child should still be running"))
                .expect("pid fits in i32"),
        );
        tokio::spawn(async move {
            let _: io::Result<ExitStatus> = child.wait().await;
        });

        // When the orphan stop sequence runs against its pgid
        let result = stop_orphan(pgid, Duration::from_secs(5)).await;

        // Then it succeeds via the default SIGTERM disposition, without needing SIGKILL
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn positive_stop_orphan_escalates_to_sigkill_when_the_process_ignores_sigterm() {
        // Given a process, spawned in its own group, that ignores SIGTERM; something else
        // (standing in for launchd reaping a real orphan) reaps it once it exits
        let svc = service(&["sh", "-c", "trap '' TERM; sleep 30"]);
        let mut child = spawn::spawn(&svc).expect("spawn should succeed");
        let pgid = Pid::from_raw(
            i32::try_from(child.id().expect("child should still be running"))
                .expect("pid fits in i32"),
        );
        sleep(Duration::from_millis(200)).await;
        tokio::spawn(async move {
            let _: io::Result<ExitStatus> = child.wait().await;
        });

        // When the orphan stop sequence runs against its pgid with a short grace period
        let result = stop_orphan(pgid, SHORT_GRACE_PERIOD).await;

        // Then the grace period elapses and it is terminated by the follow-up SIGKILL
        assert!(result.is_ok());
    }
}
