//! App-level service supervision: the startup, monitoring, and shutdown flow.
//!
//! `shepherdr_core` deliberately stops short of spawning, exit detection, and the wiring between
//! its own modules, leaving them to "a higher layer" (see the module docs of
//! [`shepherdr_core::monitor`], [`shepherdr_core::reload`], and [`shepherdr_core::state`]). This
//! module is that layer: it owns one supervision task per service, publishes their observable
//! state, and runs the stop sequence for all of them when the app exits.

mod service;

use std::mem;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use rustc_hash::FxHashMap;
use shepherdr_core::config::{Config, LogConfig, RestartConfig, StopConfig};
use shepherdr_core::state::{self, CleanupResult, ServiceCleanup};
use tauri::async_runtime;
use tokio::sync::{oneshot, watch};

use crate::supervisor::service::{ServiceCommand, ServiceHandle, ServiceTask};

/// The observable state of one service, as the tray displays it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ServiceState {
    /// A child process is running.
    Running,
    /// No child process, and none will be started until asked.
    Stopped,
    /// The child exited and a restart is pending, waiting out the backoff.
    Restarting,
    /// Consecutive failures reached the limit; auto-restart stopped.
    Failed,
}

/// A snapshot of every known service's [`ServiceState`], keyed by service name.
pub type ServiceStates = FxHashMap<String, ServiceState>;

/// A handle to the running supervisor.
///
/// Cheap to clone; every clone refers to the same set of supervision tasks.
#[derive(Clone)]
pub struct Supervisor {
    inner: Arc<Inner>,
}

/// The supervisor state shared by every [`Supervisor`] clone and every supervision task.
struct Inner {
    /// The live supervision tasks, plus the flag that closes registration for good.
    registry: Mutex<Registry>,
    /// The published [`ServiceStates`]. Held behind an `Arc` so the supervision tasks can publish
    /// into it without holding a reference back to the whole [`Inner`].
    states: Arc<watch::Sender<ServiceStates>>,
}

/// The supervision tasks currently registered.
#[derive(Default)]
struct Registry {
    /// Set by [`Supervisor::shutdown`]. Once set, no further service is ever registered, so a
    /// shutdown that races an in-flight startup cannot leave a service running behind it.
    shutting_down: bool,
    /// One entry per service defined in the config file, whether enabled or not, keyed by name.
    handles: FxHashMap<String, ServiceHandle>,
}

impl Supervisor {
    /// Starts the supervisor and returns immediately.
    ///
    /// The startup flow (orphan cleanup, loading the config, spawning the services) runs on the
    /// async runtime, because orphan cleanup runs a full stop sequence and can take as long as
    /// the grace period; blocking the app's startup on it would freeze the UI for that long.
    /// [`Supervisor::shutdown`] is safe to call before the flow has finished.
    ///
    /// # Panics
    ///
    /// Panics if the Tauri async runtime cannot be initialized.
    #[must_use]
    pub fn start() -> Self {
        let (states, _) = watch::channel(ServiceStates::default());
        let supervisor = Self {
            inner: Arc::new(Inner {
                registry: Mutex::new(Registry::default()),
                states: Arc::new(states),
            }),
        };

        let starting = supervisor.clone();
        let _startup = async_runtime::spawn(async move { starting.bootstrap().await });
        supervisor
    }

    /// The current state of every known service.
    #[must_use]
    pub fn states(&self) -> ServiceStates {
        self.inner.states.borrow().clone()
    }

    /// Subscribes to state changes. The receiver yields a fresh [`ServiceStates`] snapshot every
    /// time any service changes state.
    #[must_use]
    pub fn subscribe(&self) -> watch::Receiver<ServiceStates> {
        self.inner.states.subscribe()
    }

    /// Manually starts `name`, resetting its failure counter and backoff so that a failed service
    /// can be retried.
    ///
    /// A name that is not defined in the config file is ignored, as is a service that is already
    /// running.
    pub fn start_service(&self, name: &str) {
        self.command(name, ServiceCommand::Start);
    }

