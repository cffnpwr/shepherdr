//! Capturing service child process output and rotating the log files.

use std::fs::{self, File, OpenOptions};
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, PoisonError};
use std::{env, io};

use thiserror::Error;
use tokio::io::{AsyncRead, AsyncReadExt as _};
use tokio::process::Child;
use tokio::task::{self, JoinHandle};

/// Per-generation file size cap, in bytes. Defaults to 10 MiB.
///
/// Chosen so a single generation stays a manageable size even under a crash loop that keeps
/// producing output.
pub const DEFAULT_MAX_BYTES: u64 = 10 * 1024 * 1024;

/// Number of generations kept, including the current file. Defaults to 5.
///
/// Bounds a service's log volume to roughly `DEFAULT_MAX_BYTES * DEFAULT_MAX_GENERATIONS`
/// (50 MiB) while keeping enough history to diagnose a recent crash.
pub const DEFAULT_MAX_GENERATIONS: u32 = 5;

/// Errors raised while capturing and rotating logs.
#[derive(Debug, Error)]
pub enum LogError {
    /// The home directory could not be resolved.
    #[error("failed to resolve the home directory")]
    HomeDirNotFound,
    /// The log file could not be opened.
    #[error("failed to open the log file: {path}")]
    Open {
        /// The path that was being opened.
        path: PathBuf,
        /// The underlying I/O error.
        #[source]
        source: io::Error,
    },
}

/// A handle to the log capture tasks returned by [`capture`].
pub struct CaptureHandle {
    stdout: Option<JoinHandle<io::Result<()>>>,
    stderr: Option<JoinHandle<io::Result<()>>>,
}

impl CaptureHandle {
    /// Waits for both reader tasks to finish.
    ///
    /// # Errors
    ///
    /// Returns an error if a reader task failed while writing to the log file, or if a task
    /// panicked.
    pub async fn join(self) -> io::Result<()> {
        join_reader(self.stdout).await?;
        join_reader(self.stderr).await?;
        Ok(())
    }
}

/// Waits for one reader task and flattens its result.
async fn join_reader(handle: Option<JoinHandle<io::Result<()>>>) -> io::Result<()> {
    let Some(handle) = handle else {
        return Ok(());
    };
    match handle.await {
        Ok(result) => result,
        Err(_) => Err(io::Error::other("log capture task panicked")),
    }
}

/// Captures a service's stdout and stderr, writing them to
/// `~/Library/Logs/shepherdr/<name>.log` with rotation.
///
/// Takes `child`'s stdout and stderr pipes and reads each on its own task, appending to the same
/// log file; the two streams end up interleaved in a single file. `max_bytes` and
/// `max_generations` control the rotation and are normally resolved from the `[log]` config
/// section (falling back to [`DEFAULT_MAX_BYTES`] and [`DEFAULT_MAX_GENERATIONS`] when unset
/// there).
///
/// # Errors
///
/// Returns an error when the home directory cannot be resolved, or when the log file cannot be
/// opened.
pub fn capture(
    name: &str,
    max_bytes: u64,
    max_generations: u32,
    child: &mut Child,
) -> Result<CaptureHandle, LogError> {
    capture_in(&log_dir()?, name, max_bytes, max_generations, child)
}

/// Captures with an explicit output directory and rotation limits.
fn capture_in(
    dir: &Path,
    name: &str,
    max_bytes: u64,
    max_generations: u32,
    child: &mut Child,
) -> Result<CaptureHandle, LogError> {
    let path = dir.join(format!("{name}.log"));
    let writer = RotatingWriter::open(path.clone(), max_bytes, max_generations)
        .map_err(|source| LogError::Open { path, source })?;
    let writer = Arc::new(Mutex::new(writer));
    let stdout = child
        .stdout
        .take()
        .map(|out| spawn_reader(out, Arc::clone(&writer)));
    let stderr = child.stderr.take().map(|err| spawn_reader(err, writer));
    Ok(CaptureHandle { stdout, stderr })
}

/// Resolves the default log directory (`~/Library/Logs/shepherdr`).
fn log_dir() -> Result<PathBuf, LogError> {
    Ok(env::home_dir()
        .ok_or(LogError::HomeDirNotFound)?
        .join("Library")
        .join("Logs")
        .join("shepherdr"))
}

