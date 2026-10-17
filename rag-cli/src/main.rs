//! `rag` command-line entry point. Parsing and rendering only; no wiring yet.

// The presentation layer legitimately prints and exits; the workspace denies apply to core/infra.
#![allow(clippy::print_stdout, clippy::print_stderr, clippy::exit)]

use clap::Parser;

/// Local-first keyword search over git repositories.
#[derive(Parser)]
#[command(name = "rag", version, about)]
struct Cli {}

fn main() {
    Cli::parse();
}
