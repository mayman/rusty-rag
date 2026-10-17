//! Output of `rag doctor`: the response as JSON, or a short plain-text report.
//! Names and messages come from the repositories and from git, so every piece of
//! text goes through `cell`.

use rag_core::application::doctor::{DoctorResponse, DoctorSource};

use crate::output::human::cell;

pub fn render_doctor_json(response: &DoctorResponse) -> String {
    serde_json::to_string(response).expect("doctor responses hold only strings and numbers")
}

/// Seconds as the largest whole unit: `45s`, `1m`, `2h`, `3d`.
fn age(seconds: Option<u64>) -> String {
    match seconds {
        None => "-".to_owned(),
        Some(s) if s < 60 => format!("{s}s"),
        Some(s) if s < 3600 => format!("{}m", s / 60),
        Some(s) if s < 86_400 => format!("{}h", s / 3600),
        Some(s) => format!("{}d", s / 86_400),
    }
}

fn source_line(source: &DoctorSource) -> String {
    let summary = &source.summary;
    let mut line = format!(
        "  {}  {}  {}  commit {}  age {}  {} chunks  copy {}  lock {}",
        cell(&summary.name),
        cell(&summary.repo),
        cell(&summary.status),
        summary.commit.as_deref().map_or("-".to_owned(), cell),
        age(summary.age_seconds),
        summary.chunks,
        source.checkout,
        source.operation_lock
    );
    if let Some(operation) = &source.lock_operation {
        line += &format!(" ({})", cell(operation));
    }
    if source.stale_versions {
        line += "  STALE index";
    }
    line
}

pub fn render_doctor_human(response: &DoctorResponse) -> String {
    let database = &response.database;
    let mut out = format!("data dir: {}\n", cell(&response.data_dir));
    if database.exists {
        out += &format!(
            "database: {} bytes, write-ahead log {} bytes",
            database.size_bytes, database.wal_bytes
        );
        if let Some(integrity) = &database.integrity {
            out += &format!(", integrity {}", cell(integrity));
        }
        out.push('\n');
    } else {
        out += "database: none yet\n";
    }
    let git = &response.git;
    out += &match (&git.version, git.on_path) {
        (_, false) => "git: not found on PATH\n".to_owned(),
        (Some(version), true) => format!(
            "git: {}{}\n",
            cell(version),
            if git.supported {
                String::new()
            } else {
                format!(" (needs {} or newer)", git.minimum)
            }
        ),
        (None, true) => "git: version unknown\n".to_owned(),
    };
    if response.sources.is_empty() {
        out += "sources: none\n";
    } else {
        out += "sources:\n";
        for source in &response.sources {
            out += &source_line(source);
            out.push('\n');
        }
    }
    out += &format!(
        "leftovers: {} in tmp/, {} unregistered clones\n",
        response.leftovers.tmp_entries, response.leftovers.unregistered_checkouts
    );
    if response.ok {
        out += "ok: no problems found\n";
    }
    for warning in &response.warnings {
        out += &format!("warning: {}\n", cell(warning));
    }
    out
}
