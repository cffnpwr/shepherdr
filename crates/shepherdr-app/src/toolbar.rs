//! The log window's title bar toolbar: a native `NSToolbar` carrying the service picker.
//!
//! The picker is an `NSPopUpButton` rather than a control inside the web view because the
//! toolbar's material is drawn by `AppKit` around a real toolbar item and cannot be reproduced from
//! the web view: attaching an `NSToolbar` in `NSWindowToolbarStyleUnified` is by itself what makes
//! macOS build the `NSGlassContainerView` / `NSGlassEffectView` pair around the item (measured on
//! macOS 27 in the mockup this module was ported from). The window keeps its ordinary title bar --
//! no `titlebarAppearsTransparent`, no `fullSizeContentView` -- so the web view stays below the
//! toolbar instead of sliding under it.
//!
//! The toolbar decides nothing for itself. Its item list is fixed at one item, the entries that
//! item offers are rebuilt from the [`Supervisor`]'s published states every time they change --
//! the same way [`crate::tray`] rebuilds its menu -- and every selection is passed straight on to
//! the frontend, as a [`Selection`] it can read as it starts up and an event carrying every change
//! after that.

mod picker;

use std::cell::RefCell;

use objc2::rc::Retained;
use objc2::runtime::{NSObject, NSObjectProtocol, ProtocolObject};
use objc2::{DefinedClass as _, MainThreadMarker, MainThreadOnly, define_class, msg_send, sel};
use objc2_app_kit::{
    NSPopUpButton,
    NSToolbar,
    NSToolbarDelegate,
    NSToolbarDisplayMode,
    NSToolbarItem,
    NSToolbarItemIdentifier,
    NSWindow,
    NSWindowToolbarStyle,
};
use objc2_foundation::{NSArray, NSString};
use shepherdr_core::log;
use shepherdr_core::logging::error_chain;
use tauri::{AppHandle, Emitter as _, Manager as _, Runtime, async_runtime};

use crate::logs::Selection;
use crate::supervisor::{ServiceStates, Supervisor};
use crate::window;

/// The toolbar's identifier, which is also what `AppKit` forms its autosave name from.
const TOOLBAR_ID: &str = "dev.cffnpwr.shepherdr.log-window";
/// The identifier of the one item the toolbar holds.
const PICKER_ITEM_ID: &str = "dev.cffnpwr.shepherdr.service-picker";
/// The event the frontend receives whenever the picked service changes.
///
/// The payload is the service name, or `null` when the configuration defines no service at all and
/// the picker is therefore empty. A bare name rather than an object: the selection is a single
/// value, and the log window has nothing else to say about it. The selection standing before the
/// frontend was listening is read from [`Selection`] instead.
const SELECTION_EVENT: &str = "service-selected";

/// What the toolbar delegate carries.
///
/// Neither the item nor the picker is among it. `AppKit` requires a fresh `NSToolbarItem` from
/// every delegate call -- "The toolbar becomes the owner of the returned item [...] Don't recycle
/// toolbar items; always provide a new instance, even if the toolbar previously asked for an item
/// with the same identifier"[^1] -- and an `NSView` belongs to one superview at a time, so the
/// picker has to be new each time too. What is kept here is what a new picker is built from.
///
/// [^1]: <https://developer.apple.com/documentation/appkit/nstoolbardelegate/toolbar(_:itemforitemidentifier:willbeinsertedintotoolbar:)>
struct Ivars {
    /// The window the toolbar belongs to, which is how the item the toolbar currently holds -- and
    /// the picker inside it -- is reached again after ownership of the item has passed over.
    window: Retained<NSWindow>,
    /// The handle the selection is published through.
    app: AppHandle,
    /// The names a picker offers. The supervisor publishes on every state change, but only a
    /// reload changes which services exist, so this is what keeps the ordinary case from tearing
    /// the entries down and building them back up under an open menu.
    shown: RefCell<Vec<String>>,
}

