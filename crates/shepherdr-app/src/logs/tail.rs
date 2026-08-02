//! The tailing loop for one service's log file: incremental reads, rotation detection, and line
//! buffering.
//!
//! Talks to its consumer through the [`TailSink`] trait rather than a concrete Tauri channel, so
//! the loop and the rotation detection it drives can be exercised in tests without a running
//! Tauri application.

use std::fs::Metadata;
use std::io::{self, SeekFrom};
use std::mem;
use std::os::unix::fs::MetadataExt as _;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::Serialize;
use tokio::fs::{self, File};
use tokio::io::{AsyncReadExt as _, AsyncSeekExt as _};
use tokio::time::sleep;

/// How long to wait before checking again for a log file that does not exist yet (a service that
/// has not started, or has not produced any output yet).
const RETRY_INTERVAL: Duration = Duration::from_secs(1);

/// Bytes read from the end of the file when a tail starts, whether on the first open or after a
/// rotation is detected.
///
/// Bounds the initial read regardless of how large `[log].max_size` is configured, since only a
/// recent window is ever shown and the log window caps how many lines it keeps besides.
const INITIAL_WINDOW_BYTES: u64 = 256 * 1024;

/// The device and inode of an open file, used to notice that the path now refers to a different
/// file than the one this tail has open: rotation renames the file this tail has open out of the
/// way and creates a fresh one in its place, so the path and the open file's identity diverge.
type Identity = (u64, u64);

/// One update sent to the log window over the [`TailSink`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "event", content = "data", rename_all = "snake_case")]
pub(crate) enum TailEvent {
    /// Replaces the displayed content: the initial recent window on a fresh tail, or the start
    /// of a new generation once a rotation is detected.
    Reset {
        /// The lines to display, oldest first.
        lines: Vec<String>,
    },
    /// Appends to the displayed content.
    Append {
        /// The lines to append, oldest first.
        lines: Vec<String>,
    },
    /// The tail could not continue; the task ends after delivering this.
    Error {
        /// A human-readable description of the failure.
        message: String,
    },
}

/// Where a [`run`] task delivers its [`TailEvent`]s.
///
/// Implemented for `tauri::ipc::Channel` in the parent module; tests below use a fake instead so
/// the loop can be exercised without a Tauri application.
pub(crate) trait TailSink {
    /// Delivers `event`, reporting whether the consumer is still there to receive further ones.
    /// Returning `false` ends the tail task.
    fn deliver(&self, event: TailEvent) -> bool;
}

/// Tails `path` until `sink` reports the consumer is gone.
///
/// `poll_interval` is normally resolved from the `[log]` config section's `tail_poll_interval`
/// (falling back to [`shepherdr_core::config::DEFAULT_TAIL_POLL_INTERVAL`] when unset there).
/// Waits out [`RETRY_INTERVAL`] while the file does not exist yet, and starts over from the
/// current file's start whenever a rotation is detected: rotated generations are not browsable
/// from the log window, so re-reading from the beginning of the new current file is enough.
pub(crate) async fn run(path: PathBuf, poll_interval: Duration, sink: impl TailSink) {
    loop {
        match open_current(&path).await {
            Ok(mut opened) => {
                let lines = mem::take(&mut opened.lines);
                if !sink.deliver(TailEvent::Reset { lines }) {
                    return;
                }
                if !tail_from(&path, poll_interval, opened, &sink).await {
                    return;
                }
                // Rotated: loop again to reopen the fresh current file.
            }
            Err(OpenError::NotFound) => sleep(RETRY_INTERVAL).await,
            Err(OpenError::Io(error)) => {
                sink.deliver(TailEvent::Error {
                    message: error.to_string(),
                });
                return;
            }
        }
    }
}

