//! `rag` command-line entry point. The one place that prints and sets the exit
//! code: parsing is in `cli`, wiring in `app`, rendering in `output`.

// The presentation layer legitimately prints and exits; the workspace denies apply to core/infra.
#![allow(clippy::print_stdout, clippy::print_stderr, clippy::exit)]

use std::ffi::OsString;
use std::io::Write;
use std::process::ExitCode;

use clap::error::ErrorKind;
use clap::Parser;
use rag_core::application::error::{AppError, Scrubbed};

mod app;
mod cli;
mod location;
mod output;

use app::Mode;
use cli::Cli;

fn main() -> ExitCode {
    let args: Vec<OsString> = std::env::args_os().collect();
    // Decided from the raw arguments so that a usage error is rendered in the
    // mode the caller asked for.
    let mode = if args.iter().any(|arg| arg == "--human") {
        Mode::Human
    } else {
        Mode::Json
    };
    let cli = match Cli::try_parse_from(&args) {
        Ok(cli) => cli,
        Err(error) => return finish_parse_error(&error, mode),
    };
    let progress = |line: &str| eprintln!("rag: {line}");
    match app::run(cli.command, mode, &progress) {
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
    let rendered = error.to_string();
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
        eprintln!("error: {usage}\n");
    }
    eprint!("{rendered}");
    ExitCode::from(output::error::exit_code(&usage))
}

/// JSON mode: the error object on stdout and a plain line on stderr. Human mode:
/// the message and hint on stderr only.
fn fail(error: &AppError, mode: Mode) -> ExitCode {
    match mode {
        Mode::Json => {
            write_stdout(&output::error::render_error_json(error));
            eprintln!("error: {error}");
        }
        Mode::Human => eprint!("{}", output::human::render_error_human(error)),
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
