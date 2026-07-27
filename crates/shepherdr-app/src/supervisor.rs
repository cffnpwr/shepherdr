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
use shepherdr_core::config::{Config, ConfigError, Service};
use shepherdr_core::reload::{self, Action};
use shepherdr_core::state::{self, CleanupResult, ServiceCleanup};
use tauri::async_runtime;
use tokio::sync::{Mutex as AsyncMutex, oneshot, watch};

use crate::supervisor::service::{ServiceCommand, ServiceHandle, ServiceTask};

/// The observable state of one service, as the tray displays it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ServiceState {
    /// A child process is running.
    Running,
    /// No child process, and none will be started until asked.
    Stopped,
    /// The child exited and a restart is pending, waiting out the backoff.
    AwaitingRestart,
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
    /// The configuration the running services were built from, and the old side of the next
    /// reload's diff. The async lock is what serializes the startup flow against reloads and
    /// reloads against each other: applying a diff spans the stop sequences it triggers, so the
    /// next diff must not be computed against a configuration that is only half applied.
    config: AsyncMutex<Config>,
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
                config: AsyncMutex::new(Config::default()),
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

    /// Reloads the config file and reconciles the running services with it.
    ///
    /// Added services are started, removed ones are stopped and taken out of the tray, redefined
    /// ones are restarted, and ones whose `enabled` flipped are started or stopped, as
    /// [`reload::plan`] decides. A service that comes out of the diff unchanged keeps running
    /// untouched.
    ///
    /// The global `[log]`, `[restart]`, and `[stop]` sections take effect for every supervision
    /// task this reload creates. A service that the diff leaves alone keeps the settings it was
    /// started with, since the reload only ever acts on a service the diff singles out.
    ///
    /// # Errors
    ///
    /// Returns the error from [`Config::load`] when the new configuration cannot be read, parsed,
    /// or validated. Nothing is changed in that case: the previous configuration stays applied.
    pub async fn reload(&self) -> Result<(), ConfigError> {
        let new = Config::load()?;
        let mut applied = self.inner.config.lock().await;
        self.reconcile(&applied, &new).await;
        *applied = new;
        Ok(())
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
    ///
    /// The configuration lock is held across the whole flow, so a reload asked for while the app
    /// is still starting up waits and then diffs against the configuration that was applied,
    /// rather than starting the same services a second time.
    async fn bootstrap(&self) {
        let mut applied = self.inner.config.lock().await;
        let config = load_config();
        report_cleanup(&state::cleanup(config.stop.grace_period).await);
        self.spawn_services(&config);
        *applied = config;
    }

    /// Registers and starts one supervision task per service defined in `config`.
    ///
    /// Disabled services get a task too, sitting in [`ServiceState::Stopped`], so they show up in
    /// the tray and can be started from there.
    fn spawn_services(&self, config: &Config) {
        for service in &config.services {
            self.spawn_service(service.clone(), config);
        }
    }

    /// Registers and starts the supervision task for one service, under `config`'s global
    /// settings. Does nothing once the supervisor is shutting down.
    fn spawn_service(&self, service: Service, config: &Config) {
        let mut registry = self.registry();
        if registry.shutting_down {
            return;
        }
        let name = service.name.clone();
        let (handle, task) = ServiceTask::new(
            service,
            config.restart,
            config.log.clone(),
            config.stop.grace_period,
            Arc::clone(&self.inner.states),
        );
        let _supervision = async_runtime::spawn(task.run());
        let _replaced = registry.handles.insert(name, handle);
    }

    /// Applies the difference between the applied configuration and a newly loaded one.
    async fn reconcile(&self, old: &Config, new: &Config) {
        let old_services = index(&old.services);
        let new_services = index(&new.services);

        for (name, action) in reload::plan(old, new) {
            let defined = new_services.get(name.as_str()).copied();
            let redefined = old_services.get(name.as_str()) != new_services.get(name.as_str());
            match effect_of(action, defined.is_some(), redefined) {
                Effect::Keep => {}
                Effect::Retire => {
                    self.retire_service(&name).await;
                    self.forget_state(&name);
                }
                Effect::Respawn => {
                    self.retire_service(&name).await;
                    if let Some(service) = defined {
                        self.spawn_service(service.clone(), new);
                    }
                }
            }
        }
    }

    /// Takes `name`'s supervision task out of the registry and waits for its stop sequence.
    ///
    /// The task is ended rather than merely stopped, because the definition it carries is the one
    /// that is being replaced or dropped.
    async fn retire_service(&self, name: &str) {
        let Some(handle) = self.registry().handles.remove(name) else {
            return;
        };
        let (done, completion) = oneshot::channel();
        if handle.send(ServiceCommand::Shutdown(done)) {
            let _stopped = completion.await;
        }
    }

    /// Drops `name` from the published states, taking it out of the tray.
    fn forget_state(&self, name: &str) {
        self.inner.states.send_modify(|states| {
            let _removed = states.remove(name);
        });
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

/// What a reload does to one service's supervision task.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Effect {
    /// Leave the task running as it is.
    Keep,
    /// End the task and drop the service from the tray.
    Retire,
    /// End the task and register a new one built from the new definition.
    Respawn,
}

/// Turns a reload [`Action`] into the effect it has on the supervision task, given whether the
/// service is still `defined` in the new configuration and whether its definition was changed
/// (`redefined`) by the reload.
///
/// An [`Action`] says what should end up running, but a supervision task carries the definition it
/// was built with, so anything the action changes has to be rebuilt from the new definition rather
/// than commanded on the old task. [`Action::NoChange`] therefore still respawns a task whose
/// definition changed: that case only arises for a service that is disabled on both sides, so
/// nothing observable is stopped, and the task is left holding the definition a later manual start
/// would run.
fn effect_of(action: Action, defined: bool, redefined: bool) -> Effect {
    if !defined {
        return Effect::Retire;
    }
    if action == Action::NoChange && !redefined {
        return Effect::Keep;
    }
    Effect::Respawn
}

/// Indexes services by name, for looking a definition up on either side of a reload.
fn index(services: &[Service]) -> FxHashMap<&str, &Service> {
    services
        .iter()
        .map(|service| (service.name.as_str(), service))
        .collect()
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
            Config::default()
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn positive_effect_of_an_action_on_a_service_no_longer_defined_retires_it() {
        // Given a service that the new configuration dropped altogether
        let action = Action::Stop;

        // When the effect on its supervision task is decided
        let effect = effect_of(action, false, true);

        // Then the task ends and the service leaves the tray
        assert_eq!(effect, Effect::Retire);
    }

    #[test]
    fn positive_effect_of_no_change_on_an_untouched_service_keeps_its_task() {
        // Given a service the diff singles out for nothing, defined exactly as before
        let action = Action::NoChange;

        // When the effect on its supervision task is decided
        let effect = effect_of(action, true, false);

        // Then it keeps running untouched
        assert_eq!(effect, Effect::Keep);
    }

    #[test]
    fn positive_effect_of_no_change_on_a_redefined_service_respawns_its_task() {
        // Given a service the diff asks nothing of, but whose definition changed
        let action = Action::NoChange;

        // When the effect on its supervision task is decided
        let effect = effect_of(action, true, true);

        // Then the task is rebuilt so that it carries the new definition
        assert_eq!(effect, Effect::Respawn);
    }

    #[test]
    fn positive_effect_of_start_respawns_the_task_from_the_new_definition() {
        // Given a service the diff wants started
        let action = Action::Start;

        // When the effect on its supervision task is decided
        let effect = effect_of(action, true, true);

        // Then the task is rebuilt, which is what starts it under the new definition
        assert_eq!(effect, Effect::Respawn);
    }

    #[test]
    fn positive_effect_of_stop_on_a_service_still_defined_respawns_its_task() {
        // Given a service the diff wants stopped but that the new configuration still defines
        let action = Action::Stop;

        // When the effect on its supervision task is decided
        let effect = effect_of(action, true, true);

        // Then the task is rebuilt, disabled, so the service stops yet stays in the tray
        assert_eq!(effect, Effect::Respawn);
    }

    #[test]
    fn positive_effect_of_restart_respawns_the_task_from_the_new_definition() {
        // Given a service the diff wants restarted
        let action = Action::Restart;

        // When the effect on its supervision task is decided
        let effect = effect_of(action, true, true);

        // Then the task is rebuilt, which stops the old definition and starts the new one
        assert_eq!(effect, Effect::Respawn);
    }
}