/// Polls `opened` every `poll_interval` until a rotation is detected or the sink reports its
/// consumer is gone.
///
/// Returns `true` when a rotation ended the loop (the caller should reopen), `false` when the
/// sink is gone or an I/O error ended the tail for good.
async fn tail_from(
    path: &Path,
    poll_interval: Duration,
    opened: OpenedTail,
    sink: &impl TailSink,
) -> bool {
    let OpenedTail {
        mut file,
        identity,
        mut position,
        mut carry,
        ..
    } = opened;
    loop {
        sleep(poll_interval).await;
        match poll_once(&mut file, path, &mut position, identity).await {
            Ok(PollOutcome::NoChange) => {}
            Ok(PollOutcome::Appended(chunk)) => {
                let lines = split_complete_lines(&mut carry, &chunk);
                if !lines.is_empty() && !sink.deliver(TailEvent::Append { lines }) {
                    return false;
                }
            }
            Ok(PollOutcome::Rotated) => return true,
            Err(error) => {
                sink.deliver(TailEvent::Error {
                    message: error.to_string(),
                });
                return false;
            }
        }
    }
}

/// A file that could not be opened for tailing.
#[derive(Debug)]
enum OpenError {
    /// The log file does not exist yet.
    NotFound,
    /// Some other I/O error.
    Io(io::Error),
}

/// The state carried from opening a tail into its polling loop.
struct OpenedTail {
    /// The open file, positioned at end-of-file.
    file: File,
    /// The device and inode identifying this particular file, to notice a rotation.
    identity: Identity,
    /// The byte offset already accounted for (the file's length at open time).
    position: u64,
    /// A trailing, not yet newline-terminated line carried over into the next poll.
    carry: Vec<u8>,
    /// The complete lines read from the initial window, ready to display.
    lines: Vec<String>,
}

/// Opens `path` and reads its initial display window.
///
/// The window starts at [`INITIAL_WINDOW_BYTES`] before the end of the file (or the start of the
/// file, if it is shorter). The first line of that window is discarded when the window does not
/// start at the beginning of the file, since it is very likely a line cut short by the window
/// boundary rather than a complete one.
async fn open_current(path: &Path) -> Result<OpenedTail, OpenError> {
    let mut file = match File::open(path).await {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Err(OpenError::NotFound),
        Err(error) => return Err(OpenError::Io(error)),
    };
    let metadata = file.metadata().await.map_err(OpenError::Io)?;
    let identity = file_identity(&metadata);
    let len = metadata.len();
    let start = tail_window_start(len, INITIAL_WINDOW_BYTES);
    file.seek(SeekFrom::Start(start))
        .await
        .map_err(OpenError::Io)?;
    let mut buffer = Vec::new();
    file.read_to_end(&mut buffer).await.map_err(OpenError::Io)?;

    let mut carry = Vec::new();
    let mut lines = split_complete_lines(&mut carry, &buffer);
    if start > 0 && !lines.is_empty() {
        let _cut_short = lines.remove(0);
    }

    Ok(OpenedTail {
        file,
        identity,
        position: len,
        carry,
        lines,
    })
}

/// What one poll of the tailed file found.
enum PollOutcome {
    /// Nothing new since the last poll.
    NoChange,
    /// New bytes appended since `position`.
    Appended(Vec<u8>),
    /// `path` now refers to a different file than the one `file` has open.
    Rotated,
}

/// Checks `path` and `file` for a rotation, then reads any bytes appended past `position`,
/// advancing it.
async fn poll_once(
    file: &mut File,
    path: &Path,
    position: &mut u64,
    identity: Identity,
) -> io::Result<PollOutcome> {
    let path_identity = match fs::metadata(path).await {
        Ok(metadata) => file_identity(&metadata),
        // The old file was already renamed away and the new one has not been created yet; the
        // next poll will pick it up once it exists.
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(PollOutcome::Rotated),
        Err(error) => return Err(error),
    };
    if path_identity != identity {
        return Ok(PollOutcome::Rotated);
    }

    let len = file.metadata().await?.len();
    if len < *position {
        // Truncated out from under us without a rotation we could detect; resync the same way.
        return Ok(PollOutcome::Rotated);
    }
    if len == *position {
        return Ok(PollOutcome::NoChange);
    }

    file.seek(SeekFrom::Start(*position)).await?;
    let delta = usize::try_from(len - *position)
        .map_err(|_conversion| io::Error::other("log grew by an unreadable amount"))?;
    let mut buffer = vec![0_u8; delta];
    file.read_exact(&mut buffer).await?;
    *position = len;
    Ok(PollOutcome::Appended(buffer))
}

/// The `(device, inode)` pair identifying a file, independent of the path used to open it.
fn file_identity(metadata: &Metadata) -> Identity {
    (metadata.dev(), metadata.ino())
}

