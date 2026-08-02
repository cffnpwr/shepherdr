//! The tray menu: the identifier and label of every item, and the assembly of the menu itself.
//!
//! Menu items carry no state of their own; a click comes back as the item's identifier, so the
//! identifiers are the only channel through which "restart this particular service" survives the
//! round trip. [`item_id`] writes them and [`action_of`] reads them back.

use std::str::FromStr;

use tauri::menu::{Menu, MenuBuilder, SubmenuBuilder};
use tauri::{AppHandle, Wry};

use crate::supervisor::{ServiceState, ServiceStates};

/// First field of a per-service item's identifier, distinguishing it from the app-wide ones.
const SERVICE_TAG: &str = "service";
/// Field separator inside a per-service item's identifier.
const SEPARATOR: char = ':';

/// The failure of reading back something no menu item ever wrote.
///
/// Carries nothing: the string that failed to parse is what the caller already has, and there is
/// only one way to fail.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct UnknownId;

/// What clicking a tray menu item is asking for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum MenuAction {
    /// Start the named service.
    Start(String),
    /// Stop the named service.
    Stop(String),
    /// Restart the named service.
    Restart(String),
    /// Reload the config file and apply the difference.
    Reload,
    /// Show the log window.
    OpenLogs,
    /// Quit the app.
    Quit,
}

/// An item that acts on the app as a whole, rather than on one service.
///
/// The counterpart of [`Operation`] for the tail of the menu, and the single place those items are
/// written down: the menu is built from [`AppItem::GROUPS`], an identifier is written by
/// [`AppItem::as_str`] and read back by its [`FromStr`] implementation, and [`AppItem::action`]
/// turns it into a [`MenuAction`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AppItem {
    /// Reload the config file and apply the difference.
    Reload,
    /// Show the log window.
    OpenLogs,
    /// Quit the app.
    Quit,
}

impl AppItem {
    /// Every app-wide item, in menu order, split into the runs that a separator divides. Quitting
    /// sits in a run of its own, set apart from the items above it.
    const GROUPS: [&'static [Self]; 2] = [&[Self::Reload, Self::OpenLogs], &[Self::Quit]];

    /// The item's identifier.
    const fn as_str(self) -> &'static str {
        match self {
            Self::Reload => "reload",
            Self::OpenLogs => "open-logs",
            Self::Quit => "quit",
        }
    }

    /// The label the item carries in the menu.
    const fn label(self) -> &'static str {
        match self {
            Self::Reload => "設定を再読み込み",
            Self::OpenLogs => "ログを開く",
            Self::Quit => "Shepherdrを終了",
        }
    }

    /// The action of carrying this item out.
    fn action(self) -> MenuAction {
        match self {
            Self::Reload => MenuAction::Reload,
            Self::OpenLogs => MenuAction::OpenLogs,
            Self::Quit => MenuAction::Quit,
        }
    }
}

impl FromStr for AppItem {
    type Err = UnknownId;

    /// Reads an identifier back, rejecting anything [`AppItem::as_str`] does not write.
    ///
    /// # Errors
    ///
    /// Returns [`UnknownId`] when `id` is not the identifier of any app-wide item.
    fn from_str(id: &str) -> Result<Self, Self::Err> {
        Self::GROUPS
            .into_iter()
            .flatten()
            .copied()
            .find(|item| item.as_str() == id)
            .ok_or(UnknownId)
    }
}

/// An operation offered under every service.
///
/// This is the single place the set of operations is written down: the menu is built from
/// [`Operation::ALL`], an identifier field is written by [`Operation::as_str`] and read back by its
/// [`FromStr`] implementation, and [`Operation::act_on`] turns it into a [`MenuAction`]. Adding an
/// operation cannot leave the writing and the reading sides out of step, because all four go
/// through this type and the last three match on it exhaustively.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Operation {
    /// Start the service.
    Start,
    /// Stop the service.
    Stop,
    /// Restart the service.
    Restart,
}

impl Operation {
    /// Every operation, in the order the menu lists them.
    const ALL: [Self; 3] = [Self::Start, Self::Stop, Self::Restart];

    /// The operation field written into an item's identifier.
    const fn as_str(self) -> &'static str {
        match self {
            Self::Start => "start",
            Self::Stop => "stop",
            Self::Restart => "restart",
        }
    }

    /// The label the item carries in the menu.
    const fn label(self) -> &'static str {
        match self {
            Self::Start => "起動",
            Self::Stop => "停止",
            Self::Restart => "再起動",
        }
    }

    /// The action of carrying this operation out on the named service.
    fn act_on(self, name: String) -> MenuAction {
        match self {
            Self::Start => MenuAction::Start(name),
            Self::Stop => MenuAction::Stop(name),
            Self::Restart => MenuAction::Restart(name),
        }
    }
}

