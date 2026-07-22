//! Stopping a service's process group: `SIGTERM`, escalating to `SIGKILL` after a grace period.

use std::io;
use std::process::ExitStatus;
use std::time::Duration;

use nix::sys::signal::{Signal, killpg};
use nix::unistd::Pid;
use thiserror::Error;
use tokio::process::Child;
use tokio::time::timeout;

/// Grace period between `SIGTERM` and the follow-up `SIGKILL`. Fallback for `[stop]`'s
/// `grace_period` when that section or field is omitted.
pub const DEFAULT_GRACE_PERIOD: Duration = Duration::from_secs(10);

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
}