/// The byte offset a fresh tail should start reading from: `window` bytes before the end of a
/// file of length `len`, or the start of the file if it is shorter than that.
fn tail_window_start(len: u64, window: u64) -> u64 {
    len.saturating_sub(window)
}

/// Splits `chunk` into complete (newline-terminated) lines, appending it to `carry` first so a
/// line split across two calls is reassembled. Trailing `\r` (CRLF line endings) is stripped.
/// Any bytes after the last newline are left in `carry` for the next call.
fn split_complete_lines(carry: &mut Vec<u8>, chunk: &[u8]) -> Vec<String> {
    carry.extend_from_slice(chunk);
    let mut lines = Vec::new();
    let mut start = 0;
    while let Some(offset) = carry[start..].iter().position(|&byte| byte == b'\n') {
        let end = start + offset;
        lines.push(String::from_utf8_lossy(strip_trailing_cr(&carry[start..end])).into_owned());
        start = end + 1;
    }
    let _consumed = carry.drain(0..start);
    lines
}

/// Strips one trailing `\r` from `line`, if present.
fn strip_trailing_cr(line: &[u8]) -> &[u8] {
    match line.split_last() {
        Some((b'\r', rest)) => rest,
        _no_trailing_cr => line,
    }
}

#[cfg(test)]
mod tests {
    use std::{env, fs};

    use super::*;

