//! `rag` command-line entry point. Parsing and rendering only; no wiring yet.

use clap::Parser;

/// Local-first keyword search over git repositories.
#[derive(Parser)]
#[command(name = "rag", version, about)]
struct Cli {}

fn main() {
    Cli::parse();
}