define_class!(
    // SAFETY:
    // - The superclass NSObject has no subclassing requirements.
    // - `ServiceToolbar` does not implement `Drop`.
    #[unsafe(super(NSObject))]
    // `NSToolbarDelegate` is a main-thread-only protocol, and this class builds AppKit views, so
    // instances of it must not leave the main thread either.
    #[thread_kind = MainThreadOnly]
    #[ivars = Ivars]
    struct ServiceToolbar;

    unsafe impl NSObjectProtocol for ServiceToolbar {}

    unsafe impl NSToolbarDelegate for ServiceToolbar {
        /// Builds the toolbar an item holding a picker, with no label of its own: labels would add
        /// an `NSToolbarLabelStack` under the item and grow the toolbar from 52pt to 66pt, and the
        /// display mode is `IconOnly` in any case.
        ///
        /// Both the item and its picker are new every call, as the framework requires (see
        /// [`Ivars`]). The picker is fully configured before it is returned, so what the toolbar
        /// puts on screen never passes through an empty state.
        #[unsafe(method_id(toolbar:itemForItemIdentifier:willBeInsertedIntoToolbar:))]
        fn item_for_identifier(
            &self,
            _toolbar: &NSToolbar,
            identifier: &NSToolbarItemIdentifier,
            _inserted: bool,
        ) -> Option<Retained<NSToolbarItem>> {
            (identifier.to_string() == PICKER_ITEM_ID).then(|| {
                let item = NSToolbarItem::initWithItemIdentifier(self.mtm().alloc(), identifier);
                item.setView(Some(&self.build_picker()));
                item
            })
        }

        #[unsafe(method_id(toolbarDefaultItemIdentifiers:))]
        fn default_item_identifiers(
            &self,
            _toolbar: &NSToolbar,
        ) -> Retained<NSArray<NSToolbarItemIdentifier>> {
            item_identifiers()
        }

        #[unsafe(method_id(toolbarAllowedItemIdentifiers:))]
        fn allowed_item_identifiers(
            &self,
            _toolbar: &NSToolbar,
        ) -> Retained<NSArray<NSToolbarItemIdentifier>> {
            item_identifiers()
        }
    }

    impl ServiceToolbar {
        /// The picker's action, sent by `AppKit` when the user picks a service.
        ///
        /// The sender is the picker that was clicked, which is how the selection is read without
        /// having to work out which of the pickers handed out is the live one.
        #[unsafe(method(serviceSelected:))]
        fn service_selected(&self, sender: Option<&NSPopUpButton>) {
            let selected = sender
                .and_then(NSPopUpButton::titleOfSelectedItem)
                .map(|title| title.to_string());
            publish(&self.ivars().app, selected);
        }
    }
);

impl ServiceToolbar {
    /// Builds the delegate for `window`'s toolbar.
    fn new(mtm: MainThreadMarker, app: AppHandle, window: Retained<NSWindow>) -> Retained<Self> {
        let this = mtm.alloc::<Self>().set_ivars(Ivars {
            window,
            app,
            shown: RefCell::new(Vec::new()),
        });
        unsafe { msg_send![super(this), init] }
    }

    /// Builds a picker offering the services currently known, with the published selection picked,
    /// wired to report what the user picks back here.
    fn build_picker(&self) -> Retained<NSPopUpButton> {
        let selected = self.ivars().app.state::<Selection>().current();
        let picker = picker::build(
            self.mtm(),
            &self.ivars().shown.borrow(),
            selected.as_deref(),
        );
        // SAFETY: a target is unowned, as target/action always is, and `DELEGATE` keeps this
        // object alive for as long as the process runs.
        unsafe {
            picker.setTarget(Some(self));
            picker.setAction(Some(sel!(serviceSelected:)));
        }
        picker
    }

    /// Takes `names` as the services on offer and brings the picker on screen, if there is one,
    /// into line with whatever that settles on.
    ///
    /// Everything the picker is set from comes out of [`settle_services`], so this cannot put a
    /// different list or a different selection on screen than the one the frontend was told about.
    fn set_services(&self, names: &[String]) {
        let Some(refresh) = settle_services(&self.ivars().app, &self.ivars().shown, names) else {
            return;
        };
        let Some(picker) = picker::find(&self.ivars().window, PICKER_ITEM_ID) else {
            return;
        };
        picker::fill(&picker, &refresh.names, refresh.selected.as_deref());
    }
}

thread_local! {
    /// The log window's toolbar delegate, once it has been installed.
    ///
    /// Both references `AppKit` keeps to it -- the toolbar's delegate and the picker's target -- are
    /// unowned, so something on this side has to own it; and the task watching the supervisor,
    /// which reaches it from another thread, needs a way back to it that does not carry a
    /// `Retained` across threads. A slot local to the main thread is both at once.
    static DELEGATE: RefCell<Option<Retained<ServiceToolbar>>> = const { RefCell::new(None) };
}

