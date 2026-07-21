//! Spawning service child processes.

use std::ffi::OsString;
use std::os::unix::process::CommandExt as _;
use std::process::{Child, Command, Stdio};
use std::{env, io};

use nix::unistd::{Uid, User};

use crate::config::Service;

/// Spawns a service child process.
///
/// When `login_shell` is `false`, the service argv is executed directly without
/// interposing a shell. When `true`, it is launched through the user's login
/// shell so it inherits the login-initialized environment (a Homebrew `PATH`,
/// for example); the argv is still passed through verbatim and never
/// re-evaluated by the shell.
///
/// In both modes the child runs in its own process group, so the stop sequence
/// and orphan cleanup can act on the whole process tree as one unit. `env` is
/// applied on top of the base environment, `cwd` sets the working directory, and
/// a relative `command[0]` is resolved against the effective `PATH`.
///
/// stdin is closed; stdout and stderr are piped for the caller to capture.
///
/// A missing working directory or a missing/non-executable `command[0]` is not
/// reported here: the child exits early with a diagnostic on its captured
/// stderr, which the monitor and log layers handle.
///
/// # Errors
///
/// Returns an error when `command` is empty, when the login shell path cannot be
/// spawned, when the app working directory cannot be resolved for a login-shell
/// service without a `cwd`, or when the child process cannot be spawned.
pub fn spawn(service: &Service) -> io::Result<Child> {
    if service.login_shell {
        spawn_login_shell(service)
    } else {
        spawn_direct(service)
    }
}

/// Executes the service argv directly, layering `env` on the app environment.
fn spawn_direct(service: &Service) -> io::Result<Child> {
    let (program, args) = split_command(service)?;
    let mut command = Command::new(program);
    command.args(args).envs(&service.env);
    if let Some(cwd) = &service.cwd {
        command.current_dir(cwd);
    }
    configure_child_io(&mut command);
    command.spawn()
}

/// Executes the service argv through a login shell.
///
/// Runs `<shell> -l -c 'cd "$1" && shift && exec "$@"' shepherdr <cwd> env
/// <KEY=VALUE...> <argv...>`. Passing the argv as positional parameters keeps it
/// verbatim; the shell parses only the fixed script. `env` is the exec target so
/// its assignments are applied after login initialization, taking precedence
/// over it, and `command[0]` is resolved by `env` against the login `PATH`. The
/// leading `cd` fixes the working directory as the final state regardless of any
/// `cd` performed by the initialization scripts.
fn spawn_login_shell(service: &Service) -> io::Result<Child> {
    let (program, args) = split_command(service)?;
    let cwd = match &service.cwd {
        Some(dir) => dir.clone(),
        None => env::current_dir()?,
    };
    let mut command = Command::new(login_shell());
    command
        .arg("-l")
        .arg("-c")
        .arg(r#"cd "$1" && shift && exec "$@""#)
        .arg("shepherdr")
        .arg(cwd)
        .arg("env");
    for (key, value) in &service.env {
        command.arg(format!("{key}={value}"));
    }
    command.arg(program).args(args);
    configure_child_io(&mut command);
    command.spawn()
}

/// Splits the service `command` into its program and arguments.
fn split_command(service: &Service) -> io::Result<(&String, &[String])> {
    service
        .command
        .split_first()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "service command is empty"))
}

/// Applies the shared stdio and process-group configuration.
fn configure_child_io(command: &mut Command) {
    command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .process_group(0);
}

/// Resolves the user's login shell: `$SHELL`, else the password database, else
/// `/bin/sh`.
///
/// `$SHELL` may be absent under a GUI (`LaunchServices`) launch, so the password
/// database is consulted as the authoritative source before the final fallback.
fn login_shell() -> OsString {
    resolve_shell(env::var_os("SHELL"))
}

