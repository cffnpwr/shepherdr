//! The supervision task for a single service.
//!
//! One task per service owns that service's child process for its whole lifetime and drives the
//! cycle `spawn` -> record the cleanup state -> capture the logs -> wait for the exit ->
//! [`Monitor::record_exit`] -> back off and start again. It is the only place that touches the
//! child, so tray operations and app shutdown reach it as [`ServiceCommand`]s over a channel
//! rather than by sharing the child itself.

use std::sync::Arc;
use std::time::Duration;

use shepherdr_core::config::{LogConfig, RestartConfig, Service};
use shepherdr_core::logging::{self, CaptureHandle};
use shepherdr_core::monitor::{DesiredState, Monitor, RestartDecision};
use shepherdr_core::{spawn, state, stop};
use tokio::process::Child;
use tokio::sync::{mpsc, oneshot, watch};
use tokio::time::{Instant, sleep_until, timeout};

use crate::supervisor::{ServiceState, ServiceStates};

/// A control message for one service's supervision task.
pub(super) enum ServiceCommand {
    /// Start the service, resetting its failure counter and backoff.
    Start,
    /// Run the stop sequence and stay stopped.
    Stop,
    /// Run the stop sequence, then start again immediately.
    Restart,
    /// Run the stop sequence and end the task, signalling completion on the channel.
    Shutdown(oneshot::Sender<()>),
}

/// The supervisor's end of one supervision task.
pub(super) struct ServiceHandle {
    /// Unbounded so that sending a command never blocks or awaits: the tray and the shutdown path
    /// both send from contexts that must not stall, and the commands are only ever user-driven,
    /// so the queue cannot grow without bound.
    commands: mpsc::UnboundedSender<ServiceCommand>,
}

impl ServiceHandle {
    /// Delivers `command`, reporting whether the task was still alive to receive it.
    pub(super) fn send(&self, command: ServiceCommand) -> bool {
        self.commands.send(command).is_ok()
    }
}

/// The supervision task for one service.
pub(super) struct ServiceTask {
    /// The service definition this task runs.
    service: Service,
    /// The restart policy and failure state.
    monitor: Monitor,
    /// The log rotation limits applied to every capture.
    log: LogConfig,
    /// The stop sequence's grace period.
    grace_period: Duration,
    /// Incoming control messages.
    commands: mpsc::UnboundedReceiver<ServiceCommand>,
    /// Where this task publishes its [`ServiceState`].
    states: Arc<watch::Sender<ServiceStates>>,
    /// The child process, while one is running.
    running: Option<RunningChild>,
}

/// A running child process and the bookkeeping tied to that particular run.
struct RunningChild {
    /// The child process itself.
    child: Child,
    /// The log capture tasks reading its stdout and stderr. `None` when the capture could not be
    /// set up; the service still runs, only its output is lost.
    capture: Option<CaptureHandle>,
    /// When it was spawned, used to measure the uptime [`Monitor::record_exit`] judges.
    started_at: Instant,
}

/// What the task woke up for.
enum Event {
    /// The running child exited on its own.
    Exited,
    /// The restart backoff elapsed.
    BackoffElapsed,
    /// A control message arrived.
    Command(ServiceCommand),
    /// The supervisor dropped this task's handle without shutting it down.
    Closed,
}

impl ServiceTask {
    /// Builds the task for `service` together with the handle used to control it.
    pub(super) fn new(
        service: Service,
        restart: RestartConfig,
        log: LogConfig,
        grace_period: Duration,
        states: Arc<watch::Sender<ServiceStates>>,
    ) -> (ServiceHandle, Self) {
        let (sender, commands) = mpsc::unbounded_channel();
        let monitor = Monitor::new(initial_desired_state(service.enabled), restart);
        let task = Self {
            service,
            monitor,
            log,
            grace_period,
            commands,
            states,
            running: None,
        };
        (ServiceHandle { commands: sender }, task)
    }

