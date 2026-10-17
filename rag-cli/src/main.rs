//! `rag` command-line entry point. The one place that prints and sets the exit
//! code: parsing is in `cli`, wiring in `app`, rendering in `output`.

// The presentation layer legitimately prints and exits; the workspace denies apply to core/infra.
#![allow(clippy::print_stdout, clippy::print_stderr, clippy::exit)]

use std::ffi::OsString;
use std::io::Write;
use std::process::ExitCode;
use std::sync::Arc;

use clap::error::ErrorKind;
use clap::Parser;
use rag_core::application::error::{AppError, Scrubbed};
use rag_core::domain::git_location::scrub_credentials;
use rag_core::domain::untrusted_text::sanitize_untrusted;
use rag_infra::deadline::Deadline;

mod app;
mod cli;
mod crash;
mod location;
mod output;
mod progress;

use app::{Mode, RunOptions};
use cli::Cli;
use progress::{Sink, StderrProgress, HEARTBEAT};

fn main() -> ExitCode {
    crash::install_hook();
    let args: Vec<OsString> = std::env::args_os().collect();
    let parsed = Cli::try_parse_from(&args);
    // Decided by the parser, so that a usage error is rendered in the mode the
    // caller asked for.
    let mode = output_mode(&parsed, &args);
    // A panic anywhere below is reported as the generic error, exit code 1.
    crash::guarded(|| execute(parsed, mode)).unwrap_or_else(|error| fail(&error, mode))
}

/// The output mode: the `--human` flag as clap parses it when the arguments parse, so
/// a query that is literally `--human` (given after `--`) is just a query. When they
/// do not parse, the error is still rendered in the mode asked for: `--human` counts
/// when it stands before the `--` terminator.
fn output_mode(parsed: &Result<Cli, clap::Error>, args: &[OsString]) -> Mode {
    let human = match parsed {
        Ok(cli) => cli.human,
        Err(_) => args
            .iter()
            .take_while(|arg| *arg != "--")
            .any(|arg| arg == "--human"),
    };
    if human {
        Mode::Human
    } else {
        Mode::Json
    }
}

fn execute(parsed: Result<Cli, clap::Error>, mode: Mode) -> ExitCode {
    let cli = match parsed {
        Ok(cli) => cli,
        Err(error) => return finish_parse_error(&error, mode),
    };
    let sink: Sink = Arc::new(|line| write_stderr(&format!("rag: {line}\n")));
    // The budget starts now: it covers waiting for a lock, fetching, and indexing.
    let deadline = Deadline::after_seconds(cli.deadline);
    let progress = StderrProgress::new(sink, HEARTBEAT).with_deadline(deadline);
    let options = RunOptions {
        progress: &progress,
        no_wait: cli.no_wait,
        deadline,
    };
    match app::run(cli.command, mode, &options) {
        Ok(executed) => {
            write_stdout(&executed.stdout);
            ExitCode::from(executed.code)
        }
        Err(error) => fail(&error, mode),
    }
}

/// Help and version go to stdout with exit 0; anything else is a usage error.
fn finish_parse_error(error: &clap::Error, mode: Mode) -> ExitCode {
    if matches!(
        error.kind(),
        ErrorKind::DisplayHelp | ErrorKind::DisplayVersion
    ) {
        // A closed pipe is not worth a message.
        let _ = error.print();
        return ExitCode::SUCCESS;
    }
    // clap quotes the offending argument, which may be a URL with a password.
    let rendered = safe_text(&error.to_string());
    // Without a command clap prints the help text, whose first line is not a message.
    let missing_command = error.kind() == ErrorKind::DisplayHelpOnMissingArgumentOrSubcommand;
    let message = if missing_command {
        "a command is required; run `rag --help` to see them".to_owned()
    } else {
        usage_error_message(&rendered)
    };
    let usage = AppError::Usage(Scrubbed::from(message));
    if mode == Mode::Json {
        write_stdout(&output::error::render_error_json(&usage));
    }
    if missing_command {
        write_stderr(&format!("error: {usage}\n\n"));
    }
    write_stderr(&rendered);
    ExitCode::from(output::error::exit_code(&usage))
}

/// The message of a clap usage error: its first paragraph on one line (what is
/// wrong), then `; usage: <the usage line of the subcommand>` so the caller can fix
/// the command without another round trip.
fn usage_error_message(rendered: &str) -> String {
    let detail = rendered
        .lines()
        .take_while(|line| !line.trim().is_empty())
        .map(|line| line.trim().trim_start_matches("error: "))
        .collect::<Vec<_>>()
        .join(" ");
    match rendered
        .lines()
        .find_map(|line| line.trim_start().strip_prefix("Usage: "))
    {
        Some(usage) => format!("{detail}; usage: {}", usage.trim()),
        None => detail,
    }
}

