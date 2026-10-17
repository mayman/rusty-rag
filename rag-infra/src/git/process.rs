//! Running `git` as a hardened subprocess with a deadline.
//!
//! Every invocation gets the same environment and `-c` options (no prompts, no
//! pagers, restricted transports, no hooks), runs in a scratch directory, gets a null
//! stdin unless input is supplied, and is killed *with its whole process tree* after
//! a timeout (or when it prints more than its output cap). Output is captured as
//! bytes.

use std::ffi::{OsStr, OsString};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc};
use std::thread;
use std::time::{Duration, Instant};

/// How long to wait for the output pipes to close after the process has exited or
/// was killed. A grandchild that inherited a pipe (a transport helper) can hold it
/// open past the death of `git` itself.
const PIPE_GRACE: Duration = Duration::from_secs(2);

/// How much of stderr is kept; the rest is read and thrown away.
const MAX_STDERR_BYTES: usize = 1024 * 1024;

/// Everything captured from one finished process.
#[derive(Debug)]
pub struct Output {
    pub status: ExitStatus,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
}

/// Why a subprocess did not finish.
#[derive(Debug, PartialEq, Eq)]
pub enum RunError {
    /// The process exceeded its deadline and was killed.
    TimedOut,
    /// The process printed more than the allowed output and was killed.
    OutputTooLarge,
    /// The process could not be started (no `git` on PATH) or its pipes failed.
    Io(String),
}

/// Runs `command` to completion, killing it (and everything it started) after
/// `timeout`. `input`, when given, is written to stdin (which is then closed);
/// otherwise stdin is null.
pub fn run(
    command: Command,
    input: Option<Vec<u8>>,
    timeout: Duration,
) -> Result<Output, RunError> {
    run_capped(command, input, timeout, usize::MAX)
}

/// Like `run`, and kills the process when its stdout grows beyond `max_stdout`
/// bytes, so a request for too much data never fills memory.
pub fn run_capped(
    mut command: Command,
    input: Option<Vec<u8>>,
    timeout: Duration,
    max_stdout: usize,
) -> Result<Output, RunError> {
    command
        .stdin(if input.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    // Its own process group, so the whole group can be signalled on timeout.
    #[cfg(unix)]
    std::os::unix::process::CommandExt::process_group(&mut command, 0);
    let mut child = command.spawn().map_err(|error| {
        RunError::Io(format!("cannot start {:?}: {error}", command.get_program()))
    })?;

    let writer = child.stdin.take().zip(input).map(|(mut stdin, bytes)| {
        // A child that exits early closes the pipe; that error is not ours to report.
        thread::spawn(move || {
            let _ = stdin.write_all(&bytes);
        })
    });
    let overflow = Arc::new(AtomicBool::new(false));
    let stdout = collect(child.stdout.take(), max_stdout, Some(Arc::clone(&overflow)));
    let stderr = collect(child.stderr.take(), MAX_STDERR_BYTES, None);

    let deadline = Instant::now() + timeout;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if overflow.load(Ordering::SeqCst) => {
                kill_tree(&mut child);
                return Err(RunError::OutputTooLarge);
            }
            Ok(None) if Instant::now() >= deadline => {
                kill_tree(&mut child);
                // The pipe readers are detached on purpose: see `PIPE_GRACE`.
                return Err(RunError::TimedOut);
            }
            Ok(None) => thread::sleep(Duration::from_millis(5)),
            Err(error) => return Err(RunError::Io(error.to_string())),
        }
    };
    if let Some(writer) = writer {
        let _ = writer.join();
    }
    if overflow.load(Ordering::SeqCst) {
        return Err(RunError::OutputTooLarge);
    }
    Ok(Output {
        status,
        stdout: stdout.recv_timeout(PIPE_GRACE).unwrap_or_default(),
        stderr: stderr.recv_timeout(PIPE_GRACE).unwrap_or_default(),
    })
}

