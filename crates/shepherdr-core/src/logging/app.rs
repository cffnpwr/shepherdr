//! The app's own diagnostic log: a process-wide [`log::Log`] implementation, its formatting, and
//! [`init_app_logger`], which installs it.

use std::fmt::Display;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, PoisonError};

use chrono::{DateTime, Local, TimeZone};
use log::{Log, Metadata, Record};

use crate::logging::{LogError, RotatingWriter, error_chain, log_dir};

/// Per-generation file size cap for the app's own log, in bytes: 1 MiB.
///
/// The app's self log records only its own diagnostics (see [`init_app_logger`]), several orders
/// of magnitude lower volume than a service's raw stdout/stderr, so it does not need
/// [`super::DEFAULT_MAX_BYTES`]'s headroom. Fixed rather than read from the `[log]` config
/// section: the app's log has to be ready before [`crate::config::Config::load`] runs, so it
/// could not record that load failing otherwise.
pub const APP_LOG_MAX_BYTES: u64 = 1024 * 1024;

/// Number of generations kept for the app's own log, including the current file: 3.
///
/// See [`APP_LOG_MAX_BYTES`] for why this is fixed independently of
/// [`super::DEFAULT_MAX_GENERATIONS`] and the `[log]` config section.
pub const APP_LOG_MAX_GENERATIONS: u32 = 3;

/// Target prefix a [`log`] record must have to be kept by [`AppLogger`] or its stderr fallback.
///
/// A global logger installed via [`init_app_logger`] receives every `log` record produced
/// in-process, including tao/tauri/html5ever's own. [`log::Log`]'s documentation states that
/// `enabled` is "not necessarily called before" `log`, so the filtering has to happen inside
/// `log` itself rather than relying on `enabled` alone. Both this crate's and `shepherdr-app`'s
/// module paths start with `"shepherdr"` (`shepherdr-app`'s `[lib] name` is `shepherdr_app_lib`),
/// which is what a record's default target is set to, so the prefix check keeps exactly the two.
const APP_LOG_TARGET_PREFIX: &str = "shepherdr";

