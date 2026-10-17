//! Running `git` as a hardened subprocess with a deadline.
//!
//! Every invocation gets the same environment and `-c` options (no prompts, no
//! pagers, restricted transports), a null stdin unless input is supplied, and a
//! timeout after which the process is killed. Output is captured as bytes.

use std::ffi::{OsStr, OsString};
use std::io::{Read, Write};
use std::path::Path;
use std::process::{Command, ExitStatus, Stdio};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

/// How long to wait for the output pipes to close after the process has exited or
/// was killed. A grandchild that inherited a pipe (a transport helper) can hold it
/// open past the death of `git` itself.
const PIPE_GRACE: Duration = Duration::from_secs(2);

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
    /// The process could not be started (no `git` on PATH) or its pipes failed.
    Io(String),
}

/// Runs `command` to completion, killing it after `timeout`. `input`, when given,
/// is written to stdin (which is then closed); otherwise stdin is null.
pub fn run(
    mut command: Command,
    input: Option<Vec<u8>>,
    timeout: Duration,
) -> Result<Output, RunError> {
    command
        .stdin(if input.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = command.spawn().map_err(|error| {
        RunError::Io(format!("cannot start {:?}: {error}", command.get_program()))
    })?;

    let writer = child.stdin.take().zip(input).map(|(mut stdin, bytes)| {
        // A child that exits early closes the pipe; that error is not ours to report.
        thread::spawn(move || {
            let _ = stdin.write_all(&bytes);
        })
    });
    let stdout = collect(child.stdout.take());
    let stderr = collect(child.stderr.take());

    let deadline = Instant::now() + timeout;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if Instant::now() >= deadline => {
                let _ = child.kill();
                let _ = child.wait();
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
    Ok(Output {
        status,
        stdout: stdout.recv_timeout(PIPE_GRACE).unwrap_or_default(),
        stderr: stderr.recv_timeout(PIPE_GRACE).unwrap_or_default(),
    })
}

fn collect<R: Read + Send + 'static>(pipe: Option<R>) -> mpsc::Receiver<Vec<u8>> {
    let (sender, receiver) = mpsc::channel();
    thread::spawn(move || {
        let mut bytes = Vec::new();
        if let Some(mut pipe) = pipe {
            let _ = pipe.read_to_end(&mut bytes);
        }
        let _ = sender.send(bytes);
    });
    receiver
}

/// `-c` options on every invocation: restricted transports, no interaction, no
/// background work, byte-exact content.
const HARDENING_CONFIG: &[&str] = &[
    "protocol.allow=never",
    "protocol.https.allow=always",
    "protocol.ssh.allow=always",
    "protocol.file.allow=always",
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
/// another set of objects.
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
];

/// The ssh command: the user's `GIT_SSH_COMMAND` if any, never asking for input.
fn ssh_command(user_value: Option<OsString>) -> OsString {
    let mut command = user_value
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| OsString::from("ssh"));
    command.push(" -o BatchMode=yes");
    command
}

/// A hardened `git` command. `git_dir`, when given, is passed as `--git-dir` so
/// that `git` never discovers some other repository above a missing directory.
pub fn git_command(git_dir: Option<&Path>) -> Command {
    let mut command = Command::new("git");
    for name in SCRUBBED_ENV {
        command.env_remove(name);
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

    #[test]
    fn commands_never_prompt_and_use_the_c_locale() {
        let env = env_of(&git_command(None));
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
        let env = env_of(&git_command(None));
        for name in ["GIT_DIR", "GIT_WORK_TREE", "GIT_INDEX_FILE", "GIT_SSH"] {
            assert_eq!(env.get(name), Some(&None), "{name} must be unset");
        }
    }

    #[test]
    fn transports_are_denied_by_default_with_an_explicit_allow_list() {
        let args = args_of(&git_command(None));
        let config: Vec<&str> = args
            .chunks(2)
            .filter(|pair| pair[0] == "-c")
            .map(|pair| pair[1].as_str())
            .collect();
        assert_eq!(config.first(), Some(&"protocol.allow=never"));
        for allowed in ["https", "ssh", "file"] {
            assert!(config.contains(&format!("protocol.{allowed}.allow=always").as_str()));
        }
        for needed in ["core.longpaths=true", "core.autocrlf=false"] {
            assert!(config.contains(&needed), "{needed}");
        }
    }

    #[test]
    fn the_git_dir_is_passed_explicitly() {
        let args = args_of(&git_command(Some(Path::new("some/dir"))));
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
        let command = with_args(git_command(None), ["--version"]);
        let output = run(command, None, Duration::from_secs(30)).unwrap();
        assert!(output.status.success());
        assert!(output.stdout.starts_with(b"git version"));
    }

    #[test]
    fn input_is_written_to_stdin() {
        let command = with_args(git_command(None), ["hash-object", "--stdin"]);
        let output = run(command, Some(b"hello\n".to_vec()), Duration::from_secs(30)).unwrap();
        assert_eq!(
            String::from_utf8_lossy(&output.stdout).trim(),
            "ce013625030ba8dba906f756967f9e9ca394464a"
        );
    }

    #[test]
    fn a_process_past_its_deadline_is_killed_and_reported_as_timed_out() {
        // A git alias runs `sleep` through git's own shell on every platform.
        let command = with_args(git_command(None), ["-c", "alias.nap=!sleep 20", "nap"]);
        let started = Instant::now();
        let result = run(command, None, Duration::from_millis(300));
        assert_eq!(result.unwrap_err(), RunError::TimedOut);
        assert!(
            started.elapsed() < Duration::from_secs(10),
            "returned promptly even though a grandchild still holds the pipes: {:?}",
            started.elapsed()
        );
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