impl FromStr for Operation {
    type Err = UnknownId;

    /// Reads an operation field back, rejecting anything [`Operation::as_str`] does not write.
    ///
    /// # Errors
    ///
    /// Returns [`UnknownId`] when `field` names no operation the menu offers.
    fn from_str(field: &str) -> Result<Self, Self::Err> {
        Self::ALL
            .into_iter()
            .find(|operation| operation.as_str() == field)
            .ok_or(UnknownId)
    }
}

/// Builds the whole tray menu for the given service states.
///
/// Each service becomes a submenu labelled with its name and current state, holding the three
/// operations, and the app-wide items follow. Services are ordered by name: the states arrive as a
/// map, so the order they were declared in is not available, and a stable order keeps items from
/// moving under the pointer every time one of them changes state.
///
/// # Errors
///
/// Returns the error from the underlying menu construction.
pub(super) fn build(app: &AppHandle, states: &ServiceStates) -> tauri::Result<Menu<Wry>> {
    let mut services: Vec<(&String, &ServiceState)> = states.iter().collect();
    services.sort_unstable_by_key(|&(name, _)| name);

    // Every run of items but the first is preceded by a separator. The services are that first run
    // whenever there are any; with none, the menu opens straight on the app-wide items.
    let mut separate = !services.is_empty();
    let mut menu = MenuBuilder::new(app);
    for (name, state) in services {
        let mut submenu = SubmenuBuilder::new(app, service_label(name, *state));
        for operation in Operation::ALL {
            submenu = submenu.text(item_id(operation, name), operation.label());
        }
        menu = menu.item(&submenu.build()?);
    }
    for group in AppItem::GROUPS {
        if separate {
            menu = menu.separator();
        }
        separate = true;
        for item in group {
            menu = menu.text(item.as_str(), item.label());
        }
    }

    menu.build()
}

/// Reads a clicked item's identifier back into the action it stands for. An identifier that no
/// item in the current menu writes yields `None`.
pub(super) fn action_of(id: &str) -> Option<MenuAction> {
    id.parse::<AppItem>()
        .ok()
        .map(AppItem::action)
        .or_else(|| service_action_of(id))
}

/// Reads a per-service identifier, `service:<operation>:<name>`.
///
/// The name is taken as the entire remainder, so a service name containing the separator still
/// round-trips through [`item_id`].
fn service_action_of(id: &str) -> Option<MenuAction> {
    let mut fields = id.splitn(3, SEPARATOR);
    if fields.next()? != SERVICE_TAG {
        return None;
    }
    let operation = fields.next()?.parse::<Operation>().ok()?;
    let name = fields.next()?.to_owned();
    Some(operation.act_on(name))
}

/// The identifier of one service's operation item.
fn item_id(operation: Operation, name: &str) -> String {
    let operation = operation.as_str();
    format!("{SERVICE_TAG}{SEPARATOR}{operation}{SEPARATOR}{name}")
}

/// The label of a service's own entry: its name followed by the state it is in.
fn service_label(name: &str, state: ServiceState) -> String {
    format!("{name}: {}", state_label(state))
}

