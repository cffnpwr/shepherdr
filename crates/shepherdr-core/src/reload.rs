//! Computing the reload plan: the per-service action needed to reconcile running state with a
//! newly loaded config.
//!
//! This is a pure diff between the old and new [`Config`]. It does not spawn, stop, or touch any
//! live `Monitor`; applying the plan (spawning, stopping, restarting, and updating each service's
//! live desired state) is the responsibility of a higher layer that combines this with
//! [`crate::spawn`], [`crate::stop`], and [`crate::monitor`].

use rustc_hash::{FxHashMap, FxHashSet};

use crate::config::{Config, Service};

/// The action a reload should take for one service, decided from its `enabled` value on each
/// side and whether its definition changed between the old and new config.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    /// Start it under the new definition.
    Start,
    /// Stop the running instance.
    Stop,
    /// Stop the running instance and start it again under the new definition.
    Restart,
    /// Nothing to do.
    NoChange,
}

/// Computes the reload plan: one [`Action`] per service name appearing in `old`, `new`, or both.
///
/// Only the two configs are taken as input; a service's actual live running state is not
/// consulted. The decision for a name present in both configs is:
///
/// | old.enabled | new.enabled | definition changed | action |
/// | --- | --- | --- | --- |
/// | `true` | `true` | no | [`Action::NoChange`] |
/// | `true` | `true` | yes | [`Action::Restart`] |
/// | `true` | `false` | (either) | [`Action::Stop`] |
/// | `false` | `true` | (either) | [`Action::Start`] |
/// | `false` | `false` | (either) | [`Action::NoChange`] |
///
/// A name present only in `old` is [`Action::Stop`] when it was enabled there, else
/// [`Action::NoChange`]. A name present only in `new` is [`Action::Start`] when it is enabled
/// there, else [`Action::NoChange`].
#[must_use]
pub fn plan(old: &Config, new: &Config) -> FxHashMap<String, Action> {
    let old_services = index(&old.services);
    let new_services = index(&new.services);

    let mut names: FxHashSet<&str> = FxHashSet::default();
    names.extend(old_services.keys().copied());
    names.extend(new_services.keys().copied());

    names
        .into_iter()
        .map(|name| {
            let action = match (old_services.get(name), new_services.get(name)) {
                (Some(old_service), Some(new_service)) => {
                    action_for_matched(old_service, new_service)
                }
                (Some(old_service), None) => action_for_removed(old_service),
                (None, Some(new_service)) => action_for_added(new_service),
                (None, None) => unreachable!("name was collected from at least one of the maps"),
            };
            (name.to_owned(), action)
        })
        .collect()
}

/// Indexes services by name for lookup during the diff.
fn index(services: &[Service]) -> FxHashMap<&str, &Service> {
    services
        .iter()
        .map(|service| (service.name.as_str(), service))
        .collect()
}

/// Decides the action for a service present in both configs, per the decision table on [`plan`].
fn action_for_matched(old: &Service, new: &Service) -> Action {
    match (old.enabled, new.enabled) {
        (true, true) => {
            if definition_changed(old, new) {
                Action::Restart
            } else {
                Action::NoChange
            }
        }
        (true, false) => Action::Stop,
        (false, true) => Action::Start,
        (false, false) => Action::NoChange,
    }
}

/// Decides the action for a service present only in the old config.
fn action_for_removed(old: &Service) -> Action {
    if old.enabled {
        Action::Stop
    } else {
        Action::NoChange
    }
}

/// Decides the action for a service present only in the new config.
fn action_for_added(new: &Service) -> Action {
    if new.enabled {
        Action::Start
    } else {
        Action::NoChange
    }
}

/// Whether two service definitions, matched by name, differ in a way that requires a restart to
/// take effect: anything about how the process is run (`command`, `login_shell`, `env`, `cwd`).
/// `enabled` is handled separately by [`action_for_matched`], and `name` is the join key between
/// the old and new definitions, so it is always equal for a matched pair.
fn definition_changed(old: &Service, new: &Service) -> bool {
    old.command != new.command
        || old.login_shell != new.login_shell
        || old.env != new.env
        || old.cwd != new.cwd
}

#[cfg(test)]
mod tests {
    use rustc_hash::FxHashMap;

    use super::*;
    use crate::config::{LogConfig, RestartConfig, StopConfig};