    /// Runs the supervision loop until the task is shut down.
    pub(super) async fn run(mut self) {
        let mut restart_at = if self.monitor.desired_state() == DesiredState::Running {
            self.start_child()
        } else {
            self.publish(ServiceState::Stopped);
            None
        };

        loop {
            match self.next_event(restart_at).await {
                Event::Exited => restart_at = self.handle_exit().await,
                Event::BackoffElapsed => restart_at = self.start_child(),
                Event::Command(ServiceCommand::Start) => {
                    self.monitor.start();
                    restart_at = self.start_child();
                }
                Event::Command(ServiceCommand::Stop) => {
                    self.monitor.stop();
                    self.stop_child().await;
                    self.publish(ServiceState::Stopped);
                    restart_at = None;
                }
                Event::Command(ServiceCommand::Restart) => {
                    self.stop_child().await;
                    self.monitor.start();
                    restart_at = self.start_child();
                }
                Event::Command(ServiceCommand::Shutdown(done)) => {
                    self.stop_child().await;
                    let _received = done.send(());
                    return;
                }
                Event::Closed => {
                    self.stop_child().await;
                    return;
                }
            }
        }
    }

    /// Waits for whatever can happen next, given whether a child is running and whether a restart
    /// is pending.
    ///
    /// Every branch is cancel-safe, so losing a race never loses a child exit or a command.
    async fn next_event(&mut self, restart_at: Option<Instant>) -> Event {
        match (self.running.as_mut(), restart_at) {
            (Some(running), _) => {
                tokio::select! {
                    result = running.child.wait() => {
                        if let Err(error) = result {
                            eprintln!(
                                "shepherdr: failed to wait for service \"{}\": {error}",
                                self.service.name
                            );
                        }
                        Event::Exited
                    }
                    command = self.commands.recv() => {
                        command.map_or(Event::Closed, Event::Command)
                    }
                }
            }
            (None, Some(at)) => {
                tokio::select! {
                    () = sleep_until(at) => Event::BackoffElapsed,
                    command = self.commands.recv() => {
                        command.map_or(Event::Closed, Event::Command)
                    }
                }
            }
            (None, None) => self.commands.recv().await.map_or(Event::Closed, Event::Command),
        }
    }

    /// Spawns the child process and starts capturing its output.
    ///
    /// Returns the instant a restart falls due, which only happens when the spawn itself failed: a
    /// failed spawn is fed to the monitor as a zero-uptime exit, so it counts towards the failure
    /// limit exactly like a program that starts and immediately dies. Does nothing when a child is
    /// already running.
    fn start_child(&mut self) -> Option<Instant> {
        if self.running.is_some() {
            return None;
        }

        let mut child = match spawn::spawn(&self.service) {
            Ok(child) => child,
            Err(error) => {
                eprintln!(
                    "shepherdr: failed to spawn service \"{}\": {error}",
                    self.service.name
                );
                return self.record_exit(Duration::ZERO);
            }
        };
        if let Err(error) = state::record(&self.service.name, &child) {
            eprintln!(
                "shepherdr: failed to record the cleanup state of \"{}\": {error}",
                self.service.name
            );
        }
        let capture = match logging::capture(
            &self.service.name,
            self.log.max_size,
            self.log.max_generations,
            &mut child,
        ) {
            Ok(capture) => Some(capture),
            Err(error) => {
                eprintln!(
                    "shepherdr: failed to capture the logs of \"{}\": {error}",
                    self.service.name
                );
                None
            }
        };

        self.running = Some(RunningChild {
            child,
            capture,
            started_at: Instant::now(),
        });
        self.publish(ServiceState::Running);
        None
    }

    /// Handles a child that exited on its own, returning the instant its restart falls due.
    async fn handle_exit(&mut self) -> Option<Instant> {
        let uptime = self.finish_run().await;
        self.record_exit(uptime)
    }

    /// Runs the stop sequence against the running child, if there is one.
    ///
    /// `SIGTERM` and, after the grace period, `SIGKILL` to the service's process group, then the
    /// cleanup state record is dropped and the log capture is drained.
    async fn stop_child(&mut self) {
        if let Some(running) = self.running.as_mut()
            && let Err(error) = stop::stop(&mut running.child, self.grace_period).await
        {
            eprintln!(
                "shepherdr: failed to stop service \"{}\": {error}",
                self.service.name
            );
        }
        let _uptime = self.finish_run().await;
    }