    /// Manually stops `name`, taking it out of the restart loop until it is started again.
    ///
    /// A name that is not defined in the config file is ignored.
    pub fn stop_service(&self, name: &str) {
        self.command(name, ServiceCommand::Stop);
    }

    /// Manually restarts `name`: the stop sequence, then a fresh start.
    ///
    /// A name that is not defined in the config file is ignored.
    pub fn restart_service(&self, name: &str) {
        self.command(name, ServiceCommand::Restart);
    }

    /// Stops every service and waits for all of them to finish.
    ///
    /// The stop sequence is applied to all services concurrently and this only returns once every
    /// one of them has completed it. Registration is closed first, so a service whose startup was
    /// still in flight is never spawned afterwards.
    pub async fn shutdown(&self) {
        let handles = {
            let mut registry = self.registry();
            registry.shutting_down = true;
            mem::take(&mut registry.handles)
        };

        // Deliver every stop first, then wait: the tasks run the sequence concurrently.
        let mut completions = Vec::with_capacity(handles.len());
        for handle in handles.into_values() {
            let (done, completion) = oneshot::channel();
            if handle.send(ServiceCommand::Shutdown(done)) {
                completions.push(completion);
            }
        }
        for completion in completions {
            let _stopped = completion.await;
        }
    }

    /// Runs the startup flow: clean up the previous run's orphans, then start the services.
    async fn bootstrap(&self) {
        let config = load_config();
        report_cleanup(&state::cleanup(config.stop.grace_period).await);
        self.spawn_services(config);
    }

    /// Registers and starts one supervision task per service defined in `config`.
    ///
    /// Disabled services get a task too, sitting in [`ServiceState::Stopped`], so they show up in
    /// the tray and can be started from there.
    fn spawn_services(&self, config: Config) {
        let Config {
            services,
            log,
            restart,
            stop,
        } = config;

        let mut registry = self.registry();
        if registry.shutting_down {
            return;
        }
        for service in services {
            let name = service.name.clone();
            let (handle, task) = ServiceTask::new(
                service,
                restart,
                log.clone(),
                stop.grace_period,
                Arc::clone(&self.inner.states),
            );
            let _supervision = async_runtime::spawn(task.run());
            let _replaced = registry.handles.insert(name, handle);
        }
    }

    /// Sends `command` to `name`'s supervision task, ignoring an unknown name.
    fn command(&self, name: &str, command: ServiceCommand) {
        if let Some(handle) = self.registry().handles.get(name) {
            let _delivered = handle.send(command);
        }
    }

    /// Locks the registry, recovering from a poisoned lock rather than propagating the panic.
    fn registry(&self) -> MutexGuard<'_, Registry> {
        self.inner
            .registry
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
    }
}

/// Loads the config file, falling back to a configuration with no services when it cannot be read.
///
/// The config file is the single source of truth for what should run, and nothing about the
/// running state is persisted, so when the file cannot be read or parsed there is no service
/// definition left to derive: the supervisor starts nothing. The app itself stays up regardless,
/// since a missing file is the ordinary first-run case and the tray is what the user needs in
/// order to fix and reload the configuration.
fn load_config() -> Config {
    match Config::load() {
        Ok(config) => config,
        Err(error) => {
            eprintln!("shepherdr: failed to load the configuration: {error}");
            Config {
                services: Vec::new(),
                log: LogConfig::default(),
                restart: RestartConfig::default(),
                stop: StopConfig::default(),
            }
        }
    }
}

/// Reports the orphan-cleanup outcomes that need the user's attention.
///
/// A process that could not be terminated is the only outcome worth surfacing: it may still hold
/// the resources (a listening port, for instance) that the service about to be started needs.
fn report_cleanup(result: &CleanupResult) {
    let CleanupResult::Matched(outcomes) = result else {
        return;
    };
    for (name, outcome) in outcomes {
        if let ServiceCleanup::StopFailed(error) = outcome {
            eprintln!("shepherdr: failed to clean up the orphaned process of \"{name}\": {error}");
        }
    }
}