/// Installs the toolbar on the log window and keeps its picker in step with the supervisor.
///
/// The app must already manage a [`Supervisor`] when this is called, and this must run on the main
/// thread, which is where Tauri runs the setup hook.
///
/// A failure to install the toolbar is reported and otherwise left alone, as in [`crate::window`]:
/// the services and the tray, which are what the app is for, do not depend on the log window
/// having a toolbar.
pub fn setup(app: &AppHandle) {
    let Some(mtm) = MainThreadMarker::new() else {
        log::error!("the log window toolbar can only be installed on the main thread");
        return;
    };
    let Some(ns_window) = log_window(app, mtm) else {
        return;
    };

    let supervisor = app.state::<Supervisor>().inner().clone();
    // Subscribing before the first fill, exactly as the tray does: a snapshot read out of the
    // receiver cannot miss a publish that lands between reading the current states and starting to
    // watch for the next change.
    let mut states = supervisor.subscribe();
    let initial = service_names(&states.borrow_and_update());

    // Filled before the toolbar is attached, so that the very first item the toolbar asks for
    // already carries the services that are known.
    let delegate = ServiceToolbar::new(mtm, app.clone(), ns_window.clone());
    delegate.set_services(&initial);

    let toolbar = NSToolbar::initWithIdentifier(mtm.alloc(), &NSString::from_str(TOOLBAR_ID));
    toolbar.setDelegate(Some(ProtocolObject::from_ref(&*delegate)));
    // Icons only: with labels the toolbar grows by the height of an `NSToolbarLabelStack`, and the
    // one item the toolbar holds is a picker that names its own selection.
    toolbar.setDisplayMode(NSToolbarDisplayMode::IconOnly);
    ns_window.setToolbarStyle(NSWindowToolbarStyle::Unified);
    ns_window.setToolbar(Some(&toolbar));

    DELEGATE.with_borrow_mut(|slot| *slot = Some(delegate));

    let app = app.clone();
    let _refresh = async_runtime::spawn(async move {
        while states.changed().await.is_ok() {
            let names = service_names(&states.borrow_and_update());
            refresh(&app, names);
        }
    });
}

/// Hands a new set of service names to the picker, from whichever thread the state change arrived
/// on.
fn refresh(app: &AppHandle, names: Vec<String>) {
    let scheduled = app.run_on_main_thread(move || {
        DELEGATE.with_borrow(|slot| {
            if let Some(delegate) = slot.as_deref() {
                delegate.set_services(&names);
            }
        });
    });
    if let Err(error) = scheduled {
        log::error!(
            "failed to update the log window toolbar: {}",
            error_chain(&error)
        );
    }
}

/// The `NSWindow` behind the log window, or `None` when it cannot be reached.
fn log_window(app: &AppHandle, _mtm: MainThreadMarker) -> Option<Retained<NSWindow>> {
    let Some(webview) = app.get_webview_window(window::MAIN) else {
        log::error!("the log window is not available");
        return None;
    };
    let pointer = match webview.ns_window() {
        Ok(pointer) => pointer.cast::<NSWindow>(),
        Err(error) => {
            log::error!(
                "failed to reach the log window's NSWindow: {}",
                error_chain(&error)
            );
            return None;
        }
    };
    // SAFETY: Tauri hands back the `NSWindow` of a window it keeps alive for the life of the app,
    // and the `MainThreadMarker` this takes proves the caller may touch it.
    unsafe { Retained::retain(pointer) }
}

/// The identifiers of every item the toolbar holds, which is the picker and nothing else.
fn item_identifiers() -> Retained<NSArray<NSToolbarItemIdentifier>> {
    NSArray::from_retained_slice(&[NSString::from_str(PICKER_ITEM_ID)])
}

/// What a change to the services on offer leaves for the picker on screen to be set to.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Refresh {
    /// The entries the picker should offer.
    names: Vec<String>,
    /// The entry it should have picked.
    selected: Option<String>,
}

/// Takes `names` as the services on offer, recording them in `shown`, settling the selection on
/// one of them and publishing it; answers with what the picker on screen should then be set to.
///
/// A list `shown` already holds is left alone and answered with `None`: the supervisor publishes
/// on every service state change, but only a reload changes which services exist, and rebuilding
/// the entries in the ordinary case would tear them down under an open menu.
///
/// The whole list is replaced rather than patched, because a reload can add and remove services,
/// not just reorder them.
///
/// Publishing before answering is what keeps the picker and the frontend from being set from
/// different values: the only thing the caller has to put in the view is the [`Refresh`] handed
/// back here, which carries the selection that has already gone out. It is also why there being no
/// picker on screen at this moment is harmless -- an item built later reads the same published
/// selection back out of [`Selection`].
fn settle_services<R: Runtime>(
    app: &AppHandle<R>,
    shown: &RefCell<Vec<String>>,
    names: &[String],
) -> Option<Refresh> {
    if *shown.borrow() == names {
        return None;
    }
    let _replaced = shown.replace(names.to_vec());

    let previous = app.state::<Selection>().current();
    let selected = settled_selection(names, previous.as_deref());
    publish(app, selected.clone());

    Some(Refresh {
        names: names.to_vec(),
        selected,
    })
}

