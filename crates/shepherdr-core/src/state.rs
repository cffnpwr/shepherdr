//! State management for orphan-process cleanup.
//!
//! [`record`] and [`forget`] correspond, from the caller's perspective, to the start and end of
//! one service's lifetime. Call [`record`] once, immediately after a successful `spawn::spawn`,
//! and call [`forget`] once, after the process has actually exited through a normal stop (a tray
//! operation, a reload, or app shutdown). Entries left behind because neither was called (the
//! app itself terminated abnormally) are processed once, at the next startup, by [`cleanup`].
//! How a monitor loop or orchestrator wires this up is outside this module's responsibility;
//! this only provides the pure recording/matching/cleanup API.

use std::path::{Path, PathBuf};
use std::time::Duration;
use std::{env, fs, io};

use nix::unistd::Pid;
use rustc_hash::FxHashMap;
use serde::{Deserialize, Serialize};
use thiserror::Error;
use tokio::process::Child;
use toml::ser;

use crate::procinfo;
use crate::stop::{self, StopError};

/// State file name, under `~/Library/Application Support/shepherdr/`.
const STATE_FILE_NAME: &str = "state.toml";

/// One service's process-group identity, as recorded in the state file.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct RecordedProcess {
    /// Process group ID. `spawn::spawn` always leads its own process group
    /// (`process_group(0)`), so this equals the leading process's pid, and is the pid used to
    /// re-query it during cleanup.
    pgid: u32,
    /// The leading process's start time (seconds). Fixed at `fork` and unaffected by a later
    /// `exec`.
    start_time_sec: u64,
    /// Start time's microsecond remainder.
    start_time_usec: u64,
    /// The executable's absolute path at recording time. Kept for diagnostics only; not used
    /// when matching against a currently running process during cleanup (see [`cleanup_one`]).
    exe_path: PathBuf,
}

/// The entire contents of the state file.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct State {
    /// OS boot time (seconds) at the time this content was written.
    boot_time_sec: u64,
    /// Boot time's microsecond remainder.
    boot_time_usec: u64,
    /// Recorded processes, keyed by service name.
    #[serde(default)]
    services: FxHashMap<String, RecordedProcess>,
}

/// Errors raised while recording or reading orphan-cleanup state.
#[derive(Debug, Error)]
pub enum StateError {
    /// The home directory could not be resolved.
    #[error("failed to resolve the home directory")]
    HomeDirNotFound,
    /// The OS boot time could not be read.
    #[error("failed to read the OS boot time")]
    BootTime(#[source] io::Error),
    /// The state could not be serialized to TOML.
    #[error("failed to serialize the state file")]
    Serialize(#[source] ser::Error),
    /// Writing or removing the state file failed.
    #[error("failed to update the state file: {path}")]
    Persist {
        /// The path being written or removed.
        path: PathBuf,
        /// The underlying I/O error.
        #[source]
        source: io::Error,
    },
}

/// The cleanup outcome [`cleanup`] reached for one recorded service.
#[derive(Debug)]
pub enum ServiceCleanup {
    /// The process currently holding the recorded pgid no longer matches the recorded start
    /// time (already exited, or the pid/pgid was reused by an unrelated process): left
    /// untouched.
    NotMatched,
    /// Matched, and the cleanup stop sequence terminated it.
    Stopped,
    /// Matched, but the cleanup stop sequence failed to terminate it.
    StopFailed(StopError),
}

/// The outcome of one [`cleanup`] run.
#[derive(Debug)]
pub enum CleanupResult {
    /// No usable state (the file was missing, unreadable, or did not parse as this crate's
    /// schema). Nothing was touched. A first run always takes this path.
    NoState,
    /// The recorded OS boot time differs from the current one (the OS rebooted), so every
    /// recorded process is necessarily gone. The state file was discarded without touching any
    /// process.
    BootChanged,
    /// The recorded boot time matches the current one. Holds one outcome per recorded service,
    /// keyed by service name.
    Matched(FxHashMap<String, ServiceCleanup>),
}

/// Records `child`'s process-group identity in the cleanup state file.
///
/// Call this once, immediately after a successful `spawn::spawn`, before the caller starts
/// monitoring `child`. Overwrites only `name`'s own entry; other services' records are kept.
/// Pair every call with a later [`forget`] once the process has actually exited through a normal
/// stop, so that a future crash of the app itself does not mistake that process for one still
/// needing cleanup.
///
/// Does nothing (returns `Ok(())` without writing) when `child`'s pid is already gone (reaped by
/// `wait`), or when its process information could not be queried on the OS: there is then
/// nothing left to protect.
///
/// # Errors
///
/// Returns an error when the home directory or the current OS boot time cannot be resolved, or
/// when the updated state cannot be serialized or written.
pub fn record(name: &str, child: &Child) -> Result<(), StateError> {
    record_at(&state_path()?, name, child)
}

/// Runs [`record`]'s body against an explicit path.
fn record_at(path: &Path, name: &str, child: &Child) -> Result<(), StateError> {
    let Some(pid) = child.id() else {
        return Ok(());
    };
    #[expect(
        clippy::cast_possible_wrap,
        reason = "a real OS pid never approaches i32::MAX, so this never actually wraps"
    )]
    let pid = pid as i32;
    let Some(process) = procinfo::running_process(pid) else {
        return Ok(());
    };
    let boot_time = procinfo::boot_time().map_err(StateError::BootTime)?;

