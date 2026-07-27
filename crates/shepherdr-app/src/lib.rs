//! Tauri application shell for Shepherdr.

pub mod supervisor;
mod tray;
mod window;

use tauri::{ActivationPolicy, Builder, Manager as _, RunEvent, async_runtime};

use crate::supervisor::Supervisor;

/// Starts the Tauri application event loop.
///
/// The only supported launch path is `open` (`LaunchServices`), which does not start a second
/// instance while one is running; the single-instance plugin is registered first so that a launch
/// through an unsupported path (running the binary directly, say) exits before this process can
/// touch any service. Every service is started by [`Supervisor`] and stopped again when the event
/// loop is on its way out.
///
/// On macOS the activation policy is set to [`ActivationPolicy::Accessory`] so the app does not
/// show a Dock icon; it is meant to run as a menu bar resident app only. The log window is
/// declared invisible in `tauri.conf.json` and only ever shown from the tray, and closing it hides
/// it again instead of ending the app.
///
/// # Errors
///
/// Returns an error if the Tauri application fails to initialize.
#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() -> tauri::Result<()> {
    let builder = Builder::default();
    // Nothing to do in the first instance: the second instance exits on its own without having
    // touched a service, and the tray of the instance already running stays where it is.
    #[cfg(desktop)]
    let builder = builder.plugin(tauri_plugin_single_instance::init(|_app, _args, _cwd| {}));

    let app = builder
        .setup(|app| {
            #[cfg(target_os = "macos")]
            app.set_activation_policy(ActivationPolicy::Accessory);

            app.manage(Supervisor::start());
            tray::setup(app.handle())?;

            Ok(())
        })
        .on_window_event(window::hide_on_close)
        .build(tauri::generate_context!())?;

    app.run(|app_handle, event| {
        // `Exit` rather than `ExitRequested`, because it is the one point every exit path reaches:
        // the tray's quit item and `applicationWillTerminate` on a Cmd+Q or a logout both converge
        // here, right before the event loop tears down. Blocking the main thread on the stop
        // sequence is what holds the process open until every service has actually finished.
        if matches!(event, RunEvent::Exit) {
            async_runtime::block_on(app_handle.state::<Supervisor>().shutdown());
        }
    });
    Ok(())
}
