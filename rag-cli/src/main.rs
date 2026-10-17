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
    // Decided from the raw arguments so that a usage error is rendered in the
    // mode the caller asked for.
    let mode = if args.iter().any(|arg| arg == "--human") {
        Mode::Human
    } else {
        Mode::Json
    };
    // A panic anywhere below is reported as the generic error, exit code 1.
    crash::guarded(|| execute(&args, mode)).unwrap_or_else(|error| fail(&error, mode))
}

fn execute(args: &[OsString], mode: Mode) -> ExitCode {
    let cli = match Cli::try_parse_from(args) {
        Ok(cli) => cli,
        Err(error) => return finish_parse_error(&error, mode),
    };
    let sink: Sink = Arc::new(|line| write_stderr(&format!("rag: {line}\n")));
    let progress = StderrProgress::new(sink, HEARTBEAT);
    let options = RunOptions {
        progress: &progress,
        no_wait: cli.no_wait,
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
        rendered
            .lines()
            .next()
            .unwrap_or_default()
            .trim_start_matches("error: ")
            .to_owned()
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
    use super::safe_text;

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