/// Text from outside, safe to print: credentials removed, control and invisible
/// characters replaced (line breaks and tabs stay).
fn safe_text(text: &str) -> String {
    sanitize_untrusted(&scrub_credentials(text))
}

/// JSON mode: the error object on stdout and a plain line on stderr. Human mode:
/// the message and hint on stderr only.
fn fail(error: &AppError, mode: Mode) -> ExitCode {
    match mode {
        Mode::Json => {
            write_stdout(&output::error::render_error_json(error));
            write_stderr(&format!("error: {error}\n"));
        }
        Mode::Human => write_stderr(&output::human::render_error_human(error)),
    }
    ExitCode::from(output::error::exit_code(error))
}

/// Writes `text` and a newline; a closed pipe is ignored.
fn write_stdout(text: &str) {
    let mut stdout = std::io::stdout().lock();
    let _ = stdout.write_all(text.as_bytes());
    if !text.ends_with('\n') {
        let _ = stdout.write_all(b"\n");
    }
}

/// Writes `text` to stderr; a closed stderr is ignored (`eprintln!` would panic).
fn write_stderr(text: &str) {
    let _ = std::io::stderr().write_all(text.as_bytes());
}

#[cfg(test)]
mod tests {
    use super::{output_mode, safe_text, usage_error_message, Mode};
    use crate::cli::Cli;
    use clap::Parser;
    use std::ffi::OsString;

    fn args(list: &[&str]) -> Vec<OsString> {
        std::iter::once("rag")
            .chain(list.iter().copied())
            .map(OsString::from)
            .collect()
    }

    fn mode_of(list: &[&str]) -> Mode {
        let args = args(list);
        output_mode(&Cli::try_parse_from(&args), &args)
    }

    fn rendered(list: &[&str]) -> String {
        safe_text(&Cli::try_parse_from(args(list)).unwrap_err().to_string())
    }

    #[test]
    fn the_human_flag_selects_human_output_anywhere_in_a_valid_command() {
        assert_eq!(mode_of(&["list", "--human"]), Mode::Human);
        assert_eq!(mode_of(&["--human", "list"]), Mode::Human);
        assert_eq!(mode_of(&["list"]), Mode::Json);
    }

    #[test]
    fn a_query_that_is_literally_the_human_flag_does_not_flip_the_mode() {
        assert_eq!(mode_of(&["search", "--", "--human"]), Mode::Json);
        assert_eq!(mode_of(&["search", "-k", "3", "--", "--human"]), Mode::Json);
    }

    #[test]
    fn an_unparseable_command_still_reports_in_the_mode_asked_for() {
        assert_eq!(mode_of(&["search", "--human"]), Mode::Human);
        assert_eq!(
            mode_of(&["search", "x", "-k", "many", "--human"]),
            Mode::Human
        );
        assert_eq!(mode_of(&["frobnicate"]), Mode::Json);
        assert_eq!(
            mode_of(&["frobnicate", "--", "--human"]),
            Mode::Json,
            "after the terminator it is not a flag"
        );
    }

    #[test]
    fn a_usage_error_message_carries_its_detail_and_the_subcommand_usage_line() {
        let message = usage_error_message(&rendered(&["search"]));
        assert_eq!(
            message,
            "the following required arguments were not provided: <QUERY>; usage: rag search <QUERY>"
        );
        let show = usage_error_message(&rendered(&["show", "only-a-repo"]));
        assert!(show.contains("; usage: rag show "), "{show}");
        let unknown = usage_error_message(&rendered(&["frobnicate"]));
        assert!(unknown.starts_with("unrecognized subcommand"), "{unknown}");
        assert!(unknown.contains("; usage: rag "), "{unknown}");
    }

    #[test]
    fn a_message_without_a_usage_line_is_just_the_detail() {
        assert_eq!(
            usage_error_message("error: plain problem\n"),
            "plain problem"
        );
    }

    #[test]
    fn clap_text_loses_credentials_and_escape_sequences_but_keeps_its_layout() {
        let text = safe_text(
            "error: unexpected argument 'https://user:tok3n@example.com/x'\n\nUsage:\trag\u{1b}[2J",
        );
        assert!(!text.contains("tok3n"), "{text}");
        assert!(!text.contains('\u{1b}'), "{text}");
        assert!(text.contains("\n\nUsage:\trag"), "{text}");
    }
}
