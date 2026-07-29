//! The commands backing the log window: which service it is showing, and streaming that service's
//! log file to the frontend as it grows.
//!
//! Tailing is push-based, over a Tauri channel, rather than polled from the frontend: the Tauri
//! channel is the mechanism the framework recommends for ordered, high-throughput streams such as
//! this one (it is what the framework itself uses internally to stream a child process's own
//! output to the frontend), and it keeps the frontend from having to poll a command on a timer.

// The #[tauri::command] macro expands an async command (`tail_log`) into an additional,
// invisible dispatch item beside the visible function; that generated item references its
// return type through a fully qualified path, which clippy attributes back to this module
// rather than to anything written here.
#![allow(clippy::absolute_paths, reason = "see the module-level comment above")]

mod tail;

use std::path::PathBuf;
use std::sync::{Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use serde::{Serialize, Serializer};
use shepherdr_core::logging::{self, LogError};
use tauri::async_runtime::{self, JoinHandle};
use tauri::ipc::Channel;
use thiserror::Error;

use crate::logs::tail::TailEvent;
use crate::supervisor::Supervisor;

impl tail::TailSink for Channel<TailEvent> {
    fn deliver(&self, event: TailEvent) -> bool {
        self.send(event).is_ok()
    }
}

/// Errors [`tail_log`] can return to the frontend.
#[derive(Debug, Error)]
pub enum TailLogError {
    /// `name` is not a service the supervisor knows about.
    #[error("unknown service \"{0}\"")]
    UnknownService(String),
    /// The log file path for `name` could not be resolved.
    #[error("failed to resolve the log path of \"{name}\"")]
    LogPath {
        /// The service whose log path could not be resolved.
        name: String,
        /// The underlying error.
        #[source]
        source: LogError,
    },
}

// Tauri requires every command error to implement `Serialize`; the standard approach (see the
// "Error Handling" section of https://v2.tauri.app/develop/calling-rust/) is a `thiserror` enum
// paired with a hand-written `Serialize` that sends the display message across the IPC boundary.
impl Serialize for TailLogError {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(&self.to_string())
    }
}

/// Holds the tail task the log window currently has running, if any.
///
/// Only one tail is ever shown in the log window at a time, so starting a new one (a fresh
/// selection, or switching services) simply replaces whatever was running before.
#[derive(Default)]
pub struct TailRegistry {
    current: Mutex<Option<JoinHandle<()>>>,
}

impl TailRegistry {
    /// Ends whichever tail task is running, if any.
    fn stop(&self) {
        if let Some(handle) = self.lock().take() {
            handle.abort();
        }
    }

    /// Starts tailing `path` at `poll_interval`, ending whichever tail task was running before.
    fn replace(&self, path: PathBuf, poll_interval: Duration, sink: Channel<TailEvent>) {
        let handle = async_runtime::spawn(tail::run(path, poll_interval, sink));
        if let Some(previous) = self.lock().replace(handle) {
            previous.abort();
        }
    }

    /// Locks the current tail slot, recovering from a poisoned lock rather than propagating the
    /// panic.
    fn lock(&self) -> MutexGuard<'_, Option<JoinHandle<()>>> {
        self.current.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

/// The service the log window is showing, as picked in the window's toolbar.
///
/// The toolbar reports every change as an event, but Tauri does not replay an event to a listener
/// that registers after it was emitted, and the first selection is settled while the frontend is
/// still starting up. Keeping the selection here as well is what lets the frontend read the one it
/// was not yet listening for. This is also the record of what was last reported, so that the
/// published selection and the last event can never say different things.
#[derive(Default)]
pub struct Selection {
    current: Mutex<Option<String>>,
}

impl Selection {
    /// Records `name` as the selection, answering whether that is a change from what stood before.
    pub fn update(&self, name: Option<String>) -> bool {
        let mut current = self.lock();
        if *current == name {
            return false;
        }
        *current = name;
        true
    }

    /// The selection as it stands.
    #[must_use]
    pub fn current(&self) -> Option<String> {
        self.lock().clone()
    }

    /// Locks the selection, recovering from a poisoned lock rather than propagating the panic.
    fn lock(&self) -> MutexGuard<'_, Option<String>> {
        self.current.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

/// The service the log window's toolbar currently has picked, or `null` when the configuration
/// defines no service to pick.
///
/// The frontend reads this once, as it starts up; every later change reaches it as the toolbar's
/// own event instead.
#[tauri::command]
#[allow(
    clippy::needless_pass_by_value,
    reason = "tauri::command parameters are extracted by Tauri's IPC layer, which requires \
              State<T> by value per the framework's own command signature convention"
)]
pub fn selected_service(selection: tauri::State<'_, Selection>) -> Option<String> {
    selection.current()
}

/// Starts tailing `name`'s log file, delivering updates over `on_event`. Replaces whatever this
/// window was tailing before.
///
/// The poll interval is read from the currently applied configuration's `[log]` section (see
/// [`Supervisor::log_config`](crate::supervisor::Supervisor::log_config)).
///
/// # Errors
///
/// Returns [`TailLogError::UnknownService`] when `name` is not a service the supervisor knows
/// about, or [`TailLogError::LogPath`] when the log directory's path cannot be resolved.
#[tauri::command]
#[allow(
    clippy::needless_pass_by_value,
    reason = "tauri::command parameters (State<T>, Channel<T>, and IPC-deserialized arguments \
              such as `name`) are extracted by Tauri's IPC layer by value per the framework's \
              own command signature convention"
)]
pub async fn tail_log(
    supervisor: tauri::State<'_, Supervisor>,
    registry: tauri::State<'_, TailRegistry>,
    name: String,
    on_event: Channel<TailEvent>,
) -> Result<(), TailLogError> {
    if !supervisor.states().contains_key(&name) {
        return Err(TailLogError::UnknownService(name));
    }
    let path =
        logging::log_path(&name).map_err(move |source| TailLogError::LogPath { name, source })?;
    let poll_interval = supervisor.log_config().await.tail_poll_interval;
    registry.replace(path, poll_interval, on_event);
    Ok(())
}

/// Ends whatever this window is currently tailing.
#[tauri::command]
#[allow(
    clippy::needless_pass_by_value,
    reason = "tauri::command parameters are extracted by Tauri's IPC layer, which requires \
              State<T> by value per the framework's own command signature convention"
)]
pub fn stop_tail(registry: tauri::State<'_, TailRegistry>) {
    registry.stop();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn positive_update_reports_a_change_when_a_different_service_is_picked() {
        // Given a selection standing at one service
        let selection = Selection::default();
        let _first = selection.update(Some("caddy".to_owned()));

        // When another one is picked
        let changed = selection.update(Some("redis".to_owned()));

        // Then the change is reported, which is what lets the event go out
        assert!(changed);
    }

    #[test]
    fn negative_update_reports_no_change_when_the_same_service_is_picked_again() {
        // Given a selection standing at one service
        let selection = Selection::default();
        let _first = selection.update(Some("caddy".to_owned()));

        // When a rebuild of the picker's entries settles on that same service
        let changed = selection.update(Some("caddy".to_owned()));

        // Then nothing is reported, so the rebuild stays quiet
        assert!(!changed);
    }

    #[test]
    fn positive_update_reports_a_change_when_the_last_service_is_gone() {
        // Given a selection standing at one service
        let selection = Selection::default();
        let _first = selection.update(Some("caddy".to_owned()));

        // When a reload leaves the configuration with no service to pick
        let changed = selection.update(None);

        // Then the change is reported, so the frontend hears that there is nothing to show
        assert!(changed);
    }
}