    let mut state = match load(path) {
        Some(state)
            if state.boot_time_sec == boot_time.sec && state.boot_time_usec == boot_time.usec =>
        {
            state
        }
        _ => State {
            boot_time_sec: boot_time.sec,
            boot_time_usec: boot_time.usec,
            services: FxHashMap::default(),
        },
    };
    state.services.insert(
        name.to_owned(),
        RecordedProcess {
            pgid: process.pgid,
            start_time_sec: process.start_time.sec,
            start_time_usec: process.start_time.usec,
            exe_path: process.exe_path,
        },
    );
    save(path, &state)
}

/// Removes `name`'s entry from the state file.
///
/// Call this once, after the process has actually exited through a normal stop (a tray
/// operation, a reload's stop, or app shutdown). This keeps a future crash of the app itself
/// from mistaking an already-gone process for one still needing cleanup.
///
/// A missing state file, and `name` having no entry to begin with, are both treated as "already
/// forgotten" and are not errors. Once a call leaves no service recorded, the file itself is
/// removed.
///
/// # Errors
///
/// Returns an error when the home directory cannot be resolved, or when the updated (or
/// removed) state file could not be written.
pub fn forget(name: &str) -> Result<(), StateError> {
    forget_at(&state_path()?, name)
}

/// Runs [`forget`]'s body against an explicit path.
fn forget_at(path: &Path, name: &str) -> Result<(), StateError> {
    let Some(mut state) = load(path) else {
        return Ok(());
    };
    if state.services.remove(name).is_none() {
        return Ok(());
    }
    if state.services.is_empty() {
        return remove_if_exists(path);
    }
    save(path, &state)
}

/// Cleans up processes orphaned by a previous, abnormally-terminated run.
///
/// Run this once, early during app startup, before spawning any service for the current run.
///
/// - No usable state file (missing, unreadable, or not parseable as this crate's schema) yields
///   [`CleanupResult::NoState`], touching nothing. A first run always takes this path.
/// - A recorded OS boot time different from the current one yields [`CleanupResult::BootChanged`]:
///   every recorded process is necessarily gone with the reboot, so the file is discarded
///   without touching any process.
/// - Otherwise (the same boot), each recorded service's pgid is re-queried on the OS, and the
///   cleanup stop sequence (`SIGTERM`, then `SIGKILL` after `grace_period`) is only run against
///   it when the current process's start time still matches what was recorded; this guards
///   against a pid/pgid that has since been reassigned to an unrelated process. The state file
///   is then removed so a later run does not repeat the check.
///
/// Never adopts a matched process back into ongoing monitoring; this only stops it.
pub async fn cleanup(grace_period: Duration) -> CleanupResult {
    let Ok(path) = state_path() else {
        return CleanupResult::NoState;
    };
    cleanup_at(&path, grace_period).await
}

