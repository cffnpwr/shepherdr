//! Tauri application shell for Shepherdr.

use tauri::{ActivationPolicy, Builder};

/// Starts the Tauri application event loop.
///
/// On macOS the activation policy is set to [`ActivationPolicy::Accessory`] so
/// the app does not show a Dock icon; it is meant to run as a menu bar
/// resident app only.
///
/// # Errors
///
/// Returns an error if the Tauri application fails to initialize or the event
/// loop exits with an error.
#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() -> tauri::Result<()> {
    Builder::default()
        .setup(|app| {
            #[cfg(target_os = "macos")]
            app.set_activation_policy(ActivationPolicy::Accessory);

            Ok(())
        })
        .run(tauri::generate_context!())
}