/// Spawns the reader task for one stream.
///
/// Reads until the pipe hits EOF (the child closes that stream), handing each chunk off to
/// [`write_chunk`] for the (blocking) write to the shared writer.
fn spawn_reader<R>(mut reader: R, writer: Arc<Mutex<RotatingWriter>>) -> JoinHandle<io::Result<()>>
where
    R: AsyncRead + Unpin + Send + 'static,
{
    task::spawn(async move {
        let mut buffer = [0_u8; 8192];
        loop {
            let read = reader.read(&mut buffer).await?;
            if read == 0 {
                return Ok(());
            }
            write_chunk(Arc::clone(&writer), buffer[..read].to_vec()).await?;
        }
    })
}

/// Writes one chunk to the shared writer on the blocking thread pool.
async fn write_chunk(writer: Arc<Mutex<RotatingWriter>>, chunk: Vec<u8>) -> io::Result<()> {
    let result = task::spawn_blocking(move || {
        writer
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .write_all(&chunk)
    })
    .await;
    match result {
        Ok(result) => result,
        Err(_) => Err(io::Error::other("log write task panicked")),
    }
}

/// An append-only writer that rotates by size cap and generation count.
///
/// Rotates before a write if that write would push the file past the size cap. The very first
/// write to a fresh file is never rotated away, even if that single write alone exceeds the cap
/// (this avoids pointless back-to-back rotations).
struct RotatingWriter {
    path: PathBuf,
    file: File,
    size: u64,
    max_bytes: u64,
    max_generations: u32,
}

impl RotatingWriter {
    /// Opens `path` in append mode, picking up the existing size if the file already has
    /// content. Creates the parent directory if it does not exist.
    fn open(path: PathBuf, max_bytes: u64, max_generations: u32) -> io::Result<Self> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let file = OpenOptions::new().create(true).append(true).open(&path)?;
        let size = file.metadata()?.len();
        Ok(Self {
            path,
            file,
            size,
            max_bytes,
            max_generations,
        })
    }

    /// Writes `buf`, rotating beforehand if the write would push the file past the size cap.
    fn write_all(&mut self, buf: &[u8]) -> io::Result<()> {
        let incoming = buf.len() as u64;
        if self.size > 0 && self.size.saturating_add(incoming) > self.max_bytes {
            self.rotate()?;
        }
        self.file.write_all(buf)?;
        self.size += incoming;
        Ok(())
    }

    /// Shifts generations down and starts a fresh current file.
    ///
    /// Shifts `<name>.log.1` through `<name>.log.<max_generations - 1>` down by one, dropping
    /// the oldest generation that would overflow. When `max_generations` is 1 or less, no prior
    /// generation is kept and the current file is simply truncated.
    fn rotate(&mut self) -> io::Result<()> {
        if self.max_generations > 1 {
            let oldest = self.generation_path(self.max_generations - 1);
            if oldest.exists() {
                fs::remove_file(&oldest)?;
            }
            for generation in (1..self.max_generations - 1).rev() {
                let from = self.generation_path(generation);
                if from.exists() {
                    fs::rename(&from, self.generation_path(generation + 1))?;
                }
            }
            fs::rename(&self.path, self.generation_path(1))?;
        }
        self.file = File::create(&self.path)?;
        self.size = 0;
        Ok(())
    }

    /// Builds the path for `<name>.log.<generation>`.
    fn generation_path(&self, generation: u32) -> PathBuf {
        let mut name = self.path.clone().into_os_string();
        name.push(format!(".{generation}"));
        PathBuf::from(name)
    }
}

#[cfg(test)]
mod tests {
    use std::process::Stdio;

    use tokio::process::Command;

    use super::*;

    /// A disposable directory for this test (wipes any leftovers from a previous run first).
    fn scratch_dir(label: &str) -> PathBuf {
        let dir = env::temp_dir().join(format!("shepherdr-logging-test-{label}"));
        let _ = fs::remove_dir_all(&dir);
        dir
    }

    #[test]
    fn positive_open_creates_the_missing_parent_directory() {
        // Given a path under a directory that does not exist yet
        let dir = scratch_dir("open-creates-parent");
        let path = dir.join("svc.log");

        // When the writer is opened
        let writer = RotatingWriter::open(path.clone(), 1024, 5).expect("open should succeed");

        // Then the file exists and starts empty
        assert_eq!(writer.size, 0);
        assert!(path.exists());
    }