    /// Builds a minimal service with the given name and enabled flag; every other field takes
    /// an arbitrary fixed value, overridden by the individual tests that need to vary it.
    fn service(name: &str, enabled: bool) -> Service {
        Service {
            name: name.to_owned(),
            command: vec!["cmd".to_owned()],
            login_shell: false,
            env: FxHashMap::default(),
            cwd: None,
            enabled,
        }
    }

    fn config(services: Vec<Service>) -> Config {
        Config {
            services,
            log: LogConfig::default(),
            restart: RestartConfig::default(),
            stop: StopConfig::default(),
        }
    }

    #[test]
    fn positive_plan_is_empty_for_two_empty_configs() {
        // Given two configs with no services
        let old = config(vec![]);
        let new = config(vec![]);

        // When the reload plan is computed
        let result = plan(&old, &new);

        // Then there is nothing to do
        assert_eq!(result, FxHashMap::default());
    }

    #[test]
    fn positive_plan_starts_an_added_enabled_service() {
        // Given a service present only in the new config, enabled
        let old = config(vec![]);
        let new = config(vec![service("added", true)]);

        // When the reload plan is computed
        let result = plan(&old, &new);

        // Then it is started
        assert_eq!(
            result,
            FxHashMap::from_iter([("added".to_owned(), Action::Start)])
        );
    }

    #[test]
    fn positive_plan_does_nothing_for_an_added_disabled_service() {
        // Given a service present only in the new config, disabled
        let old = config(vec![]);
        let new = config(vec![service("added", false)]);

        // When the reload plan is computed
        let result = plan(&old, &new);

        // Then there is nothing to run, so no action is taken
        assert_eq!(
            result,
            FxHashMap::from_iter([("added".to_owned(), Action::NoChange)])
        );
    }

    #[test]
    fn positive_plan_stops_a_removed_enabled_service() {
        // Given a service present only in the old config, enabled
        let old = config(vec![service("removed", true)]);
        let new = config(vec![]);

        // When the reload plan is computed
        let result = plan(&old, &new);

        // Then the running instance is stopped
        assert_eq!(
            result,
            FxHashMap::from_iter([("removed".to_owned(), Action::Stop)])
        );
    }

    #[test]
    fn positive_plan_does_nothing_for_a_removed_disabled_service() {
        // Given a service present only in the old config, already disabled
        let old = config(vec![service("removed", false)]);
        let new = config(vec![]);

        // When the reload plan is computed
        let result = plan(&old, &new);

        // Then nothing was running, so no action is taken
        assert_eq!(
            result,
            FxHashMap::from_iter([("removed".to_owned(), Action::NoChange)])
        );
    }

    #[test]
    fn positive_plan_does_nothing_when_enabled_and_unchanged() {
        // Given the same service, enabled, unchanged across the reload
        let svc = service("stable", true);
        let old = config(vec![svc.clone()]);
        let new = config(vec![svc]);

        // When the reload plan is computed
        let result = plan(&old, &new);

        // Then no action is taken
        assert_eq!(
            result,
            FxHashMap::from_iter([("stable".to_owned(), Action::NoChange)])
        );
    }

    #[test]
    fn positive_plan_does_nothing_when_disabled_and_unchanged() {
        // Given the same service, disabled, unchanged across the reload
        let svc = service("still-off", false);
        let old = config(vec![svc.clone()]);
        let new = config(vec![svc]);

        // When the reload plan is computed
        let result = plan(&old, &new);

        // Then no action is taken
        assert_eq!(
            result,
            FxHashMap::from_iter([("still-off".to_owned(), Action::NoChange)])
        );
    }

    #[test]
    fn positive_plan_restarts_a_service_whose_command_changed_while_staying_enabled() {
        // Given a service that stays enabled but whose command changes
        let old = config(vec![service("changed", true)]);
        let mut changed = service("changed", true);
        changed.command = vec!["new-cmd".to_owned()];
        let new = config(vec![changed]);

        // When the reload plan is computed
        let result = plan(&old, &new);

        // Then it is restarted to pick up the new definition
        assert_eq!(
            result,
            FxHashMap::from_iter([("changed".to_owned(), Action::Restart)])
        );
    }