    /// A disposable directory for this test (wipes any leftovers from a previous run first).
    fn scratch_dir(label: &str) -> PathBuf {
        let dir = env::temp_dir().join(format!("shepherdr-tail-test-{label}"));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).expect("scratch dir should be creatable");
        dir
    }

    #[test]
    fn positive_tail_window_start_is_the_start_of_a_file_shorter_than_the_window() {
        // Given a file shorter than the window
        let (len, window) = (100, 1024);

        // When the tail window start is computed
        let start = tail_window_start(len, window);

        // Then it starts at the very beginning
        assert_eq!(start, 0);
    }

    #[test]
    fn positive_tail_window_start_is_the_window_before_the_end_of_a_longer_file() {
        // Given a file longer than the window
        let (len, window) = (10_000, 1024);

        // When the tail window start is computed
        let start = tail_window_start(len, window);

        // Then it starts exactly one window before the end
        assert_eq!(start, 10_000 - 1024);
    }

    #[test]
    fn positive_split_complete_lines_returns_only_terminated_lines_and_keeps_the_rest() {
        // Given a chunk with two complete lines and a trailing partial one
        let mut carry = Vec::new();
        let chunk = b"first\nsecond\nthi";

        // When it is split
        let lines = split_complete_lines(&mut carry, chunk);

        // Then only the complete lines come back, and the partial one is carried over
        assert_eq!(lines, vec!["first".to_owned(), "second".to_owned()]);
        assert_eq!(carry, b"thi");
    }

    #[test]
    fn positive_split_complete_lines_completes_a_line_carried_from_a_previous_call() {
        // Given a carry left over from a previous call, ending mid-line
        let mut carry = b"thi".to_vec();

        // When a chunk completing that line, plus a fresh one, arrives
        let lines = split_complete_lines(&mut carry, b"rd\nfourth\n");

        // Then the carried-over prefix is reassembled into the first line
        assert_eq!(lines, vec!["third".to_owned(), "fourth".to_owned()]);
        assert!(carry.is_empty());
    }

    #[test]
    fn positive_split_complete_lines_strips_a_trailing_carriage_return() {
        // Given a CRLF-terminated line
        let mut carry = Vec::new();

        // When it is split
        let lines = split_complete_lines(&mut carry, b"crlf\r\n");

        // Then the carriage return does not end up in the displayed line
        assert_eq!(lines, vec!["crlf".to_owned()]);
    }

    #[tokio::test]
    async fn positive_poll_once_reports_no_change_when_nothing_was_appended() {
        // Given a file already fully read up to its current length
        let dir = scratch_dir("no-change");
        let path = dir.join("svc.log");
        fs::write(&path, b"hello").expect("file should be writable");
        let mut file = File::open(&path).await.expect("file should open");
        let identity = file_identity(&file.metadata().await.expect("metadata should read"));
        let mut position = 5;

        // When it is polled again with nothing new written
        let outcome = poll_once(&mut file, &path, &mut position, identity)
            .await
            .expect("poll should succeed");

        // Then it reports no change
        assert!(matches!(outcome, PollOutcome::NoChange));
    }

    #[tokio::test]
    async fn positive_poll_once_returns_the_bytes_appended_since_the_last_position() {
        // Given a file with a known position short of its current length
        let dir = scratch_dir("appended");
        let path = dir.join("svc.log");
        fs::write(&path, b"helloworld").expect("file should be writable");
        let mut file = File::open(&path).await.expect("file should open");
        let identity = file_identity(&file.metadata().await.expect("metadata should read"));
        let mut position = 5;

        // When it is polled
        let outcome = poll_once(&mut file, &path, &mut position, identity)
            .await
            .expect("poll should succeed");

        // Then the bytes appended past the position are returned and the position advances
        assert!(matches!(outcome, PollOutcome::Appended(chunk) if chunk == b"world"));
        assert_eq!(position, 10);
    }

    #[tokio::test]
    async fn positive_poll_once_detects_a_rotation_that_renamed_the_open_file_away() {
        // Given a tail with the original file open, matching the path's current identity
        let dir = scratch_dir("rotated-rename");
        let path = dir.join("svc.log");
        fs::write(&path, b"old").expect("file should be writable");
        let mut file = File::open(&path).await.expect("file should open");
        let identity = file_identity(&file.metadata().await.expect("metadata should read"));
        let mut position = 3;

        // When the rotation renames the current file away and creates a fresh one in its place,
        // exactly as `RotatingWriter::rotate` does
        fs::rename(&path, dir.join("svc.log.1")).expect("rename should succeed");
        fs::write(&path, b"new").expect("fresh file should be writable");

        // Then the next poll detects the identity mismatch rather than reading the old file
        let outcome = poll_once(&mut file, &path, &mut position, identity)
            .await
            .expect("poll should succeed");
        assert!(matches!(outcome, PollOutcome::Rotated));
    }

    #[tokio::test]
    async fn positive_poll_once_treats_a_momentarily_missing_path_as_a_rotation() {
        // Given a tail with the original file open
        let dir = scratch_dir("rotated-missing");
        let path = dir.join("svc.log");
        fs::write(&path, b"old").expect("file should be writable");
        let mut file = File::open(&path).await.expect("file should open");
        let identity = file_identity(&file.metadata().await.expect("metadata should read"));
        let mut position = 3;

        // When the current file has been renamed away and the fresh one has not landed yet
        fs::rename(&path, dir.join("svc.log.1")).expect("rename should succeed");

        // Then the gap is treated the same as a rotation, so the caller retries rather than erring
        let outcome = poll_once(&mut file, &path, &mut position, identity)
            .await
            .expect("poll should succeed");
        assert!(matches!(outcome, PollOutcome::Rotated));
    }

    #[tokio::test]
    async fn positive_open_current_reads_the_whole_file_when_shorter_than_the_window() {
        // Given a small file, well under the initial window size
        let dir = scratch_dir("open-small");
        let path = dir.join("svc.log");
        fs::write(&path, b"first\nsecond\n").expect("file should be writable");

        // When it is opened for tailing
        let opened = open_current(&path).await.expect("open should succeed");

        // Then every line is included, none discarded as a boundary artefact
        assert_eq!(opened.lines, vec!["first".to_owned(), "second".to_owned()]);
        assert_eq!(opened.position, 13);
        assert!(opened.carry.is_empty());
    }

    #[tokio::test]
    async fn positive_open_current_drops_the_first_line_cut_by_the_window_boundary() {
        // Given a file larger than the initial window, so the window starts mid-file
        let dir = scratch_dir("open-window-cut");
        let path = dir.join("svc.log");
        let padding = "x".repeat(usize::try_from(INITIAL_WINDOW_BYTES).expect("fits usize"));
        let content = format!("{padding}cut-off\nkept\n");
        fs::write(&path, &content).expect("file should be writable");

        // When it is opened for tailing
        let opened = open_current(&path).await.expect("open should succeed");

        // Then the line straddling the window boundary is dropped, keeping only whole lines
        assert_eq!(opened.lines, vec!["kept".to_owned()]);
    }
}