/// Formats one record as `"{date} {time} {offset} {LEVEL} {target}: {message}\n"`, e.g.
/// `"2026-08-02 14:23:01.123 +09:00 WARN shepherdr_app_lib::supervisor: service herdr exited\n"`.
///
/// The offset is included (`%:z`) so a reader can tell whether a timestamp is local time or UTC;
/// omitting it would leave that ambiguous, defeating the point of a log meant for after-the-fact
/// diagnosis.
fn format_record<Tz>(now: &DateTime<Tz>, record: &Record<'_>) -> String
where
    Tz: TimeZone,
    Tz::Offset: Display,
{
    format!(
        "{} {level} {target}: {args}\n",
        now.format("%Y-%m-%d %H:%M:%S%.3f %:z"),
        level = record.level(),
        target = record.target(),
        args = record.args(),
    )
}

/// A [`log::Log`] implementation that writes Shepherdr's own diagnostic records to a
/// [`RotatingWriter`], filtered to [`APP_LOG_TARGET_PREFIX`]. Installed by [`init_app_logger`].
struct AppLogger {
    writer: Mutex<RotatingWriter>,
}

impl AppLogger {
    /// Opens the writer at `dir/shepherdr.log`, rotating at [`APP_LOG_MAX_BYTES`] bytes across
    /// [`APP_LOG_MAX_GENERATIONS`] generations.
    fn open_in(dir: &Path) -> Result<Self, LogError> {
        let path = dir.join("shepherdr.log");
        let writer = RotatingWriter::open(path.clone(), APP_LOG_MAX_BYTES, APP_LOG_MAX_GENERATIONS)
            .map_err(|source| LogError::Open { path, source })?;
        Ok(Self {
            writer: Mutex::new(writer),
        })
    }
}

impl Log for AppLogger {
    fn enabled(&self, metadata: &Metadata<'_>) -> bool {
        metadata.target().starts_with(APP_LOG_TARGET_PREFIX)
    }

    fn log(&self, record: &Record<'_>) {
        if !record.target().starts_with(APP_LOG_TARGET_PREFIX) {
            return;
        }
        let line = format_record(&Local::now(), record);
        let mut writer = self.writer.lock().unwrap_or_else(PoisonError::into_inner);
        let _ = writer.write_all(line.as_bytes());
    }

    fn flush(&self) {}
}

/// Falls back to stderr when the app log file cannot be opened, preserving the app's previous
/// `eprintln!`-only behavior while applying the same target filter as [`AppLogger`].
struct StderrLogger;

impl Log for StderrLogger {
    fn enabled(&self, metadata: &Metadata<'_>) -> bool {
        metadata.target().starts_with(APP_LOG_TARGET_PREFIX)
    }

    fn log(&self, record: &Record<'_>) {
        if !record.target().starts_with(APP_LOG_TARGET_PREFIX) {
            return;
        }
        eprint!("{}", format_record(&Local::now(), record));
    }

    fn flush(&self) {}
}

/// Resolves the app's own log directory (`~/Library/Logs/shepherdr/app`).
///
/// Kept out of the per-service directory (`~/Library/Logs/shepherdr`, see [`log_dir`]):
/// `shepherdr_core::config::Config::validate` places no character restriction on a service
/// `name`, so a service could be named to produce a `shepherdr.log` of its own; two independent
/// [`RotatingWriter`]s rotating the same path would corrupt each other's files.
fn app_log_dir() -> Result<PathBuf, LogError> {
    Ok(log_dir()?.join("app"))
}

/// Builds the logger for the app's own diagnostics, opening `dir/shepherdr.log` with rotation.
/// Falls back to a logger that writes to stderr - reporting the failure, with its full cause
/// chain (see [`error_chain`]), through that same fallback - when the file cannot be opened.
fn build_app_logger_in(dir: &Path) -> Box<dyn Log> {
    match AppLogger::open_in(dir) {
        Ok(logger) => Box::new(logger),
        Err(err) => {
            eprintln!(
                "shepherdr: failed to open the app log file under {}, falling back to stderr: {}",
                dir.display(),
                error_chain(&err)
            );
            Box::new(StderrLogger)
        }
    }
}

/// Installs the process-wide [`log`] logger for Shepherdr's own diagnostics.
///
/// Writes to `~/Library/Logs/shepherdr/app/shepherdr.log`, rotating at [`APP_LOG_MAX_BYTES`]
/// bytes across [`APP_LOG_MAX_GENERATIONS`] generations, keeping only records whose target
/// starts with `"shepherdr"` (see [`APP_LOG_TARGET_PREFIX`]). The level filter is fixed at
/// [`log::LevelFilter::Info`]; there is no runtime override.
///
/// When the home directory cannot be resolved or the log file cannot be opened, falls back to a
/// logger that writes to stderr - preserving the app's previous `eprintln!`-only behavior - and
/// reports the failure (with its cause chain) through that fallback so it is not lost. A logger
/// is always installed, so callers using the `log` macros are never silently dropped.
///
/// Idempotent: `log` only ever accepts the first logger it is given, so a call after the first
/// (there should not be one) is a no-op rather than a panic.
pub fn init_app_logger() {
    let logger = match app_log_dir() {
        Ok(dir) => build_app_logger_in(&dir),
        Err(err) => {
            eprintln!(
                "shepherdr: failed to resolve the app log directory, falling back to stderr: {}",
                error_chain(&err)
            );
            Box::new(StderrLogger)
        }
    };
    // The level filter defaults to `Off` until set, so it has to be raised before the logger is
    // installed: otherwise a record logged in the window between the two calls would be silently
    // dropped by `log`'s own filter rather than reaching this logger at all.
    log::set_max_level(log::LevelFilter::Info);
    let _ = log::set_boxed_logger(logger);
}

#[cfg(test)]
mod tests {
    use std::{env, fs};

    use chrono::{Duration, FixedOffset};
    use log::Level;

    use super::*;

    /// A disposable directory for this test (wipes any leftovers from a previous run first).
    fn scratch_dir(label: &str) -> PathBuf {
        let dir = env::temp_dir().join(format!("shepherdr-logging-app-test-{label}"));
        let _ = fs::remove_dir_all(&dir);
        dir
    }

    #[test]
    fn positive_format_record_includes_the_timestamp_offset_level_target_and_message() {
        // Given a fixed time at a known, non-UTC offset, and a log record
        let offset = FixedOffset::east_opt(9 * 3600).expect("offset should be valid");
        let now = offset
            .with_ymd_and_hms(2026, 8, 2, 14, 23, 1)
            .single()
            .expect("date and time should be valid")
            + Duration::milliseconds(123);

        // When the record is formatted
        let line = format_record(
            &now,
            &Record::builder()
                .level(Level::Warn)
                .target("shepherdr_app_lib::supervisor")
                .args(format_args!("service {name} exited", name = "herdr"))
                .build(),
        );

        // Then it lays out the timestamp, offset, level, target, and message in one line
        assert_eq!(
            line,
            "2026-08-02 14:23:01.123 +09:00 WARN shepherdr_app_lib::supervisor: service herdr exited\n"
        );
    }

    #[test]
    fn positive_app_logger_writes_a_matching_target_to_the_log_file() {
        // Given an app logger opened over a scratch directory
        let dir = scratch_dir("app-logger-writes-matching-target");
        let logger = AppLogger::open_in(&dir).expect("open should succeed");

        // When a record whose target starts with the app's prefix is logged
        logger.log(
            &Record::builder()
                .level(Level::Info)
                .target("shepherdr_core::logging")
                .args(format_args!("app log started"))
                .build(),
        );

        // Then the formatted record lands in shepherdr.log under that directory
        let content =
            fs::read_to_string(dir.join("shepherdr.log")).expect("log should be readable");
        assert!(content.contains("INFO shepherdr_core::logging: app log started"));
    }

    #[test]
    fn negative_app_logger_ignores_a_record_outside_the_target_prefix() {
        // Given an app logger opened over a scratch directory
        let dir = scratch_dir("app-logger-ignores-foreign-target");
        let logger = AppLogger::open_in(&dir).expect("open should succeed");

        // When a record from an unrelated crate (e.g. tao) is logged
        logger.log(
            &Record::builder()
                .level(Level::Info)
                .target("tao::event_loop")
                .args(format_args!("should not appear"))
                .build(),
        );

        // Then nothing is written to the log file
        let content =
            fs::read_to_string(dir.join("shepherdr.log")).expect("log should be readable");
        assert_eq!(content, "");
    }

    #[test]
    fn positive_app_logger_enabled_returns_true_for_a_shepherdr_target() {
        // Given an app logger opened over a scratch directory
        let dir = scratch_dir("app-logger-enabled-true");
        let logger = AppLogger::open_in(&dir).expect("open should succeed");

        // When enabled is checked for a Shepherdr target
        let enabled = logger.enabled(&Metadata::builder().target("shepherdr_app_lib").build());

        // Then it is enabled
        assert!(enabled);
    }

    #[test]
    fn negative_app_logger_enabled_returns_false_for_a_foreign_target() {
        // Given an app logger opened over a scratch directory
        let dir = scratch_dir("app-logger-enabled-false");
        let logger = AppLogger::open_in(&dir).expect("open should succeed");

        // When enabled is checked for a target outside the app's prefix
        let enabled = logger.enabled(&Metadata::builder().target("html5ever::parser").build());

        // Then it is not enabled
        assert!(!enabled);
    }

    #[test]
    fn positive_build_app_logger_in_creates_and_writes_the_log_file() {
        // Given a fresh scratch directory
        let dir = scratch_dir("build-app-logger-writes");

        // When the app logger is built for that directory and a record is logged
        let logger = build_app_logger_in(&dir);
        logger.log(
            &Record::builder()
                .level(Level::Info)
                .target("shepherdr_core::logging")
                .args(format_args!("built and writing"))
                .build(),
        );

        // Then the log file exists under that directory with the record's message
        let content =
            fs::read_to_string(dir.join("shepherdr.log")).expect("log should be readable");
        assert!(content.contains("built and writing"));
    }

    #[test]
    fn negative_build_app_logger_in_falls_back_to_a_working_logger_when_the_file_cannot_be_opened()
    {
        // Given a path where the "directory" is actually a plain file, blocking file creation
        let dir = scratch_dir("build-app-logger-fallback");
        fs::create_dir_all(dir.parent().expect("scratch dir should have a parent"))
            .expect("temp root should be creatable");
        fs::write(&dir, b"not a directory").expect("blocking file should be writable");

        // When the app logger is built for that blocked path
        let logger = build_app_logger_in(&dir);

        // Then a usable logger is still returned, and logging through it does not panic
        logger.log(
            &Record::builder()
                .level(Level::Info)
                .target("shepherdr_core::logging")
                .args(format_args!("fallback should not panic"))
                .build(),
        );
    }
}