    #[test]
    fn positive_plan_restarts_a_service_whose_login_shell_flag_changed_while_staying_enabled() {
        // Given a service that stays enabled but whose login_shell flag changes
        let old = config(vec![service("changed", true)]);
        let mut changed = service("changed", true);
        changed.login_shell = true;
        let new = config(vec![changed]);

        // When the reload plan is computed
        let result = plan(&old, &new);

        // Then it is restarted
        assert_eq!(
            result,
            FxHashMap::from_iter([("changed".to_owned(), Action::Restart)])
        );
    }

    #[test]
    fn positive_plan_restarts_a_service_whose_env_changed_while_staying_enabled() {
        // Given a service that stays enabled but whose env changes
        let old = config(vec![service("changed", true)]);
        let mut changed = service("changed", true);
        changed.env.insert("KEY".to_owned(), "value".to_owned());
        let new = config(vec![changed]);

        // When the reload plan is computed
        let result = plan(&old, &new);

        // Then it is restarted
        assert_eq!(
            result,
            FxHashMap::from_iter([("changed".to_owned(), Action::Restart)])
        );
    }

    #[test]
    fn positive_plan_restarts_a_service_whose_cwd_changed_while_staying_enabled() {
        // Given a service that stays enabled but whose cwd changes
        let old = config(vec![service("changed", true)]);
        let mut changed = service("changed", true);
        changed.cwd = Some("/tmp".into());
        let new = config(vec![changed]);

        // When the reload plan is computed
        let result = plan(&old, &new);

        // Then it is restarted
        assert_eq!(
            result,
            FxHashMap::from_iter([("changed".to_owned(), Action::Restart)])
        );
    }

    #[test]
    fn positive_plan_stops_a_service_that_became_disabled_even_if_its_definition_also_changed() {
        // Given a service that both changes command and flips to disabled
        let old = config(vec![service("changed", true)]);
        let mut changed = service("changed", false);
        changed.command = vec!["new-cmd".to_owned()];
        let new = config(vec![changed]);

        // When the reload plan is computed
        let result = plan(&old, &new);

        // Then the priority is stopping it: enabled=false wins over the definition change
        assert_eq!(
            result,
            FxHashMap::from_iter([("changed".to_owned(), Action::Stop)])
        );
    }

    #[test]
    fn positive_plan_starts_a_service_that_became_enabled_even_if_its_definition_also_changed() {
        // Given a service that both changes command and flips to enabled
        let old = config(vec![service("changed", false)]);
        let mut changed = service("changed", true);
        changed.command = vec!["new-cmd".to_owned()];
        let new = config(vec![changed]);

        // When the reload plan is computed
        let result = plan(&old, &new);

        // Then it is simply started under the new definition, not "restarted" (nothing was
        // running to restart)
        assert_eq!(
            result,
            FxHashMap::from_iter([("changed".to_owned(), Action::Start)])
        );
    }

    #[test]
    fn positive_plan_does_nothing_for_a_disabled_service_whose_definition_changed() {
        // Given a service that stays disabled but whose definition changes
        let old = config(vec![service("still-off", false)]);
        let mut changed = service("still-off", false);
        changed.command = vec!["new-cmd".to_owned()];
        let new = config(vec![changed]);

        // When the reload plan is computed
        let result = plan(&old, &new);

        // Then nothing is running and nothing should start, so there is no action
        assert_eq!(
            result,
            FxHashMap::from_iter([("still-off".to_owned(), Action::NoChange)])
        );
    }

    #[test]
    fn positive_plan_covers_every_service_across_both_configs_independently() {
        // Given a reload combining an addition, a removal, a restart, and an unchanged service
        // in one pass
        let old = config(vec![
            service("removed", true),
            service("changed", true),
            service("stable", true),
        ]);
        let mut changed = service("changed", true);
        changed.command = vec!["new-cmd".to_owned()];
        let new = config(vec![
            service("added", true),
            changed,
            service("stable", true),
        ]);

        // When the reload plan is computed
        let result = plan(&old, &new);

        // Then each service gets its own independent action
        let expected = FxHashMap::from_iter([
            ("added".to_owned(), Action::Start),
            ("removed".to_owned(), Action::Stop),
            ("changed".to_owned(), Action::Restart),
            ("stable".to_owned(), Action::NoChange),
        ]);
        assert_eq!(result, expected);
    }
}