    #[test]
    fn positive_write_all_appends_without_rotating_when_under_the_limit() {
        // Given a writer with headroom under the size limit
        let dir = scratch_dir("append-under-limit");
        let path = dir.join("svc.log");
        let mut writer = RotatingWriter::open(path.clone(), 100, 5).expect("open should succeed");

        // When two writes stay under the limit
        writer.write_all(b"hello").expect("write should succeed");
        writer.write_all(b"world").expect("write should succeed");

        // Then both are appended to the same file and no rotation happened
        assert_eq!(
            fs::read_to_string(&path).expect("log should be readable"),
            "helloworld"
        );
        assert!(!writer.generation_path(1).exists());
    }

    #[test]
    fn positive_write_all_rotates_when_the_write_would_exceed_the_limit() {
        // Given a writer whose limit is smaller than a second write would need
        let dir = scratch_dir("rotate-on-overflow");
        let path = dir.join("svc.log");
        let mut writer = RotatingWriter::open(path.clone(), 5, 5).expect("open should succeed");
        writer.write_all(b"abcde").expect("write should succeed");

        // When a write pushes the total past the limit
        writer.write_all(b"fg").expect("write should succeed");

        // Then the prior content moved to generation 1 and the new write started fresh
        assert_eq!(
            fs::read_to_string(&path).expect("log should be readable"),
            "fg"
        );
        assert_eq!(
            fs::read_to_string(writer.generation_path(1)).expect("generation 1 should be readable"),
            "abcde"
        );
    }

    #[test]
    fn positive_rotation_shifts_generations_and_drops_the_oldest() {
        // Given a writer that rotates on every write and keeps 3 generations
        let dir = scratch_dir("rotate-shifts-generations");
        let path = dir.join("svc.log");
        let mut writer = RotatingWriter::open(path.clone(), 1, 3).expect("open should succeed");

        // When four writes each force a rotation of the previous one
        writer.write_all(b"a").expect("write should succeed");
        writer.write_all(b"b").expect("write should succeed");
        writer.write_all(b"c").expect("write should succeed");
        writer.write_all(b"d").expect("write should succeed");

        // Then the current file and the two kept generations hold the three latest writes
        assert_eq!(
            fs::read_to_string(&path).expect("current should be readable"),
            "d"
        );
        assert_eq!(
            fs::read_to_string(writer.generation_path(1)).expect("generation 1 should be readable"),
            "c"
        );
        assert_eq!(
            fs::read_to_string(writer.generation_path(2)).expect("generation 2 should be readable"),
            "b"
        );
        // And the oldest write, which would have been generation 3, was pruned
        assert!(!writer.generation_path(3).exists());
    }

    #[test]
    fn positive_rotation_with_two_generations_keeps_only_the_current_and_generation_one() {
        // Given a writer that rotates on every write and keeps only 2 generations
        let dir = scratch_dir("rotate-two-generations");
        let path = dir.join("svc.log");
        let mut writer = RotatingWriter::open(path.clone(), 1, 2).expect("open should succeed");

        // When three writes each force a rotation
        writer.write_all(b"a").expect("write should succeed");
        writer.write_all(b"b").expect("write should succeed");
        writer.write_all(b"c").expect("write should succeed");

        // Then only the current file and generation 1 remain
        assert_eq!(
            fs::read_to_string(&path).expect("current should be readable"),
            "c"
        );
        assert_eq!(
            fs::read_to_string(writer.generation_path(1)).expect("generation 1 should be readable"),
            "b"
        );
        assert!(!writer.generation_path(2).exists());
    }

    #[test]
    fn positive_rotation_discards_history_when_max_generations_is_one() {
        // Given a writer configured to keep no rotated generations at all
        let dir = scratch_dir("rotate-one-generation");
        let path = dir.join("svc.log");
        let mut writer = RotatingWriter::open(path.clone(), 1, 1).expect("open should succeed");

        // When a second write forces a rotation
        writer.write_all(b"a").expect("write should succeed");
        writer.write_all(b"b").expect("write should succeed");

        // Then the file was simply truncated to the new content, with no generation 1 file
        assert_eq!(
            fs::read_to_string(&path).expect("current should be readable"),
            "b"
        );
        assert!(!writer.generation_path(1).exists());
    }

