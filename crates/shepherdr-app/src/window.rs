//! The log window, which the app keeps alive in the background and raises on request.

use tauri::{AppHandle, Manager as _, Window, WindowEvent};

/// The label of the window declared in `tauri.conf.json`.
pub const MAIN: &str = "main";

/// Shows the log window and brings it to the front.
///
/// The window is created hidden at startup and only ever hidden again afterwards, so this is a
/// show rather than a create: the webview it holds survives being dismissed.
pub fn open_logs(app: &AppHandle) {
    let Some(window) = app.get_webview_window(MAIN) else {
        eprintln!("shepherdr: the log window is not available");
        return;
    };
    if let Err(error) = window.show() {
        eprintln!("shepherdr: failed to show the log window: {error}");
        return;
    }
    if let Err(error) = window.set_focus() {
        eprintln!("shepherdr: failed to bring the log window to the front: {error}");
    }
}

/// Turns a close of the log window into a hide.
///
/// Shepherdr lives in the menu bar, so dismissing its only window must not take the app and every
/// service down with it. Closing is intercepted here instead of at the app's exit request, which
/// leaves the exit paths that really are meant to quit -- the tray's quit item, Cmd+Q, logging out
/// -- reaching the shutdown of every service untouched.
pub fn hide_on_close(window: &Window, event: &WindowEvent) {
    let WindowEvent::CloseRequested { api, .. } = event else {
        return;
    };
    if window.label() != MAIN {
        return;
    }
    api.prevent_close();
    if let Err(error) = window.hide() {
        eprintln!("shepherdr: failed to hide the log window: {error}");
    }
}