/// Applies the shell-resolution precedence to a given `$SHELL` value.
fn resolve_shell(shell_env: Option<OsString>) -> OsString {
    match shell_env {
        Some(shell) if !shell.is_empty() => shell,
        _ => passwd_shell().unwrap_or_else(|| OsString::from("/bin/sh")),
    }
}

/// Reads the login shell from the password database for the current user.
fn passwd_shell() -> Option<OsString> {
    let user = User::from_uid(Uid::current()).ok().flatten()?;
    if user.shell.as_os_str().is_empty() {
        return None;
    }
    Some(user.shell.into_os_string())
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

    #[test]
    fn positive_resolve_shell_prefers_the_shell_env() {
        // Given a non-empty SHELL value
        let resolved = resolve_shell(Some(OsString::from("/custom/shell")));

        // Then it is used verbatim
        assert_eq!(resolved, OsString::from("/custom/shell"));
    }

    #[test]
    fn positive_resolve_shell_falls_back_when_unset() {
        // Given no SHELL value (as under a GUI launch)
        let resolved = resolve_shell(None);

        // Then it falls back to a non-empty shell from the password database or /bin/sh
        assert!(!resolved.is_empty());
    }

    #[test]
    fn positive_resolve_shell_falls_back_when_empty() {
        // Given an empty SHELL value
        let resolved = resolve_shell(Some(OsString::new()));

        // Then the empty value is rejected in favor of the fallback
        assert!(!resolved.is_empty());
    }

    #[test]
    fn positive_login_shell_runs_in_its_own_process_group() {
        // Given a login-shell service whose shell reports its own process group
        let mut svc = service(&["sh", "-c", "ps -o pgid= -p $$"]);
        svc.login_shell = true;

        // When it is spawned
        let child = spawn(&svc).expect("spawn should succeed");
        let child_pid = child.id();
        let output = child.wait_with_output().expect("child should run");

        // Then the process group survives the exec chain and leads a new group
        let reported = String::from_utf8_lossy(&output.stdout);
        let group: u32 = reported.trim().parse().expect("pgid should be a number");
        assert_eq!(group, child_pid);
    }

    #[test]
    fn positive_login_shell_argv_is_not_shell_evaluated() {
        // Given a relative command with a shell metacharacter argument
        let mut svc = service(&["printf", "%s", "$HOME"]);
        svc.login_shell = true;

        // When it is spawned through the login shell
        let output = spawn(&svc)
            .expect("spawn should succeed")
            .wait_with_output()
            .expect("child should run");

        // Then the argument is verbatim and the relative name resolved via the login PATH
        assert_eq!(String::from_utf8_lossy(&output.stdout), "$HOME");
    }

    #[test]
    fn positive_login_shell_env_overrides_inherited_value() {
        // Given a login-shell service that overrides HOME, which the login shell exports
        let mut svc = service(&["printenv", "HOME"]);
        svc.login_shell = true;
        svc.env
            .insert("HOME".to_owned(), "/shepherdr/override".to_owned());

        // When it is spawned
        let output = spawn(&svc)
            .expect("spawn should succeed")
            .wait_with_output()
            .expect("child should run");

        // Then env takes precedence over the login-initialized value
        assert_eq!(
            String::from_utf8_lossy(&output.stdout).trim(),
            "/shepherdr/override"
        );
    }

    #[test]
    fn positive_login_shell_cwd_sets_the_working_directory() {
        // Given a login-shell service with a working directory set to a temp dir
        let dir = env::temp_dir()
            .canonicalize()
            .expect("temp dir should canonicalize");
        let mut svc = service(&["pwd"]);
        svc.login_shell = true;
        svc.cwd = Some(dir.clone());

        // When it is spawned
        let output = spawn(&svc)
            .expect("spawn should succeed")
            .wait_with_output()
            .expect("child should run");

        // Then the working directory is the requested one, not the login default
        let printed = String::from_utf8_lossy(&output.stdout);
        assert_eq!(Path::new(printed.trim()), dir);
    }
}
