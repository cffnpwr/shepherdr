//! The menu bar tray icon, which is the whole of Shepherdr's controls.
//!
//! The tray owns no state: its menu is rebuilt from the [`Supervisor`]'s published states every
//! time they change, and every click is turned straight back into a supervisor operation, a
//! reload, a request for the log window, or an app exit.

mod menu;

use shepherdr_core::logging::error_chain;
use tauri::image::Image;
use tauri::tray::TrayIconBuilder;
use tauri::{AppHandle, Manager as _, async_runtime};

use crate::supervisor::{ServiceStates, Supervisor};
use crate::tray::menu::MenuAction;
use crate::window;

/// The identifier the tray icon is registered under, used to find it again when the menu has to be
/// rebuilt.
const TRAY_ID: &str = "shepherdr";

/// The menu bar icon, embedded in the binary so that it cannot go missing at runtime.
///
/// It is a template image: black on transparent. Paired with `icon_as_template`, macOS ignores the
/// colour and renders the alpha channel in the menu bar's own foreground colour, so the shape
/// follows the light and dark appearances without a second asset.
///
/// The image is drawn at a height of 18 points whatever its size, so it is authored at twice that
/// to stay sharp on a Retina display.
const ICON: &[u8] = include_bytes!("../icons/tray.png");

/// Installs the tray icon and keeps its menu in step with the supervisor.
///
/// The app must already manage a [`Supervisor`] when this is called.
///
/// # Errors
///
/// Returns an error when the tray icon, its image, or its initial menu cannot be created.
pub fn setup(app: &AppHandle) -> tauri::Result<()> {
    let supervisor = app.state::<Supervisor>().inner().clone();

    // Subscribing before building the initial menu is essential, not incidental: whatever the
    // supervisor has already published by the time this runs only ever reaches a receiver that
    // existed before it was sent. Subscribing first and reading the initial snapshot back out of
    // that same receiver (rather than a separate `supervisor.states()` call) guarantees nothing
    // published between "read the current state" and "start watching for the next change" is
    // missed: a publish before this point lands in the initial snapshot, and a publish after it
    // wakes the `changed()` loop below.
    let mut states = supervisor.subscribe();
    let initial = states.borrow_and_update().clone();

    let _tray = TrayIconBuilder::with_id(TRAY_ID)
        .icon(Image::from_bytes(ICON)?)
        .icon_as_template(true)
        .menu(&menu::build(app, &initial)?)
        .on_menu_event(|app, event| dispatch(app, event.id.as_ref()))
        .build(app)?;

    let app = app.clone();
    let _refresh = async_runtime::spawn(async move {
        while states.changed().await.is_ok() {
            let snapshot = states.borrow_and_update().clone();
            refresh(&app, &snapshot);
        }
    });
    Ok(())
}

/// Rebuilds the menu for a new set of states and hands it to the tray icon.
///
/// The whole menu is replaced rather than patched, because a reload can add and remove services,
/// not just change what the existing ones say.
fn refresh(app: &AppHandle, states: &ServiceStates) {
    let Some(tray) = app.tray_by_id(TRAY_ID) else {
        return;
    };
    match menu::build(app, states) {
        Ok(menu) => {
            if let Err(error) = tray.set_menu(Some(menu)) {
                log::error!("failed to update the tray menu: {}", error_chain(&error));
            }
        }
        Err(error) => log::error!("failed to build the tray menu: {}", error_chain(&error)),
    }
}

/// Carries out the click on the menu item with the given identifier.
fn dispatch(app: &AppHandle, id: &str) {
    let Some(action) = menu::action_of(id) else {
        log::warn!("ignored a click on the unknown tray menu item \"{id}\"");
        return;
    };
    let supervisor = app.state::<Supervisor>();
    match action {
        MenuAction::Start(name) => supervisor.start_service(&name),
        MenuAction::Stop(name) => supervisor.stop_service(&name),
        MenuAction::Restart(name) => supervisor.restart_service(&name),
        // A reload runs the stop sequence of everything it replaces, so it cannot be awaited here
        // on the main thread without freezing the menu for as long as the grace period.
        MenuAction::Reload => {
            let supervisor = supervisor.inner().clone();
            let _reload = async_runtime::spawn(async move {
                if let Err(error) = supervisor.reload().await {
                    log::error!(
                        "failed to reload the configuration: {}",
                        error_chain(&error)
                    );
                }
            });
        }
        MenuAction::OpenLogs => window::open_logs(app),
        // Every service is stopped from the `Exit` event this leads to, so there is nothing to do
        // here beyond asking the event loop to wind down.
        MenuAction::Quit => app.exit(0),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Height of the tray icon asset in pixels, being the 18 points it is drawn at on a display
    /// with a scale factor of two.
    const ICON_HEIGHT: u32 = 36;

    #[test]
    fn positive_icon_decodes_at_the_height_the_menu_bar_is_drawn_at() {
        // Given the embedded tray icon asset
        let image = Image::from_bytes(ICON).expect("the tray icon should decode");

        // When its height and buffer length are read
        let measurements = (image.height(), image.rgba().len());

        // Then it is authored at twice the 18 point drawing height, with four bytes per pixel
        let pixels = image.width() as usize * ICON_HEIGHT as usize;
        assert_eq!(measurements, (ICON_HEIGHT, pixels * 4));
    }

    #[test]
    fn positive_icon_carries_a_shape_in_its_alpha_channel() {
        // Given the embedded tray icon asset, which macOS draws from its alpha channel alone
        let image = Image::from_bytes(ICON).expect("the tray icon should decode");

        // When the fully opaque and the fully transparent pixels are counted
        let alpha = image.rgba().iter().skip(3).step_by(4);
        let (opaque, clear) = alpha.fold((0_usize, 0_usize), |(opaque, clear), &a| match a {
            u8::MAX => (opaque + 1, clear),
            0 => (opaque, clear + 1),
            _ => (opaque, clear),
        });

        // Then the asset is neither blank nor a solid block
        assert!(opaque > 0 && clear > 0, "opaque: {opaque}, clear: {clear}");
    }
}
