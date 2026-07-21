//! Monitoring, restarting, and the failed state for a supervised service.
//!
//! This is a pure state machine for deciding whether and when to restart a
//! service after its child process exits. It does not perform the spawning or
//! the exit detection (`wait`) itself; those are the responsibility of a
//! higher layer that combines this with [`crate::spawn`]. The restart policy
//! (backoff, thresholds, failure limit) is supplied by the caller as a
//! [`RestartConfig`], which is derived from the optional `[restart]` section
//! of the config file (falling back to its defaults when omitted or absent).

use std::time::Duration;

use crate::config::RestartConfig;

/// The run state the user wants for a service.
///
/// Not persisted. On app startup this is always derived from `enabled` in
/// `config.toml`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DesiredState {
    /// Keep it running. Restart it when it exits.
    Running,
    /// Keep it stopped. Do not restart it when it exits.
    Stopped,
}

/// The action to take right after a child process exits, returned by
/// [`Monitor::record_exit`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RestartDecision {
    /// Restart after the given delay.
    RestartAfter(Duration),
    /// Consecutive failures reached the limit and the service transitioned to
    /// the failed state. Do not auto-restart.
    Failed,
    /// The desired state is stopped, so do not restart.
    Stopped,
}

/// Holds the restart policy and failure state for a single service.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Monitor {
    desired: DesiredState,
    restart: RestartConfig,
    consecutive_failures: u32,
    backoff: Duration,
    failed: bool,
}

impl Monitor {
    /// Initializes the monitor with the given desired state and restart policy.
    ///
    /// On app startup, pass the desired state derived from the service's
    /// `enabled` setting and the `restart` policy from the config file's
    /// `[restart]` section (or [`RestartConfig::default`] if it is absent).
    #[must_use]
    pub fn new(desired: DesiredState, restart: RestartConfig) -> Self {
        Self {
            desired,
            backoff: restart.initial_backoff,
            restart,
            consecutive_failures: 0,
            failed: false,
        }
    }

    /// The current desired state.
    #[must_use]
    pub fn desired_state(&self) -> DesiredState {
        self.desired
    }

    /// Whether the service has already transitioned to the failed state.
    #[must_use]
    pub fn is_failed(&self) -> bool {
        self.failed
    }

    /// The current number of consecutive failures.
    #[must_use]
    pub fn consecutive_failures(&self) -> u32 {
        self.consecutive_failures
    }

    /// Records that the child process exited and returns the action to take.
    ///
    /// `uptime` is the elapsed time from start to exit. The exit code or
    /// signal is not considered; only the fact that it exited and how long it
    /// ran determine whether and when to restart.
    ///
    /// When the desired state is stopped, this unconditionally returns
    /// [`RestartDecision::Stopped`]. Otherwise, an exit with `uptime` below
    /// the configured failure threshold counts as a consecutive failure; once
    /// that count reaches the configured limit, the service transitions to
    /// the failed state and this returns [`RestartDecision::Failed`]. Short
    /// of that limit, this returns [`RestartDecision::RestartAfter`] with a
    /// delay governed by the exponential backoff, which resets to its
    /// configured initial interval once a run reaches the configured
    /// stable-uptime threshold and otherwise grows by the configured
    /// multiplier, capped at the configured maximum.
    #[must_use]
    pub fn record_exit(&mut self, uptime: Duration) -> RestartDecision {
        if self.desired == DesiredState::Stopped {
            return RestartDecision::Stopped;
        }

        if uptime < self.restart.failure_uptime_threshold {
            self.consecutive_failures += 1;
        } else {
            self.consecutive_failures = 0;
        }

        if self.consecutive_failures >= self.restart.max_consecutive_failures {
            self.failed = true;
            return RestartDecision::Failed;
        }

        let delay = self.backoff;
        self.backoff = if uptime >= self.restart.stable_uptime {
            self.restart.initial_backoff
        } else {
            self.backoff
                .checked_mul(self.restart.backoff_multiplier)
                .map_or(self.restart.max_backoff, |grown| {
                    grown.min(self.restart.max_backoff)
                })
        };
        RestartDecision::RestartAfter(delay)
    }