/// Runs [`cleanup`]'s body against an explicit path.
async fn cleanup_at(path: &Path, grace_period: Duration) -> CleanupResult {
    let Some(state) = load(path) else {
        return CleanupResult::NoState;
    };
    let Ok(current_boot_time) = procinfo::boot_time() else {
        return CleanupResult::NoState;
    };
    if state.boot_time_sec != current_boot_time.sec
        || state.boot_time_usec != current_boot_time.usec
    {
        let _ignored = remove_if_exists(path);
        return CleanupResult::BootChanged;
    }

    let mut outcomes = FxHashMap::default();
    for (name, recorded) in state.services {
        let outcome = cleanup_one(&recorded, grace_period).await;
        outcomes.insert(name, outcome);
    }
    let _ignored = remove_if_exists(path);
    CleanupResult::Matched(outcomes)
}

/// Checks one recorded entry against the current OS process information and, if matched, stops
/// it via the cleanup stop sequence.
///
/// Matching compares only the recorded start time (seconds and microseconds) against the
/// process currently holding `recorded.pgid`; `recorded.exe_path` is diagnostic only and plays
/// no part in the check. A process's start time is fixed at `fork` and unaffected by a later
/// `exec`, so this still matches a `login_shell` service whose visible executable changes after
/// `spawn::spawn` returns.
async fn cleanup_one(recorded: &RecordedProcess, grace_period: Duration) -> ServiceCleanup {
    #[expect(
        clippy::cast_possible_wrap,
        reason = "a real OS pgid never approaches i32::MAX, so this never actually wraps"
    )]
    let pid = recorded.pgid as i32;
    let Some(current) = procinfo::running_process(pid) else {
        return ServiceCleanup::NotMatched;
    };
    let start_time_matches = current.start_time.sec == recorded.start_time_sec
        && current.start_time.usec == recorded.start_time_usec;
    if !start_time_matches {
        return ServiceCleanup::NotMatched;
    }

    match stop::stop_orphan(Pid::from_raw(pid), grace_period).await {
        Ok(()) => ServiceCleanup::Stopped,
        Err(source) => ServiceCleanup::StopFailed(source),
    }
}

/// Resolves the default state file path (`~/Library/Application Support/shepherdr/state.toml`).
fn state_path() -> Result<PathBuf, StateError> {
    Ok(env::home_dir()
        .ok_or(StateError::HomeDirNotFound)?
        .join("Library")
        .join("Application Support")
        .join("shepherdr")
        .join(STATE_FILE_NAME))
}

/// Loads the state file at `path`. A missing file, an unreadable file, and one that fails to
/// parse are all treated alike, as `None`: the state file is a disposable cache, not
/// user-authored configuration, so a corrupt copy is silently passed over rather than surfaced
/// as an error, and simply gets overwritten.
fn load(path: &Path) -> Option<State> {
    let content = fs::read_to_string(path).ok()?;
    toml::from_str(&content).ok()
}

/// Writes `state` to `path`, creating the parent directory if it does not exist.
fn save(path: &Path, state: &State) -> Result<(), StateError> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|source| StateError::Persist {
            path: path.to_path_buf(),
            source,
        })?;
    }
    let content = toml::to_string(state).map_err(StateError::Serialize)?;
    fs::write(path, content).map_err(|source| StateError::Persist {
        path: path.to_path_buf(),
        source,
    })
}