/// How each service state reads in the menu.
fn state_label(state: ServiceState) -> &'static str {
    match state {
        ServiceState::Running => "実行中",
        ServiceState::Stopped => "停止",
        ServiceState::AwaitingRestart => "再起動待ち",
        ServiceState::Failed => "失敗",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn positive_service_label_shows_a_running_service_as_running() {
        // Given a service with a child process running
        let state = ServiceState::Running;

        // When its menu entry is labelled
        let label = service_label("herdr", state);

        // Then the label names it and says it is running
        assert_eq!(label, "herdr: 実行中");
    }

    #[test]
    fn positive_service_label_shows_a_stopped_service_as_stopped() {
        // Given a service that is not running and will not be started until asked
        let state = ServiceState::Stopped;

        // When its menu entry is labelled
        let label = service_label("herdr", state);

        // Then the label says it is stopped
        assert_eq!(label, "herdr: 停止");
    }

    #[test]
    fn positive_service_label_shows_a_backing_off_service_as_awaiting_a_restart() {
        // Given a service waiting out its restart backoff
        let state = ServiceState::AwaitingRestart;

        // When its menu entry is labelled
        let label = service_label("herdr", state);

        // Then the label says a restart is pending
        assert_eq!(label, "herdr: 再起動待ち");
    }

    #[test]
    fn positive_service_label_shows_a_service_past_its_failure_limit_as_failed() {
        // Given a service whose consecutive failures reached the limit
        let state = ServiceState::Failed;

        // When its menu entry is labelled
        let label = service_label("herdr", state);

        // Then the label says it failed
        assert_eq!(label, "herdr: 失敗");
    }

    #[test]
    fn positive_action_of_reads_back_the_start_item_of_a_service() {
        // Given the identifier written for a service's start item
        let id = item_id(Operation::Start, "herdr");

        // When the click is read back
        let action = action_of(&id);

        // Then it asks to start that service
        assert_eq!(action, Some(MenuAction::Start("herdr".to_owned())));
    }

    #[test]
    fn positive_action_of_reads_back_the_stop_item_of_a_service() {
        // Given the identifier written for a service's stop item
        let id = item_id(Operation::Stop, "herdr");

        // When the click is read back
        let action = action_of(&id);

        // Then it asks to stop that service
        assert_eq!(action, Some(MenuAction::Stop("herdr".to_owned())));
    }

    #[test]
    fn positive_action_of_reads_back_the_restart_item_of_a_service() {
        // Given the identifier written for a service's restart item
        let id = item_id(Operation::Restart, "herdr");

        // When the click is read back
        let action = action_of(&id);

        // Then it asks to restart that service
        assert_eq!(action, Some(MenuAction::Restart("herdr".to_owned())));
    }

    #[test]
    fn positive_every_operation_the_menu_offers_is_read_back_as_itself() {
        // Given every operation the menu puts under a service
        let operations = Operation::ALL;

        // When each one's identifier field is written and read back
        let parsed: Vec<Result<Operation, UnknownId>> = operations
            .into_iter()
            .map(|operation| operation.as_str().parse())
            .collect();

        // Then each yields the operation it was written from, so no operation can be added to the
        // menu without also being readable
        let expected: Vec<Result<Operation, UnknownId>> = operations.into_iter().map(Ok).collect();
        assert_eq!(parsed, expected);
    }

    #[test]
    fn positive_action_of_keeps_a_service_name_containing_the_separator_intact() {
        // Given a service whose name contains the identifier's own separator
        let id = item_id(Operation::Start, "group:member");

        // When the click is read back
        let action = action_of(&id);

        // Then the whole name survives the round trip
        assert_eq!(action, Some(MenuAction::Start("group:member".to_owned())));
    }

    #[test]
    fn positive_action_of_reads_the_reload_item() {
        // Given the reload item's identifier
        let id = AppItem::Reload.as_str();

        // When the click is read back
        let action = action_of(id);

        // Then it asks for a reload
        assert_eq!(action, Some(MenuAction::Reload));
    }

    #[test]
    fn positive_action_of_reads_the_open_logs_item() {
        // Given the log window item's identifier
        let id = AppItem::OpenLogs.as_str();

        // When the click is read back
        let action = action_of(id);

        // Then it asks for the log window
        assert_eq!(action, Some(MenuAction::OpenLogs));
    }

    #[test]
    fn positive_action_of_reads_the_quit_item() {
        // Given the quit item's identifier
        let id = AppItem::Quit.as_str();

        // When the click is read back
        let action = action_of(id);

        // Then it asks to quit
        assert_eq!(action, Some(MenuAction::Quit));
    }

    #[test]
    fn positive_every_app_item_the_menu_offers_is_read_back_as_itself() {
        // Given every app-wide item the menu lists, across all of its separated runs
        let items: Vec<AppItem> = AppItem::GROUPS.into_iter().flatten().copied().collect();

        // When each one's identifier is written and read back
        let parsed: Vec<Result<AppItem, UnknownId>> =
            items.iter().map(|item| item.as_str().parse()).collect();

        // Then each yields the item it was written from, so no item can be added to the menu
        // without also being readable
        let expected: Vec<Result<AppItem, UnknownId>> = items.iter().copied().map(Ok).collect();
        assert_eq!(parsed, expected);
    }

    #[test]
    fn negative_action_of_rejects_an_identifier_that_is_not_a_service_item() {
        // Given an identifier that no item of this menu writes
        let id = "something-else";

        // When the click is read back
        let action = action_of(id);

        // Then nothing is asked for
        assert_eq!(action, None);
    }

    #[test]
    fn negative_action_of_rejects_a_service_identifier_with_an_unknown_operation() {
        // Given a service identifier naming an operation the menu does not offer
        let id = "service:pause:herdr";

        // When the click is read back
        let action = action_of(id);

        // Then nothing is asked for
        assert_eq!(action, None);
    }

    #[test]
    fn negative_action_of_rejects_a_service_identifier_without_a_name() {
        // Given a service identifier whose name field is missing entirely
        let id = "service:start";

        // When the click is read back
        let action = action_of(id);

        // Then nothing is asked for
        assert_eq!(action, None);
    }
}
