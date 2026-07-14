//! Spawning service child processes.

use std::io;
use std::os::unix::process::CommandExt as _;
use std::process::{Child, Command, Stdio};

use crate::config::Service;

/// Spawns a service by executing its argv directly, without interposing a shell.
///
/// The child runs in its own process group, so the stop sequence and orphan
/// cleanup can act on the whole process tree as one unit. `env` is layered on
/// top of the app process environment, `cwd` sets the working directory when
/// present, and a relative `command[0]` is resolved against the app process's
/// `PATH`. The argv is passed through verbatim; no shell re-evaluates it.
///
/// stdin is closed; stdout and stderr are piped for the caller to capture.
///
/// # Errors
///
/// Returns an error when `command` is empty or when the child process cannot be
/// spawned.
pub fn spawn(service: &Service) -> io::Result<Child> {
    let (program, args) = service
        .command
        .split_first()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "service command is empty"))?;
    let mut command = Command::new(program);
    command
        .args(args)
        .envs(&service.env)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .process_group(0);
    if let Some(cwd) = &service.cwd {
        command.current_dir(cwd);
    }
    command.spawn()
}

#[cfg(test)]
mod tests {
    use std::env;
    use std::path::Path;

    use rustc_hash::FxHashMap;

    use super::*;

    fn service(command: &[&str]) -> Service {
        Service {
            name: "test".to_owned(),
            command: command.iter().map(|&s| s.to_owned()).collect(),
            login_shell: false,
            env: FxHashMap::default(),
            cwd: None,
            enabled: true,
        }
    }

    #[test]
    fn positive_spawn_runs_in_its_own_process_group() {
        // Given a service whose shell reports its own process group
        let svc = service(&["sh", "-c", "ps -o pgid= -p $$"]);

        // When it is spawned
        let child = spawn(&svc).expect("spawn should succeed");
        let child_pid = child.id();
        let output = child.wait_with_output().expect("child should run");

        // Then the reported process group equals the child pid: it leads a new group
        let reported = String::from_utf8_lossy(&output.stdout);
        let group: u32 = reported.trim().parse().expect("pgid should be a number");
        assert_eq!(group, child_pid);
    }

    #[test]
    fn positive_env_is_layered_and_relative_name_resolves_via_path() {
        // Given a relative command that reads an overlaid env var
        let mut svc = service(&["printenv", "SHEPHERDR_TEST_VAR"]);
        svc.env
            .insert("SHEPHERDR_TEST_VAR".to_owned(), "hello".to_owned());

        // When it is spawned
        let output = spawn(&svc)
            .expect("spawn should succeed")
            .wait_with_output()
            .expect("child should run");

        // Then the overlaid value is visible to the child
        assert_eq!(String::from_utf8_lossy(&output.stdout).trim(), "hello");
    }

    #[test]
    fn positive_cwd_sets_the_working_directory() {
        // Given a service that prints its working directory, set to a temp dir
        let dir = env::temp_dir()
            .canonicalize()
            .expect("temp dir should canonicalize");
        let mut svc = service(&["pwd"]);
        svc.cwd = Some(dir.clone());

        // When it is spawned
        let output = spawn(&svc)
            .expect("spawn should succeed")
            .wait_with_output()
            .expect("child should run");

        // Then the child's working directory is the one requested
        let printed = String::from_utf8_lossy(&output.stdout);
        assert_eq!(Path::new(printed.trim()), dir);
    }

    #[test]
    fn positive_argv_is_not_shell_evaluated() {
        // Given an argument containing shell metacharacters
        let svc = service(&["printf", "%s", "$HOME"]);

        // When it is spawned
        let output = spawn(&svc)
            .expect("spawn should succeed")
            .wait_with_output()
            .expect("child should run");

        // Then the argument reaches the program verbatim, unexpanded
        assert_eq!(String::from_utf8_lossy(&output.stdout), "$HOME");
    }

    #[test]
    fn negative_spawn_rejects_empty_command() {
        // Given a service with no command
        let svc = service(&[]);

        // When it is spawned
        let result = spawn(&svc);

        // Then it fails with an invalid-input error
        assert!(matches!(result, Err(e) if e.kind() == io::ErrorKind::InvalidInput));
    }
}