/// Removes the file at `path`. Not an error if it is already gone.
fn remove_if_exists(path: &Path) -> Result<(), StateError> {
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(source) if source.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(source) => Err(StateError::Persist {
            path: path.to_path_buf(),
            source,
        }),
    }
}

#[cfg(test)]
mod tests {
    use std::process::{ExitStatus, Stdio};

    use rustc_hash::FxHashMap;
    use tokio::process::Command;

    use super::*;
    use crate::config::Service;
    use crate::spawn;

    /// A disposable directory for this test (wipes any leftovers from a previous run first).
    fn scratch_dir(label: &str) -> PathBuf {
        let dir = env::temp_dir().join(format!("shepherdr-state-test-{label}"));
        let _ = fs::remove_dir_all(&dir);
        dir
    }

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

    /// A real child process kept running for a test. Always clean it up with `kill_and_reap`
    /// once the test is done.
    fn spawn_running(command: &[&str]) -> Child {
        spawn::spawn(&service(command)).expect("spawn should succeed")
    }

    async fn kill_and_reap(mut child: Child) {
        let _: io::Result<()> = child.start_kill();
        let _: io::Result<ExitStatus> = child.wait().await;
    }

    #[tokio::test]
    async fn positive_record_at_writes_a_new_entry_for_a_freshly_spawned_child() {
        // Given a freshly spawned child and an empty scratch state directory
        let path = scratch_dir("record-new").join("state.toml");
        let child = spawn_running(&["sleep", "30"]);
        let pid = child.id().expect("child should still be running");

        // When it is recorded
        let result = record_at(&path, "svc", &child);

        // Then the state file holds one entry for "svc" matching the running process
        result.expect("record should succeed");
        let state = load(&path).expect("state should be readable");
        let recorded = state.services.get("svc").expect("svc should be recorded");
        assert_eq!(recorded.pgid, pid);
        assert_eq!(
            state.boot_time_sec,
            procinfo::boot_time()
                .expect("boot time should be readable")
                .sec
        );
        assert_eq!(state.services.len(), 1);

        kill_and_reap(child).await;
    }

    #[tokio::test]
    async fn positive_record_at_preserves_other_services_already_recorded() {
        // Given a state file already recording one service
        let path = scratch_dir("record-merge").join("state.toml");
        let first = spawn_running(&["sleep", "30"]);
        record_at(&path, "first", &first).expect("recording the first service should succeed");

        // When a second, different service is recorded
        let second = spawn_running(&["sleep", "30"]);
        let result = record_at(&path, "second", &second);

        // Then both entries are present
        result.expect("record should succeed");
        let state = load(&path).expect("state should be readable");
        assert!(state.services.contains_key("first"));
        assert!(state.services.contains_key("second"));

        kill_and_reap(first).await;
        kill_and_reap(second).await;
    }

    #[tokio::test]
    async fn positive_record_at_replaces_a_stale_boot_time_and_drops_older_entries() {
        // Given a state file recorded under a different (stale) boot time, holding an entry for
        // an unrelated service
        let path = scratch_dir("record-stale-boot").join("state.toml");
        let outdated = State {
            boot_time_sec: 1,
            boot_time_usec: 0,
            services: FxHashMap::from_iter([(
                "old-service".to_owned(),
                RecordedProcess {
                    pgid: 123,
                    start_time_sec: 1,
                    start_time_usec: 0,
                    exe_path: PathBuf::from("/bin/old"),
                },
            )]),
        };
        save(&path, &outdated).expect("writing the stale state should succeed");

        // When a new service is recorded
        let child = spawn_running(&["sleep", "30"]);
        let result = record_at(&path, "new-service", &child);

        // Then the boot time is refreshed to the current one and the stale entry is gone
        result.expect("record should succeed");
        let state = load(&path).expect("state should be readable");
        assert_eq!(
            state.boot_time_sec,
            procinfo::boot_time()
                .expect("boot time should be readable")
                .sec
        );
        assert_eq!(state.services.len(), 1);
        assert!(state.services.contains_key("new-service"));

        kill_and_reap(child).await;
    }