    /// Manual start from the tray. Moves the desired state back to running
    /// and resets the consecutive-failure counter and the backoff to their
    /// initial state. Retrying from the failed state goes through this path.
    pub fn start(&mut self) {
        self.desired = DesiredState::Running;
        self.consecutive_failures = 0;
        self.backoff = self.restart.initial_backoff;
        self.failed = false;
    }

    /// Manual stop from the tray. Moves the desired state to stopped,
    /// excluding the service from restart. Leaves the consecutive-failure
    /// counter, the backoff, and the failed state unchanged.
    pub fn stop(&mut self) {
        self.desired = DesiredState::Stopped;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn positive_record_exit_does_not_restart_when_desired_state_is_stopped() {
        // Given a monitor whose desired state is stopped
        let mut monitor = Monitor::new(DesiredState::Stopped, RestartConfig::default());

        // When an exit is recorded, even with an uptime that would otherwise be a failure
        let decision = monitor.record_exit(Duration::ZERO);

        // Then it reports Stopped and leaves the counters untouched
        assert_eq!(decision, RestartDecision::Stopped);
        assert_eq!(
            monitor,
            Monitor {
                desired: DesiredState::Stopped,
                restart: RestartConfig::default(),
                consecutive_failures: 0,
                backoff: RestartConfig::default().initial_backoff,
                failed: false,
            }
        );
    }

    #[test]
    fn positive_record_exit_counts_a_fast_exit_as_a_failure_and_grows_the_backoff() {
        // Given a running monitor
        let restart = RestartConfig::default();
        let mut monitor = Monitor::new(DesiredState::Running, restart);

        // When it exits before the failure-uptime threshold
        let decision = monitor.record_exit(
            restart
                .failure_uptime_threshold
                .checked_sub(Duration::from_secs(1))
                .expect("threshold should be at least one second"),
        );

        // Then it restarts after the (pre-growth) initial backoff, counts one failure, and
        // grows the backoff for the next exit
        assert_eq!(
            decision,
            RestartDecision::RestartAfter(restart.initial_backoff)
        );
        assert_eq!(
            monitor,
            Monitor {
                desired: DesiredState::Running,
                restart,
                consecutive_failures: 1,
                backoff: restart.initial_backoff * restart.backoff_multiplier,
                failed: false,
            }
        );
    }

    #[test]
    fn positive_record_exit_marks_the_service_failed_after_consecutive_fast_exits() {
        // Given a running monitor that already accumulated one less than the failure limit
        let restart = RestartConfig::default();
        let mut monitor = Monitor::new(DesiredState::Running, restart);
        for _ in 0..restart.max_consecutive_failures - 1 {
            let _: RestartDecision = monitor.record_exit(Duration::ZERO);
        }

        // When one more fast exit reaches the consecutive-failure limit
        let decision = monitor.record_exit(Duration::ZERO);

        // Then it transitions to the failed state and stops proposing restarts
        assert_eq!(decision, RestartDecision::Failed);
        assert_eq!(
            monitor.consecutive_failures(),
            restart.max_consecutive_failures
        );
        assert!(monitor.is_failed());
    }

    #[test]
    fn positive_record_exit_resets_the_failure_count_on_an_exit_past_the_threshold() {
        // Given a monitor with one counted failure
        let restart = RestartConfig::default();
        let mut monitor = Monitor::new(DesiredState::Running, restart);
        let _: RestartDecision = monitor.record_exit(Duration::ZERO);

        // When it exits past the failure-uptime threshold but before stable uptime
        let decision = monitor.record_exit(restart.failure_uptime_threshold);

        // Then the failure count resets to zero, but the backoff still grows since the run
        // was not stable
        assert_eq!(
            decision,
            RestartDecision::RestartAfter(restart.initial_backoff * restart.backoff_multiplier)
        );
        assert_eq!(
            monitor,
            Monitor {
                desired: DesiredState::Running,
                restart,
                consecutive_failures: 0,
                backoff: restart.initial_backoff
                    * restart.backoff_multiplier
                    * restart.backoff_multiplier,
                failed: false,
            }
        );
    }

    #[test]
    fn positive_record_exit_resets_the_backoff_after_stable_uptime() {
        // Given a monitor whose backoff has already grown from an earlier fast exit
        let restart = RestartConfig::default();
        let mut monitor = Monitor::new(DesiredState::Running, restart);
        let _: RestartDecision = monitor.record_exit(Duration::ZERO);
        let grown_backoff = monitor.backoff;

        // When it exits after running at least the stable-uptime duration
        let decision = monitor.record_exit(restart.stable_uptime);

        // Then this restart still uses the grown backoff, but the next one resets to initial
        assert_eq!(decision, RestartDecision::RestartAfter(grown_backoff));
        assert_eq!(
            monitor,
            Monitor {
                desired: DesiredState::Running,
                restart,
                consecutive_failures: 0,
                backoff: restart.initial_backoff,
                failed: false,
            }
        );
    }

    #[test]
    fn positive_record_exit_caps_the_backoff_at_the_configured_maximum() {
        // Given a monitor that keeps exiting past the failure threshold but short of stable
        // uptime, so the backoff keeps growing without ever counting as a failure
        let restart = RestartConfig::default();
        let mut monitor = Monitor::new(DesiredState::Running, restart);
        let mid_band_uptime = restart.failure_uptime_threshold;

        // When it exits enough times to exceed the cap through repeated doubling
        for _ in 0..10 {
            let _: RestartDecision = monitor.record_exit(mid_band_uptime);
        }

        // Then the backoff is clamped to the maximum and never counted as a failure
        assert_eq!(
            monitor,
            Monitor {
                desired: DesiredState::Running,
                restart,
                consecutive_failures: 0,
                backoff: restart.max_backoff,
                failed: false,
            }
        );
    }

    #[test]
    fn positive_start_resets_desired_state_failure_count_and_backoff() {
        // Given a monitor that failed while stopped and accumulated backoff growth
        let restart = RestartConfig::default();
        let mut monitor = Monitor::new(DesiredState::Running, restart);
        for _ in 0..restart.max_consecutive_failures {
            let _: RestartDecision = monitor.record_exit(Duration::ZERO);
        }
        monitor.stop();

        // When it is manually started from the tray
        monitor.start();

        // Then it is back to the same state as a freshly created running monitor
        assert_eq!(monitor, Monitor::new(DesiredState::Running, restart));
    }

    #[test]
    fn positive_stop_changes_only_the_desired_state() {
        // Given a running monitor with accumulated failures and backoff growth
        let restart = RestartConfig::default();
        let mut monitor = Monitor::new(DesiredState::Running, restart);
        let _: RestartDecision = monitor.record_exit(Duration::ZERO);
        let before_stop = monitor.clone();

        // When it is stopped from the tray
        monitor.stop();

        // Then only the desired state changes; counters are left as they were
        assert_eq!(
            monitor,
            Monitor {
                desired: DesiredState::Stopped,
                ..before_stop
            }
        );
    }

    #[test]
    fn positive_record_exit_uses_the_configured_restart_policy_instead_of_defaults() {
        // Given a monitor built with a custom restart policy (lower failure limit and
        // threshold than the defaults)
        let restart = RestartConfig {
            initial_backoff: Duration::from_millis(100),
            max_backoff: Duration::from_secs(1),
            backoff_multiplier: 3,
            stable_uptime: Duration::from_secs(10),
            failure_uptime_threshold: Duration::from_secs(1),
            max_consecutive_failures: 2,
        };
        let mut monitor = Monitor::new(DesiredState::Running, restart);

        // When it exits twice before the custom failure threshold
        let first = monitor.record_exit(Duration::from_millis(500));
        let second = monitor.record_exit(Duration::from_millis(500));

        // Then the first restart uses the custom initial backoff and the second reaches the
        // custom (lower) failure limit instead of the default one
        assert_eq!(
            first,
            RestartDecision::RestartAfter(Duration::from_millis(100))
        );
        assert_eq!(second, RestartDecision::Failed);
        assert!(monitor.is_failed());
    }
}