    /// Closes out a run whose child has already been reaped: the cleanup state record is dropped
    /// and the log capture is drained. Returns how long the run lasted.
    ///
    /// Dropping the record here, rather than only after a deliberate stop, keeps
    /// [`shepherdr_core::state`]'s pairing intact: the record protects a process that may still be
    /// alive, and this one is known to be gone.
    ///
    /// Draining the capture is bounded by the grace period. The readers only see EOF once every
    /// holder of the write end is gone, and a descendant that made its own process group survives
    /// the stop sequence and keeps that pipe open; waiting on it unbounded would hang the app on
    /// its way out. Giving up abandons the reader tasks, which keep appending whatever that
    /// descendant still writes.
    async fn finish_run(&mut self) -> Duration {
        let Some(running) = self.running.take() else {
            return Duration::ZERO;
        };
        if let Err(error) = state::forget(&self.service.name) {
            eprintln!(
                "shepherdr: failed to drop the cleanup state of \"{}\": {error}",
                self.service.name
            );
        }
        if let Some(capture) = running.capture {
            match timeout(self.grace_period, capture.join()).await {
                Ok(Ok(())) => {}
                Ok(Err(error)) => eprintln!(
                    "shepherdr: failed to capture the logs of \"{}\": {error}",
                    self.service.name
                ),
                Err(_elapsed) => eprintln!(
                    "shepherdr: gave up draining the logs of \"{}\": something still holds its output pipe",
                    self.service.name
                ),
            }
        }
        running.started_at.elapsed()
    }

    /// Feeds an exit of the given uptime to the monitor, publishes the resulting state, and
    /// returns the instant the restart falls due.
    fn record_exit(&mut self, uptime: Duration) -> Option<Instant> {
        let (state, backoff) = outcome_of(self.monitor.record_exit(uptime));
        self.publish(state);
        backoff.map(|delay| Instant::now() + delay)
    }

    /// Publishes this service's state to every subscriber.
    fn publish(&self, state: ServiceState) {
        self.states.send_modify(|states| {
            let _previous = states.insert(self.service.name.clone(), state);
        });
    }
}

/// The desired state a service starts in.
///
/// The desired state is never persisted, so it is derived from `enabled` on every app start and
/// from nothing else.
fn initial_desired_state(enabled: bool) -> DesiredState {
    if enabled {
        DesiredState::Running
    } else {
        DesiredState::Stopped
    }
}

/// The state to publish for a [`RestartDecision`], and the delay to wait before starting again.
fn outcome_of(decision: RestartDecision) -> (ServiceState, Option<Duration>) {
    match decision {
        RestartDecision::RestartAfter(delay) => (ServiceState::Restarting, Some(delay)),
        RestartDecision::Failed => (ServiceState::Failed, None),
        RestartDecision::Stopped => (ServiceState::Stopped, None),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn positive_initial_desired_state_is_running_for_an_enabled_service() {
        // Given a service that is enabled in the config file
        let enabled = true;

        // When its initial desired state is derived
        let desired = initial_desired_state(enabled);

        // Then it is expected to run
        assert_eq!(desired, DesiredState::Running);
    }

    #[test]
    fn positive_initial_desired_state_is_stopped_for_a_disabled_service() {
        // Given a service that is disabled in the config file
        let enabled = false;

        // When its initial desired state is derived
        let desired = initial_desired_state(enabled);

        // Then it is expected to stay stopped
        assert_eq!(desired, DesiredState::Stopped);
    }

    #[test]
    fn positive_outcome_of_a_restart_decision_waits_out_the_given_backoff() {
        // Given a decision to restart after a delay
        let decision = RestartDecision::RestartAfter(Duration::from_secs(3));

        // When it is turned into an outcome
        let outcome = outcome_of(decision);

        // Then the service is reported as awaiting a restart, due after that same delay
        assert_eq!(
            outcome,
            (ServiceState::Restarting, Some(Duration::from_secs(3)))
        );
    }

    #[test]
    fn positive_outcome_of_a_failed_decision_stops_restarting() {
        // Given a decision that the service reached its failure limit
        let decision = RestartDecision::Failed;

        // When it is turned into an outcome
        let outcome = outcome_of(decision);

        // Then the service is reported as failed and no restart is scheduled
        assert_eq!(outcome, (ServiceState::Failed, None));
    }

    #[test]
    fn positive_outcome_of_a_stopped_decision_stops_restarting() {
        // Given a decision that the service is meant to stay stopped
        let decision = RestartDecision::Stopped;

        // When it is turned into an outcome
        let outcome = outcome_of(decision);

        // Then the service is reported as stopped and no restart is scheduled
        assert_eq!(outcome, (ServiceState::Stopped, None));
    }
}