    #[tokio::test]
    async fn positive_record_at_does_nothing_when_the_child_has_already_been_reaped() {
        // Given a child that has already exited and been reaped
        let mut child = Command::new("true")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn should succeed");
        child.wait().await.expect("child should run");

        // When it is recorded
        let path = scratch_dir("record-reaped").join("state.toml");
        let result = record_at(&path, "svc", &child);

        // Then nothing is written, since there is no pid left to protect
        result.expect("record should succeed");
        assert!(!path.exists());
    }

    #[test]
    fn negative_record_at_fails_when_the_state_directory_cannot_be_created() {
        // Given a path whose parent "directory" is actually a plain file
        let dir = scratch_dir("record-blocked-directory");
        fs::create_dir_all(dir.parent().expect("scratch dir should have a parent"))
            .expect("temp root should be creatable");
        fs::write(&dir, b"not a directory").expect("blocking file should be writable");
        let path = dir.join("state.toml");
        let state = State {
            boot_time_sec: 1,
            boot_time_usec: 0,
            services: FxHashMap::default(),
        };

        // When the state is saved under it
        let result = save(&path, &state);

        // Then it fails trying to create the parent directory
        assert!(matches!(result, Err(StateError::Persist { .. })));
    }

    #[test]
    fn positive_forget_at_removes_only_the_named_entry() {
        // Given a state file recording two services
        let path = scratch_dir("forget-one-of-two").join("state.toml");
        let state = State {
            boot_time_sec: 1,
            boot_time_usec: 0,
            services: FxHashMap::from_iter([
                (
                    "keep".to_owned(),
                    RecordedProcess {
                        pgid: 1,
                        start_time_sec: 1,
                        start_time_usec: 0,
                        exe_path: PathBuf::from("/bin/keep"),
                    },
                ),
                (
                    "drop".to_owned(),
                    RecordedProcess {
                        pgid: 2,
                        start_time_sec: 1,
                        start_time_usec: 0,
                        exe_path: PathBuf::from("/bin/drop"),
                    },
                ),
            ]),
        };
        save(&path, &state).expect("writing the state should succeed");

        // When one service is forgotten
        let result = forget_at(&path, "drop");

        // Then only that entry is removed, and the file still holds the other one
        result.expect("forget should succeed");
        let remaining = load(&path).expect("state should be readable");
        assert!(remaining.services.contains_key("keep"));
        assert!(!remaining.services.contains_key("drop"));
    }

    #[test]
    fn positive_forget_at_deletes_the_file_when_no_service_remains() {
        // Given a state file recording exactly one service
        let path = scratch_dir("forget-last").join("state.toml");
        let state = State {
            boot_time_sec: 1,
            boot_time_usec: 0,
            services: FxHashMap::from_iter([(
                "only".to_owned(),
                RecordedProcess {
                    pgid: 1,
                    start_time_sec: 1,
                    start_time_usec: 0,
                    exe_path: PathBuf::from("/bin/only"),
                },
            )]),
        };
        save(&path, &state).expect("writing the state should succeed");

        // When that service is forgotten
        let result = forget_at(&path, "only");

        // Then the file is removed entirely
        result.expect("forget should succeed");
        assert!(!path.exists());
    }

    #[test]
    fn positive_forget_at_is_a_no_op_when_the_state_file_does_not_exist() {
        // Given a path with no state file
        let path = scratch_dir("forget-missing-file").join("state.toml");

        // When a service is forgotten
        let result = forget_at(&path, "svc");

        // Then it succeeds without creating anything
        result.expect("forget should succeed");
        assert!(!path.exists());
    }