    fn piped_child(script: &str) -> Child {
        Command::new("sh")
            .arg("-c")
            .arg(script)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("child should spawn")
    }

    #[tokio::test]
    async fn positive_capture_writes_stdout_to_the_log_file() {
        // Given a child that writes only to stdout
        let dir = scratch_dir("capture-stdout");
        let mut child = piped_child("printf hello");

        // When its output is captured
        let handle = capture_in(
            &dir,
            "svc",
            DEFAULT_MAX_BYTES,
            DEFAULT_MAX_GENERATIONS,
            &mut child,
        )
        .expect("capture should succeed");
        child.wait().await.expect("child should run");
        handle
            .join()
            .await
            .expect("reader tasks should finish cleanly");

        // Then the stdout content lands in the log file
        assert_eq!(
            fs::read_to_string(dir.join("svc.log")).expect("log should be readable"),
            "hello"
        );
    }

    #[tokio::test]
    async fn positive_capture_writes_stderr_to_the_log_file() {
        // Given a child that writes only to stderr
        let dir = scratch_dir("capture-stderr");
        let mut child = piped_child("printf oops 1>&2");

        // When its output is captured
        let handle = capture_in(
            &dir,
            "svc",
            DEFAULT_MAX_BYTES,
            DEFAULT_MAX_GENERATIONS,
            &mut child,
        )
        .expect("capture should succeed");
        child.wait().await.expect("child should run");
        handle
            .join()
            .await
            .expect("reader tasks should finish cleanly");

        // Then the stderr content lands in the same log file
        assert_eq!(
            fs::read_to_string(dir.join("svc.log")).expect("log should be readable"),
            "oops"
        );
    }

    #[tokio::test]
    async fn positive_capture_handles_a_child_without_a_stderr_pipe() {
        // Given a child spawned with stdout piped but stderr left unpiped
        let dir = scratch_dir("capture-no-stderr-pipe");
        let mut child = Command::new("sh")
            .arg("-c")
            .arg("printf hello")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .expect("child should spawn");

        // When its output is captured
        let handle = capture_in(
            &dir,
            "svc",
            DEFAULT_MAX_BYTES,
            DEFAULT_MAX_GENERATIONS,
            &mut child,
        )
        .expect("capture should succeed");
        child.wait().await.expect("child should run");

        // Then it does not block or fail on the missing stderr stream
        handle
            .join()
            .await
            .expect("reader tasks should finish cleanly");
        assert_eq!(
            fs::read_to_string(dir.join("svc.log")).expect("log should be readable"),
            "hello"
        );
    }

    #[tokio::test]
    async fn positive_capture_creates_the_missing_log_directory() {
        // Given a log directory that does not exist yet and a child that produces no output
        let dir = scratch_dir("capture-creates-directory");
        let mut child = piped_child(":");

        // When its output is captured
        let handle = capture_in(
            &dir,
            "svc",
            DEFAULT_MAX_BYTES,
            DEFAULT_MAX_GENERATIONS,
            &mut child,
        )
        .expect("capture should succeed");
        child.wait().await.expect("child should run");
        handle
            .join()
            .await
            .expect("reader tasks should finish cleanly");

        // Then the log file exists, empty
        assert_eq!(
            fs::read_to_string(dir.join("svc.log")).expect("log should be readable"),
            ""
        );
    }

    #[tokio::test]
    async fn negative_capture_fails_when_the_log_directory_cannot_be_created() {
        // Given a path where the "directory" is actually a plain file, blocking create_dir_all
        let dir = scratch_dir("capture-blocked-directory");
        fs::create_dir_all(dir.parent().expect("scratch dir should have a parent"))
            .expect("temp root should be creatable");
        fs::write(&dir, b"not a directory").expect("blocking file should be writable");
        let mut child = piped_child(":");

        // When capture is attempted under that blocked path
        let result = capture_in(
            &dir,
            "svc",
            DEFAULT_MAX_BYTES,
            DEFAULT_MAX_GENERATIONS,
            &mut child,
        );
        child.wait().await.expect("child should run");

        // Then it fails while trying to open the log file
        assert!(matches!(result, Err(LogError::Open { .. })));
    }
}