/// Publishes `selected` for the frontend, unless that is the selection already published.
///
/// The selection reaches [`Selection`] before the event goes out, so a frontend that reads it on
/// the back of the event never sees the older of the two.
fn publish<R: Runtime>(app: &AppHandle<R>, selected: Option<String>) {
    if !app.state::<Selection>().update(selected.clone()) {
        return;
    }
    if let Err(error) = app.emit(SELECTION_EVENT, selected) {
        log::error!(
            "failed to report the picked service: {}",
            error_chain(&error)
        );
    }
}

/// The entry a picker offering `names` settles on, given the `previous` selection.
///
/// `NSPopUpButton` selects the first entry as it is added, so a selection the new list no longer
/// offers becomes the first of that list. This is that rule written down separately, so that it
/// can be applied -- and reported to the frontend -- at a moment when no picker exists to read it
/// back from.
fn settled_selection(names: &[String], previous: Option<&str>) -> Option<String> {
    match previous {
        Some(previous) if names.iter().any(|name| name.as_str() == previous) => {
            Some(previous.to_owned())
        }
        _ => names.first().cloned(),
    }
}

/// Every service name in `states`, sorted.
///
/// The states arrive as a map, so the order they were declared in is not available; sorting keeps
/// the picker's entries from moving under the pointer every time a service changes state, and it
/// is the same order the tray menu lists services in.
fn service_names(states: &ServiceStates) -> Vec<String> {
    let mut names: Vec<String> = states.keys().cloned().collect();
    names.sort_unstable();
    names
}

#[cfg(test)]
mod tests {
    use tauri::App;
    use tauri::test::{MockRuntime, mock_app};

    use super::*;
    use crate::supervisor::ServiceState;

    /// An app standing in for the running one, with a [`Selection`] managed on it.
    ///
    /// The mock runtime is what makes this possible off the app's own event loop: nothing
    /// [`settle_services`] or [`publish`] does touches a window, so the runtime only has to exist.
    fn app() -> App<MockRuntime> {
        let app = mock_app();
        app.manage(Selection::default());
        app
    }

    #[test]
    fn negative_settle_services_asks_nothing_of_the_picker_the_second_time_the_same_list_arrives() {
        // Given a toolbar that has just taken on a list of services
        let app = app();
        let names = ["caddy".to_owned(), "redis".to_owned()];
        let shown = RefCell::new(Vec::new());
        let _taken = settle_services(app.handle(), &shown, &names);

        // When the supervisor publishes a state change that leaves the list alone
        let refresh = settle_services(app.handle(), &shown, &names);

        // Then nothing is asked of the picker, so a menu the user has open is not torn down
        assert_eq!(refresh, None);
    }

    #[test]
    fn negative_settle_services_leaves_the_published_selection_alone_when_the_list_did_not_change()
    {
        // Given a toolbar offering services, with one of them picked
        let app = app();
        let names = ["caddy".to_owned(), "redis".to_owned()];
        let shown = RefCell::new(names.to_vec());
        let _picked = app.state::<Selection>().update(Some("redis".to_owned()));

        // When the supervisor publishes a state change that leaves the list alone
        let _refresh = settle_services(app.handle(), &shown, &names);

        // Then the published selection is untouched, so the frontend hears nothing
        assert_eq!(app.state::<Selection>().current(), Some("redis".to_owned()));
    }

    #[test]
    fn positive_settle_services_settles_on_the_first_service_when_none_was_picked_before() {
        // Given a toolbar offering nothing yet
        let app = app();
        let shown = RefCell::new(Vec::new());

        // When the first services arrive
        let names = ["caddy".to_owned(), "redis".to_owned()];
        let refresh = settle_services(app.handle(), &shown, &names);

        // Then the first is settled on, and the picker is handed the same value that was published
        assert_eq!(
            (refresh, app.state::<Selection>().current()),
            (
                Some(Refresh {
                    names: names.to_vec(),
                    selected: Some("caddy".to_owned()),
                }),
                Some("caddy".to_owned())
            )
        );
    }