    #[test]
    fn positive_forget_at_is_a_no_op_when_the_named_entry_is_absent() {
        // Given a state file that does not record the service being forgotten
        let path = scratch_dir("forget-absent-entry").join("state.toml");
        let state = State {
            boot_time_sec: 1,
            boot_time_usec: 0,
            services: FxHashMap::from_iter([(
                "other".to_owned(),
                RecordedProcess {
                    pgid: 1,
                    start_time_sec: 1,
                    start_time_usec: 0,
                    exe_path: PathBuf::from("/bin/other"),
                },
            )]),
        };
        save(&path, &state).expect("writing the state should succeed");

        // When an unrecorded name is forgotten
        let result = forget_at(&path, "absent");

        // Then it succeeds and the existing entry is untouched
        result.expect("forget should succeed");
        let remaining = load(&path).expect("state should be readable");
        assert!(remaining.services.contains_key("other"));
    }

    #[test]
    fn positive_remove_if_exists_is_a_no_op_when_the_file_is_absent() {
        // Given a path with no file
        let path = scratch_dir("remove-if-exists-absent").join("state.toml");

        // When it is removed
        let result = remove_if_exists(&path);

        // Then it succeeds without error
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn positive_cleanup_at_returns_no_state_when_the_file_is_missing() {
        // Given a path with no state file
        let path = scratch_dir("cleanup-missing").join("state.toml");

        // When cleanup runs
        let result = cleanup_at(&path, Duration::from_millis(200)).await;

        // Then it reports no usable state
        assert!(matches!(result, CleanupResult::NoState));
    }

    #[tokio::test]
    async fn positive_cleanup_at_returns_no_state_when_the_file_is_not_valid_toml() {
        // Given a state file that is not valid TOML
        let dir = scratch_dir("cleanup-invalid-toml");
        fs::create_dir_all(&dir).expect("scratch dir should be creatable");
        let path = dir.join("state.toml");
        fs::write(&path, b"not valid toml {{{").expect("writing garbage should succeed");

        // When cleanup runs
        let result = cleanup_at(&path, Duration::from_millis(200)).await;

        // Then it reports no usable state, leaving the unreadable file untouched
        assert!(matches!(result, CleanupResult::NoState));
        assert!(path.exists());
    }

    #[tokio::test]
    async fn positive_cleanup_at_discards_the_file_and_reports_boot_changed() {
        // Given a state file recorded under a boot time that cannot be the current one
        let path = scratch_dir("cleanup-boot-changed").join("state.toml");
        let state = State {
            boot_time_sec: 1,
            boot_time_usec: 0,
            services: FxHashMap::from_iter([(
                "svc".to_owned(),
                RecordedProcess {
                    pgid: 1,
                    start_time_sec: 1,
                    start_time_usec: 0,
                    exe_path: PathBuf::from("/bin/svc"),
                },
            )]),
        };
        save(&path, &state).expect("writing the state should succeed");

        // When cleanup runs
        let result = cleanup_at(&path, Duration::from_millis(200)).await;

        // Then it reports a boot change and discards the file without touching any process
        assert!(matches!(result, CleanupResult::BootChanged));
        assert!(!path.exists());
    }

    #[tokio::test]
    async fn positive_cleanup_at_leaves_a_service_untouched_when_the_recorded_process_no_longer_exists()
     {
        // Given a state file, recorded under the current boot time, whose recorded pgid cannot
        // correspond to any running process
        let path = scratch_dir("cleanup-not-matched-gone").join("state.toml");
        let boot_time = procinfo::boot_time().expect("boot time should be readable");
        let unused_pgid = u32::try_from(i32::MAX).expect("i32::MAX fits in a u32");
        let state = State {
            boot_time_sec: boot_time.sec,
            boot_time_usec: boot_time.usec,
            services: FxHashMap::from_iter([(
                "gone".to_owned(),
                RecordedProcess {
                    pgid: unused_pgid,
                    start_time_sec: 1,
                    start_time_usec: 0,
                    exe_path: PathBuf::from("/bin/gone"),
                },
            )]),
        };
        save(&path, &state).expect("writing the state should succeed");

        // When cleanup runs
        let result = cleanup_at(&path, Duration::from_millis(200)).await;

        // Then the service is left untouched, and the state file is cleared regardless
        let CleanupResult::Matched(outcomes) = result else {
            unreachable!("boot time matches, so cleanup_at always returns Matched");
        };
        assert!(matches!(
            outcomes.get("gone"),
            Some(ServiceCleanup::NotMatched)
        ));
        assert!(!path.exists());
    }

    #[tokio::test]
    async fn positive_cleanup_at_leaves_a_service_untouched_when_the_start_time_does_not_match() {
        // Given a state file, recorded under the current boot time, whose entry names a real
        // running process's pgid but the wrong start time (as if that pgid had been reassigned
        // to an unrelated process)
        let path = scratch_dir("cleanup-not-matched-start-time").join("state.toml");
        let child = spawn_running(&["sleep", "30"]);
        let pid = i32::try_from(child.id().expect("child should still be running"))
            .expect("pid fits in i32");
        let process = procinfo::running_process(pid).expect("the spawned process should be found");
        let boot_time = procinfo::boot_time().expect("boot time should be readable");
        let state = State {
            boot_time_sec: boot_time.sec,
            boot_time_usec: boot_time.usec,
            services: FxHashMap::from_iter([(
                "svc".to_owned(),
                RecordedProcess {
                    pgid: process.pgid,
                    start_time_sec: 1,
                    start_time_usec: 0,
                    exe_path: process.exe_path,
                },
            )]),
        };
        save(&path, &state).expect("writing the state should succeed");

        // When cleanup runs
        let result = cleanup_at(&path, Duration::from_millis(200)).await;

        // Then the service is left untouched (the real process is not killed), and the state
        // file is cleared regardless
        let CleanupResult::Matched(outcomes) = result else {
            unreachable!("boot time matches, so cleanup_at always returns Matched");
        };
        assert!(matches!(
            outcomes.get("svc"),
            Some(ServiceCleanup::NotMatched)
        ));
        assert!(!path.exists());

        kill_and_reap(child).await;
    }

    #[tokio::test]
    async fn positive_cleanup_at_stops_a_matching_orphaned_process() {
        // Given a state file, recorded under the current boot time, whose entry matches a real
        // running process's pgid and start time exactly, but names an unrelated exe_path (as a
        // `login_shell` service's recorded exe_path can, if captured before its shell finished
        // `exec`ing the final program); something else (standing in for launchd reaping a real
        // orphan) reaps the process once it exits
        let path = scratch_dir("cleanup-stops-match").join("state.toml");
        let mut child = spawn_running(&["sleep", "30"]);
        let pid = i32::try_from(child.id().expect("child should still be running"))
            .expect("pid fits in i32");
        let process = procinfo::running_process(pid).expect("the spawned process should be found");
        let boot_time = procinfo::boot_time().expect("boot time should be readable");
        let state = State {
            boot_time_sec: boot_time.sec,
            boot_time_usec: boot_time.usec,
            services: FxHashMap::from_iter([(
                "svc".to_owned(),
                RecordedProcess {
                    pgid: process.pgid,
                    start_time_sec: process.start_time.sec,
                    start_time_usec: process.start_time.usec,
                    exe_path: PathBuf::from("/bin/an-unrelated-login-shell"),
                },
            )]),
        };
        save(&path, &state).expect("writing the state should succeed");
        tokio::spawn(async move {
            let _: io::Result<ExitStatus> = child.wait().await;
        });

        // When cleanup runs
        let result = cleanup_at(&path, Duration::from_secs(5)).await;

        // Then the process was stopped despite the mismatched exe_path, since matching relies
        // only on the start time, and the state file was cleared
        let CleanupResult::Matched(outcomes) = result else {
            unreachable!("boot time matches, so cleanup_at always returns Matched");
        };
        assert!(matches!(outcomes.get("svc"), Some(ServiceCleanup::Stopped)));
        assert!(!path.exists());
    }
}
