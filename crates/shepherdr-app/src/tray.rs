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

/// Side length of the generated tray icon, in pixels.
const ICON_SIZE: u32 = 32;
/// Radius of the disc drawn on the tray icon, in half-pixels, so 13 pixels.
const ICON_RADIUS: i64 = 26;

/// Installs the tray icon and keeps its menu in step with the supervisor.
///
/// The app must already manage a [`Supervisor`] when this is called.
///
/// # Errors
///
/// Returns an error when the tray icon or its initial menu cannot be created.
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
        .icon(icon())
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

/// Draws the tray icon: an opaque black disc on a transparent field, generated rather than loaded.
///
/// Paired with `icon_as_template`, macOS ignores the colour and renders the alpha channel in the
/// menu bar's own foreground colour, so the shape follows the light and dark appearances without a
/// second asset.
fn icon() -> Image<'static> {
    let mut rgba = Vec::new();
    for y in 0..ICON_SIZE {
        for x in 0..ICON_SIZE {
            // Measured in half-pixels, a pixel's centre lands on an integer and the disc needs no
            // rounding: the centre of the image is at (ICON_SIZE, ICON_SIZE).
            let dx = i64::from(x) * 2 + 1 - i64::from(ICON_SIZE);
            let dy = i64::from(y) * 2 + 1 - i64::from(ICON_SIZE);
            let alpha = if dx * dx + dy * dy <= ICON_RADIUS * ICON_RADIUS {
                u8::MAX
            } else {
                0
            };
            rgba.extend_from_slice(&[0, 0, 0, alpha]);
        }
    }
    Image::new_owned(rgba, ICON_SIZE, ICON_SIZE)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn positive_icon_covers_the_declared_size_with_one_rgba_pixel_each() {
        // Given the generated tray icon
        let image = icon();

        // When its dimensions and buffer length are read
        let dimensions = (image.width(), image.height(), image.rgba().len());

        // Then the buffer holds exactly four bytes per pixel of the declared size
        let pixels = ICON_SIZE as usize * ICON_SIZE as usize;
        assert_eq!(dimensions, (ICON_SIZE, ICON_SIZE, pixels * 4));
    }

    #[test]
    fn positive_icon_is_opaque_at_its_centre_and_transparent_at_its_corner() {
        // Given the generated tray icon
        let image = icon();
        let rgba = image.rgba();
        let alpha_at = |x: u32, y: u32| {
            let index = (y as usize * ICON_SIZE as usize + x as usize) * 4 + 3;
            rgba.get(index).copied()
        };

        // When the alpha of the centre pixel and of a corner pixel are read
        let centre = alpha_at(ICON_SIZE / 2, ICON_SIZE / 2);
        let corner = alpha_at(0, 0);

        // Then the disc is drawn in the middle and the corner stays clear
        assert_eq!((centre, corner), (Some(u8::MAX), Some(0)));
    }
}
