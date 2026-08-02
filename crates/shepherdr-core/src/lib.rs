//! Core of Shepherdr, a service supervisor.

pub mod config;
pub mod logging;
pub mod monitor;
mod procinfo;
pub mod reload;
pub mod spawn;
pub mod state;
pub mod stop;

/// Re-exports the `log` crate so `shepherdr-app` can use its macros
/// (`shepherdr_core::log::info!`, `warn!`, `error!`, ...) without declaring its own dependency on
/// `log`. [`logging::init_app_logger`] installs the process-wide logger those macros end up
/// calling into; `shepherdr-app`'s module paths (its `[lib] name` is `shepherdr_app_lib`) already
/// start with `"shepherdr"`, so its records pass that logger's target filter unfiltered.
pub use log;