/// Kills `child` and every process it started, then reaps it. `Child::kill` alone
/// would leave a transport helper, an ssh, or a shell alias running (and holding
/// the clone's files open). Safe Rust cannot make a Windows job object or a process
/// group signal without `unsafe` or a new dependency, so the operating system's own
/// tools do it: `taskkill /T` on Windows, `kill` on the process group elsewhere.
fn kill_tree(child: &mut Child) {
    let pid = child.id().to_string();
    #[cfg(windows)]
    let mut killer = {
        let mut killer = Command::new("taskkill");
        killer.args(["/PID", &pid, "/T", "/F"]);
        killer
    };
    #[cfg(unix)]
    let mut killer = {
        let mut killer = Command::new("kill");
        killer.args(["-KILL", "--", &format!("-{pid}")]);
        killer
    };
    #[cfg(any(windows, unix))]
    {
        killer
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        let _ = killer.status();
    }
    let _ = child.kill();
    let _ = child.wait();
}

/// Reads a pipe on its own thread. Keeps up to `cap` bytes; beyond that, a stdout
/// reader (with an `overflow` flag) stops and raises the flag, a stderr reader reads
/// on and discards.
fn collect<R: Read + Send + 'static>(
    pipe: Option<R>,
    cap: usize,
    overflow: Option<Arc<AtomicBool>>,
) -> mpsc::Receiver<Vec<u8>> {
    let (sender, receiver) = mpsc::channel();
    thread::spawn(move || {
        let mut bytes = Vec::new();
        if let Some(mut pipe) = pipe {
            let mut chunk = [0u8; 16 * 1024];
            loop {
                match pipe.read(&mut chunk) {
                    Ok(0) | Err(_) => break,
                    Ok(read) => {
                        if bytes.len() + read <= cap {
                            bytes.extend_from_slice(&chunk[..read]);
                        } else if let Some(flag) = &overflow {
                            flag.store(true, Ordering::SeqCst);
                            break;
                        }
                    }
                }
            }
        }
        let _ = sender.send(bytes);
    });
    receiver
}

/// `-c` options on every invocation: restricted transports (everything denied, then
/// an allow list; `ext`, `git`, and `http` named explicitly as well), no interaction,
/// no background work, byte-exact content.
const HARDENING_CONFIG: &[&str] = &[
    "protocol.allow=never",
    "protocol.https.allow=always",
    "protocol.ssh.allow=always",
    "protocol.file.allow=always",
    "protocol.ext.allow=never",
    "protocol.git.allow=never",
    "protocol.http.allow=never",
    "core.longpaths=true",
    "core.autocrlf=false",
    "core.fsmonitor=false",
    "core.askPass=",
    "gc.auto=0",
    "maintenance.auto=false",
    "fetch.recurseSubmodules=false",
    "submodule.recurse=false",
    "advice.detachedHead=false",
];

/// Environment variables that would redirect `git` to another repository or
/// another set of objects, make it run helpers, ask for input, loosen the transport
/// rules, or trace (and so print) what it does.
const SCRUBBED_ENV: &[&str] = &[
    "GIT_DIR",
    "GIT_WORK_TREE",
    "GIT_INDEX_FILE",
    "GIT_OBJECT_DIRECTORY",
    "GIT_ALTERNATE_OBJECT_DIRECTORIES",
    "GIT_NAMESPACE",
    "GIT_COMMON_DIR",
    "GIT_SSH",
    "GIT_EXTERNAL_DIFF",
    "GIT_PAGER",
    "GIT_ASKPASS",
    "SSH_ASKPASS",
    "GIT_ALLOW_PROTOCOL",
    "GIT_CURL_VERBOSE",
    "GIT_TRACE",
    "GIT_CONFIG_COUNT",
    "GIT_CONFIG_PARAMETERS",
];

/// Variable families whose members are found by name in the real environment:
/// `GIT_TRACE*` and the numbered `GIT_CONFIG_KEY_<n>` / `GIT_CONFIG_VALUE_<n>`.
const SCRUBBED_ENV_PREFIXES: &[&str] = &["GIT_TRACE", "GIT_CONFIG_KEY_", "GIT_CONFIG_VALUE_"];

