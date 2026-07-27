//! Commands backing the log window: which services can be tailed, and streaming one of their log
//! files to the frontend as it grows.
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

/// Lists every service name the supervisor currently knows about (from the loaded
/// configuration), sorted, for the log window's service picker.
#[tauri::command]
#[allow(
    clippy::needless_pass_by_value,
    reason = "tauri::command parameters are extracted by Tauri's IPC layer, which requires \
              State<T> by value per the framework's own command signature convention"
)]
pub fn list_services(supervisor: tauri::State<'_, Supervisor>) -> Vec<String> {
    let mut names: Vec<String> = supervisor.states().into_keys().collect();
    names.sort_unstable();
    names
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