    #[test]
    fn positive_settle_services_keeps_a_service_the_new_list_still_offers() {
        // Given a toolbar offering services, with one of them picked
        let app = app();
        let shown = RefCell::new(vec!["caddy".to_owned(), "redis".to_owned()]);
        let _picked = app.state::<Selection>().update(Some("redis".to_owned()));

        // When a reload replaces the list with one that still offers it
        let names = ["postgres".to_owned(), "redis".to_owned()];
        let refresh = settle_services(app.handle(), &shown, &names);

        // Then it stays picked, and the picker is handed the same value that was published
        assert_eq!(
            (refresh, app.state::<Selection>().current()),
            (
                Some(Refresh {
                    names: names.to_vec(),
                    selected: Some("redis".to_owned()),
                }),
                Some("redis".to_owned())
            )
        );
    }

    #[test]
    fn positive_settle_services_moves_to_the_first_service_when_the_picked_one_is_gone() {
        // Given a toolbar offering services, with one of them picked
        let app = app();
        let shown = RefCell::new(vec!["caddy".to_owned(), "redis".to_owned()]);
        let _picked = app.state::<Selection>().update(Some("redis".to_owned()));

        // When a reload drops that service
        let names = ["mysql".to_owned(), "postgres".to_owned()];
        let refresh = settle_services(app.handle(), &shown, &names);

        // Then the first of the new list is settled on, published and handed over as one value
        assert_eq!(
            (refresh, app.state::<Selection>().current()),
            (
                Some(Refresh {
                    names: names.to_vec(),
                    selected: Some("mysql".to_owned()),
                }),
                Some("mysql".to_owned())
            )
        );
    }

    #[test]
    fn negative_settle_services_settles_on_nothing_when_the_last_service_is_gone() {
        // Given a toolbar offering one service, picked
        let app = app();
        let shown = RefCell::new(vec!["caddy".to_owned()]);
        let _picked = app.state::<Selection>().update(Some("caddy".to_owned()));

        // When a reload leaves the configuration with no service at all
        let names: [String; 0] = [];
        let refresh = settle_services(app.handle(), &shown, &names);

        // Then the picker is emptied and the frontend is told there is nothing to show
        assert_eq!(
            (refresh, app.state::<Selection>().current()),
            (
                Some(Refresh {
                    names: Vec::new(),
                    selected: None,
                }),
                None
            )
        );
    }

    #[test]
    fn positive_service_names_are_sorted_by_name() {
        // Given states published in no particular order
        let states: ServiceStates = [
            ("redis".to_owned(), ServiceState::Running),
            ("caddy".to_owned(), ServiceState::Stopped),
            ("postgres".to_owned(), ServiceState::Failed),
        ]
        .into_iter()
        .collect();

        // When the picker's entries are derived from them
        let names = service_names(&states);

        // Then they come out in name order
        assert_eq!(names, ["caddy", "postgres", "redis"]);
    }

    #[test]
    fn positive_service_names_of_no_services_is_empty() {
        // Given a configuration that defines no service
        let states = ServiceStates::default();

        // When the picker's entries are derived from them
        let names = service_names(&states);

        // Then the picker is left with nothing to offer
        assert!(names.is_empty());
    }

    #[test]
    fn positive_settled_selection_keeps_a_service_the_new_list_still_offers() {
        // Given a reload that leaves the picked service in place
        let names = ["caddy".to_owned(), "redis".to_owned()];

        // When the selection is settled against the new list
        let settled = settled_selection(&names, Some("redis"));

        // Then the user keeps looking at the service they picked
        assert_eq!(settled, Some("redis".to_owned()));
    }

    #[test]
    fn positive_settled_selection_falls_to_the_first_service_when_the_picked_one_is_gone() {
        // Given a reload that dropped the picked service
        let names = ["caddy".to_owned(), "redis".to_owned()];

        // When the selection is settled against the new list
        let settled = settled_selection(&names, Some("postgres"));

        // Then it lands on the first entry, which is what the picker itself would select
        assert_eq!(settled, Some("caddy".to_owned()));
    }

    #[test]
    fn positive_settled_selection_takes_the_first_service_when_nothing_was_picked_yet() {
        // Given the first services to arrive, with nothing picked before them
        let names = ["caddy".to_owned(), "redis".to_owned()];

        // When the selection is settled against them
        let settled = settled_selection(&names, None);

        // Then the first one is picked, so the log window has something to show at once
        assert_eq!(settled, Some("caddy".to_owned()));
    }

    #[test]
    fn negative_settled_selection_of_no_services_picks_nothing() {
        // Given a reload that left the configuration with no service at all
        let names: [String; 0] = [];

        // When the selection is settled against it
        let settled = settled_selection(&names, Some("caddy"));

        // Then nothing is picked, which is what an empty picker shows
        assert_eq!(settled, None);
    }
}