/// The ssh command: the user's `GIT_SSH_COMMAND` if any, never asking for input.
fn ssh_command(user_value: Option<OsString>) -> OsString {
    let mut command = user_value
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| OsString::from("ssh"));
    command.push(" -o BatchMode=yes");
    command
}

/// The empty directory `core.hooksPath` points at, under the scratch directory.
/// Callers create it (`git_command` does no I/O).
pub fn hooks_dir(scratch: &Path) -> PathBuf {
    scratch.join("no-hooks")
}

/// A hardened `git` command that runs in `scratch` (so `git` never discovers some
/// repository above the process's working directory) with hooks disabled.
/// `git_dir`, when given, is passed as `--git-dir`, so that `git` never discovers
/// another repository above a missing directory either.
pub fn git_command(scratch: &Path, git_dir: Option<&Path>) -> Command {
    git_command_in(scratch, git_dir, std::env::vars_os().map(|(name, _)| name))
}

/// `git_command` with the names of the inherited environment variables given
/// explicitly, so the scrubbing of name families can be tested.
fn git_command_in(
    scratch: &Path,
    git_dir: Option<&Path>,
    inherited_names: impl IntoIterator<Item = OsString>,
) -> Command {
    let mut command = Command::new("git");
    command.current_dir(scratch);
    for name in SCRUBBED_ENV {
        command.env_remove(name);
    }
    for name in inherited_names {
        let in_scrubbed_family = name.to_str().is_some_and(|name| {
            SCRUBBED_ENV_PREFIXES
                .iter()
                .any(|prefix| name.starts_with(prefix))
        });
        if in_scrubbed_family {
            command.env_remove(name);
        }
    }
    command
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GCM_INTERACTIVE", "never")
        .env("LC_ALL", "C")
        .env("LANG", "C")
        .env("GIT_LFS_SKIP_SMUDGE", "1")
        .env("GIT_OPTIONAL_LOCKS", "0")
        .env(
            "GIT_SSH_COMMAND",
            ssh_command(std::env::var_os("GIT_SSH_COMMAND")),
        );
    for option in HARDENING_CONFIG {
        command.arg("-c").arg(option);
    }
    let mut hooks = OsString::from("core.hooksPath=");
    hooks.push(hooks_dir(scratch));
    command.arg("-c").arg(hooks);
    if let Some(dir) = git_dir {
        command.arg("--git-dir").arg(dir);
    }
    command
}

