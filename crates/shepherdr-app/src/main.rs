//! Entry point for the Shepherdr desktop application binary.

/// Runs the Tauri application.
///
/// # Errors
///
/// Returns an error if [`shepherdr_app_lib::run`] fails.
fn main() -> tauri::Result<()> {
    shepherdr_app_lib::run()
}