/// Appends `args` to a command.
pub fn with_args<I, S>(mut command: Command, args: I) -> Command
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    command.args(args);
    command
}
#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::time::{Duration, Instant};

    use super::*;

    fn scratch() -> tempfile::TempDir {
        tempfile::tempdir().expect("scratch dir")
    }

    fn env_of(command: &Command) -> BTreeMap<String, Option<String>> {
        command
            .get_envs()
            .map(|(k, v)| {
                (
                    k.to_string_lossy().into_owned(),
                    v.map(|v| v.to_string_lossy().into_owned()),
                )
            })
            .collect()
    }

    fn args_of(command: &Command) -> Vec<String> {
        command
            .get_args()
            .map(|a| a.to_string_lossy().into_owned())
            .collect()
    }

    fn config_of(command: &Command) -> Vec<String> {
        args_of(command)
            .chunks(2)
            .filter(|pair| pair[0] == "-c")
            .map(|pair| pair[1].clone())
            .collect()
    }

    #[test]
    fn commands_never_prompt_and_use_the_c_locale() {
        let dir = scratch();
        let env = env_of(&git_command(dir.path(), None));
        for (name, value) in [
            ("GIT_TERMINAL_PROMPT", "0"),
            ("GCM_INTERACTIVE", "never"),
            ("LC_ALL", "C"),
            ("GIT_LFS_SKIP_SMUDGE", "1"),
        ] {
            assert_eq!(env.get(name), Some(&Some(value.to_owned())), "{name}");
        }
        let ssh = env["GIT_SSH_COMMAND"].clone().unwrap();
        assert!(ssh.contains("BatchMode=yes"), "{ssh}");
    }

    #[test]
    fn inherited_repository_redirection_variables_are_removed() {
        let dir = scratch();
        let env = env_of(&git_command(dir.path(), None));
        for name in ["GIT_DIR", "GIT_WORK_TREE", "GIT_INDEX_FILE", "GIT_SSH"] {
            assert_eq!(env.get(name), Some(&None), "{name} must be unset");
        }
    }

    #[test]
    fn askpass_tracing_and_protocol_overrides_are_removed() {
        let dir = scratch();
        let env = env_of(&git_command(dir.path(), None));
        for name in [
            "GIT_ASKPASS",
            "SSH_ASKPASS",
            "GIT_ALLOW_PROTOCOL",
            "GIT_CURL_VERBOSE",
            "GIT_CONFIG_COUNT",
            "GIT_CONFIG_PARAMETERS",
            "GIT_TRACE",
        ] {
            assert_eq!(env.get(name), Some(&None), "{name} must be unset");
        }
    }

    #[test]
    fn numbered_and_prefixed_variables_found_in_the_environment_are_removed() {
        let dir = scratch();
        let present = [
            "PATH",
            "GIT_TRACE2_EVENT",
            "GIT_TRACE_PACKET",
            "GIT_CONFIG_KEY_0",
            "GIT_CONFIG_VALUE_12",
            "GIT_CONFIG_GLOBAL",
            "GIT_CONFIGURATION",
            "HOME",
        ]
        .map(OsString::from);
        let env = env_of(&git_command_in(dir.path(), None, present));
        for name in [
            "GIT_TRACE2_EVENT",
            "GIT_TRACE_PACKET",
            "GIT_CONFIG_KEY_0",
            "GIT_CONFIG_VALUE_12",
        ] {
            assert_eq!(env.get(name), Some(&None), "{name} must be unset");
        }
        for name in ["PATH", "HOME", "GIT_CONFIG_GLOBAL", "GIT_CONFIGURATION"] {
            assert!(!env.contains_key(name), "{name} is left alone");
        }
    }

    #[test]
    fn transports_are_denied_by_default_with_an_explicit_allow_list() {
        let dir = scratch();
        let config = config_of(&git_command(dir.path(), None));
        assert_eq!(
            config.first().map(String::as_str),
            Some("protocol.allow=never")
        );
        for allowed in ["https", "ssh", "file"] {
            assert!(config.contains(&format!("protocol.{allowed}.allow=always")));
        }
        for denied in ["ext", "git", "http"] {
            assert!(
                config.contains(&format!("protocol.{denied}.allow=never")),
                "protocol.{denied}.allow=never"
            );
        }
        for needed in ["core.longpaths=true", "core.autocrlf=false"] {
            assert!(config.iter().any(|c| c == needed), "{needed}");
        }
    }

    #[test]
    fn hooks_are_pointed_at_an_empty_directory_under_the_scratch_directory() {
        let dir = scratch();
        let config = config_of(&git_command(dir.path(), None));
        let expected = format!("core.hooksPath={}", hooks_dir(dir.path()).display());
        assert!(config.contains(&expected), "{config:?}");
    }

    #[test]
    fn git_runs_in_the_scratch_directory_so_it_never_discovers_another_repository() {
        let dir = scratch();
        let command = git_command(dir.path(), None);
        assert_eq!(command.get_current_dir(), Some(dir.path()));
        let command = git_command(dir.path(), Some(Path::new("some/dir")));
        assert_eq!(command.get_current_dir(), Some(dir.path()));
    }

    #[test]
    fn the_git_dir_is_passed_explicitly() {
        let dir = scratch();
        let args = args_of(&git_command(dir.path(), Some(Path::new("some/dir"))));
        let at = args.iter().position(|a| a == "--git-dir").unwrap();
        assert_eq!(args[at + 1], "some/dir");
    }

    #[test]
    fn a_user_ssh_command_is_kept_and_batch_mode_is_appended() {
        let combined = ssh_command(Some(OsString::from("ssh -i key")));
        assert_eq!(combined, OsString::from("ssh -i key -o BatchMode=yes"));
        assert_eq!(ssh_command(None), OsString::from("ssh -o BatchMode=yes"));
    }

    #[test]
    fn output_and_exit_status_are_captured_as_bytes() {
        let dir = scratch();
        let command = with_args(git_command(dir.path(), None), ["--version"]);
        let output = run(command, None, Duration::from_secs(30)).unwrap();
        assert!(output.status.success());
        assert!(output.stdout.starts_with(b"git version"));
    }

    #[test]
    fn input_is_written_to_stdin() {
        let dir = scratch();
        let command = with_args(git_command(dir.path(), None), ["hash-object", "--stdin"]);
        let output = run(command, Some(b"hello\n".to_vec()), Duration::from_secs(30)).unwrap();
        assert_eq!(
            String::from_utf8_lossy(&output.stdout).trim(),
            "ce013625030ba8dba906f756967f9e9ca394464a"
        );
    }

    #[test]
    fn a_process_past_its_deadline_is_killed_and_reported_as_timed_out() {
        let dir = scratch();
        // A git alias runs `sleep` through git's own shell on every platform.
        let command = with_args(
            git_command(dir.path(), None),
            ["-c", "alias.nap=!sleep 20", "nap"],
        );
        let started = Instant::now();
        let result = run(command, None, Duration::from_millis(300));
        assert_eq!(result.unwrap_err(), RunError::TimedOut);
        assert!(
            started.elapsed() < Duration::from_secs(10),
            "returned promptly: {:?}",
            started.elapsed()
        );
    }

    /// Number of lines in `file`, 0 when it does not exist.
    fn lines_in(file: &Path) -> usize {
        std::fs::read_to_string(file).map_or(0, |text| text.lines().count())
    }

    #[test]
    fn a_timeout_kills_the_whole_process_tree_not_just_git() {
        let dir = scratch();
        let marker = dir.path().join("ticks");
        // git -> sh -> a loop that appends a line every 100 ms for as long as it lives.
        let mut command = with_args(
            git_command(dir.path(), None),
            [
                "-c",
                "alias.tick=!while :; do echo x >> \"$TICKS\"; sleep 0.1; done",
                "tick",
            ],
        );
        command.env("TICKS", marker.to_string_lossy().replace('\\', "/"));
        let result = run(command, None, Duration::from_millis(1500));
        assert_eq!(result.unwrap_err(), RunError::TimedOut);
        assert!(lines_in(&marker) > 0, "fixture: the grandchild ran");

        std::thread::sleep(Duration::from_millis(500));
        let settled = lines_in(&marker);
        std::thread::sleep(Duration::from_millis(1000));
        assert_eq!(
            lines_in(&marker),
            settled,
            "a descendant of git is still running after the timeout"
        );
    }

    #[test]
    fn output_beyond_the_cap_kills_the_process_and_is_reported() {
        let dir = scratch();
        let command = with_args(
            git_command(dir.path(), None),
            ["-c", "alias.flood=!yes", "flood"],
        );
        let started = Instant::now();
        let result = run_capped(command, None, Duration::from_secs(60), 64 * 1024);
        assert_eq!(result.unwrap_err(), RunError::OutputTooLarge);
        assert!(
            started.elapsed() < Duration::from_secs(20),
            "{:?}",
            started.elapsed()
        );
    }

    #[test]
    fn output_within_the_cap_is_returned_whole() {
        let dir = scratch();
        let command = with_args(git_command(dir.path(), None), ["--version"]);
        let output = run_capped(command, None, Duration::from_secs(30), 1024).unwrap();
        assert!(output.stdout.starts_with(b"git version"));
    }

    #[test]
    fn a_missing_program_is_an_io_error() {
        let command = Command::new("definitely-not-a-program-rag");
        assert!(matches!(
            run(command, None, Duration::from_secs(5)),
            Err(RunError::Io(_))
        ));
    }
}
