//! Output of the commands that change the index: add, update, remove, reindex.
//! JSON is the serialized response; the human form is one or two lines per source.

use rag_core::application::add_source::AddResponse;
use rag_core::application::index_pipeline::IndexReport;
use rag_core::application::reindex::{ReindexEntry, ReindexResponse};
use rag_core::application::remove_source::RemoveResponse;
use rag_core::application::update_source::{UpdateEntry, UpdateResponse};

use crate::output::human::cell;

pub fn render_add_json(response: &AddResponse) -> String {
    serde_json::to_string(response).expect("add responses hold only strings and numbers")
}

pub fn render_update_json(response: &UpdateResponse) -> String {
    serde_json::to_string(response).expect("update responses hold only strings and numbers")
}

pub fn render_remove_json(response: &RemoveResponse) -> String {
    serde_json::to_string(response).expect("remove responses hold only strings")
}

pub fn render_reindex_json(response: &ReindexResponse) -> String {
    serde_json::to_string(response).expect("reindex responses hold only strings and numbers")
}

fn short(commit: &str) -> String {
    commit.chars().take(7).collect()
}

/// `+2 ~1 -0 files (5 unchanged), 14 chunks written; skipped: binary 2`.
fn index_summary(report: &IndexReport) -> String {
    let mut text = format!(
        "+{} ~{} -{} files ({} unchanged), {} chunks written",
        report.files_added,
        report.files_changed,
        report.files_removed,
        report.files_unchanged,
        report.chunks_written
    );
    let skipped: Vec<String> = report
        .skipped
        .iter()
        .map(|(reason, count)| format!("{reason} {count}"))
        .collect();
    if !skipped.is_empty() {
        text += &format!("; skipped: {}", skipped.join(", "));
    }
    if !report.skipped_secret_paths.is_empty() {
        let paths: Vec<String> = report
            .skipped_secret_paths
            .iter()
            .map(|path| cell(path))
            .collect();
        text += &format!(
            "; left out as secrets ({}): {}",
            paths.len(),
            paths.join(", ")
        );
    }
    text
}

pub fn render_add_human(response: &AddResponse) -> String {
    let what = if response.already_added {
        "already added, now at"
    } else {
        "added, at"
    };
    format!(
        "{} ({}) {what} {}: {}\n",
        cell(&response.name),
        cell(&response.repo),
        short(&response.commit),
        index_summary(&response.index)
    )
}

fn update_line(entry: &UpdateEntry) -> String {
    let name = cell(&entry.name);
    if let Some(error) = &entry.error {
        return match entry.result.as_str() {
            "failed" => format!("{name}: failed: {}\n", cell(&error.message)),
            _ => format!("{name}: skipped, {}\n", cell(&error.message)),
        };
    }
    let Some(report) = &entry.index else {
        return format!("{name}: {}\n", entry.result);
    };
    let now = short(&report.commit);
    match (entry.result.as_str(), &entry.previous_commit) {
        ("unchanged", _) => format!("{name}: unchanged at {now}\n"),
        (result, Some(previous)) => format!(
            "{name}: {result} {} -> {now}: {}\n",
            short(previous),
            index_summary(report)
        ),
        (result, None) => format!("{name}: {result} at {now}: {}\n", index_summary(report)),
    }
}

pub fn render_update_human(response: &UpdateResponse) -> String {
    let mut out: String = response.sources.iter().map(update_line).collect();
    if let Some(message) = &response.message {
        out += &format!("{}\n", cell(message));
    }
    out
}

pub fn render_remove_human(response: &RemoveResponse) -> String {
    let mut out = format!(
        "removed {} ({})\n",
        cell(&response.name),
        cell(&response.repo)
    );
    if !response.checkout_removed {
        let why = response
            .checkout_error
            .as_deref()
            .map_or_else(String::new, |why| format!(" ({})", cell(why)));
        out += &format!("the local clone could not be deleted now{why}; it is cleaned up later\n");
    }
    out
}

fn reindex_line(entry: &ReindexEntry) -> String {
    let name = cell(&entry.name);
    match &entry.error {
        Some(error) if entry.result == "failed" => {
            format!("{name}: failed: {}\n", cell(&error.message))
        }
        Some(error) => format!("{name}: skipped, {}\n", cell(&error.message)),
        None => format!("{name}: {}, {} chunks\n", entry.result, entry.chunks),
    }
}

pub fn render_reindex_human(response: &ReindexResponse) -> String {
    let mut out: String = response.sources.iter().map(reindex_line).collect();
    if let Some(message) = &response.message {
        out += &format!("{}\n", cell(message));
    }
    out
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use super::*;
    use rag_core::application::dto::{ErrorBody, SCHEMA_VERSION};

    const COMMIT_A: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    const COMMIT_B: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";

    fn report(commit: &str) -> IndexReport {
        IndexReport {
            commit: commit.to_owned(),
            files_added: 2,
            files_changed: 1,
            files_removed: 0,
            files_unchanged: 5,
            chunks_written: 14,
            skipped: BTreeMap::from([("binary", 2)]),
            skipped_secret_paths: Vec::new(),
            skipped_unsafe_paths: 0,
            phases: Vec::new(),
        }
    }

    fn added(already_added: bool) -> AddResponse {
        AddResponse {
            schema_version: SCHEMA_VERSION,
            repo: "github.com/o/r".to_owned(),
            name: "r".to_owned(),
            status: "ready".to_owned(),
            commit: COMMIT_A.to_owned(),
            already_added,
            index: report(COMMIT_A),
        }
    }

    #[test]
    fn add_human_names_the_source_commit_and_counts() {
        assert_eq!(
            render_add_human(&added(false)),
            "r (github.com/o/r) added, at aaaaaaa: +2 ~1 -0 files (5 unchanged), 14 chunks written; skipped: binary 2\n"
        );
    }

    #[test]
    fn adding_a_registered_source_again_says_so() {
        assert!(render_add_human(&added(true)).contains("already added, now at aaaaaaa"));
    }

    #[test]
    fn add_json_is_the_serialized_response() {
        let json = render_add_json(&added(false));
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed["schema_version"], 1);
        assert_eq!(parsed["repo"], "github.com/o/r");
        assert_eq!(parsed["index"]["chunks_written"], 14);
        assert_eq!(parsed["index"]["skipped"]["binary"], 2);
    }

    fn entry(result: &str, previous: Option<&str>, index: Option<IndexReport>) -> UpdateEntry {
        UpdateEntry {
            repo: "github.com/o/r".to_owned(),
            name: "r".to_owned(),
            result: result.to_owned(),
            previous_commit: previous.map(str::to_owned),
            index,
            error: None,
        }
    }

    fn updates(sources: Vec<UpdateEntry>, message: Option<&str>) -> UpdateResponse {
        UpdateResponse {
            schema_version: SCHEMA_VERSION,
            failed: sources.iter().filter(|e| e.result == "failed").count(),
            sources,
            message: message.map(str::to_owned),
        }
    }

    #[test]
    fn update_human_shows_the_move_between_commits() {
        let text = render_update_human(&updates(
            vec![entry("updated", Some(COMMIT_A), Some(report(COMMIT_B)))],
            None,
        ));
        assert_eq!(
            text,
            "r: updated aaaaaaa -> bbbbbbb: +2 ~1 -0 files (5 unchanged), 14 chunks written; skipped: binary 2\n"
        );
    }

    #[test]
    fn update_human_says_unchanged_and_failed() {
        let mut failed = entry("failed", Some(COMMIT_A), None);
        failed.error = Some(ErrorBody {
            code: "network_or_auth".to_owned(),
            message: "git: operation timed out".to_owned(),
            hint: None,
        });
        let text = render_update_human(&updates(
            vec![
                entry("unchanged", Some(COMMIT_A), Some(report(COMMIT_A))),
                failed,
            ],
            None,
        ));
        assert_eq!(
            text,
            "r: unchanged at aaaaaaa\nr: failed: git: operation timed out\n"
        );
    }

    #[test]
    fn update_with_no_sources_prints_the_message() {
        let text = render_update_human(&updates(vec![], Some("no sources are registered")));
        assert_eq!(text, "no sources are registered\n");
    }

    #[test]
    fn update_json_carries_the_failure_count() {
        let json = render_update_json(&updates(vec![entry("failed", None, None)], None));
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed["failed"], 1);
    }

    #[test]
    fn remove_human_mentions_a_leftover_clone_only_when_there_is_one() {
        let mut removed = RemoveResponse {
            schema_version: SCHEMA_VERSION,
            repo: "github.com/o/r".to_owned(),
            name: "r".to_owned(),
            checkout_removed: true,
            checkout_error: None,
        };
        assert_eq!(
            render_remove_human(&removed),
            "removed r (github.com/o/r)\n"
        );
        removed.checkout_removed = false;
        assert!(render_remove_human(&removed).contains("cleaned up later"));
        assert!(render_remove_json(&removed).contains(r#""checkout_removed":false"#));
    }

    #[test]
    fn a_skipped_source_is_not_called_a_failure() {
        let mut busy = entry("skipped_busy", Some(COMMIT_A), None);
        busy.error = Some(ErrorBody {
            code: "index_locked".to_owned(),
            message: "another rag process is running `add` on this source".to_owned(),
            hint: None,
        });
        let text = render_update_human(&updates(vec![busy], None));
        assert_eq!(
            text,
            "r: skipped, another rag process is running `add` on this source\n"
        );
    }

    #[test]
    fn a_skipped_reindex_is_not_called_a_failure() {
        let response = ReindexResponse {
            schema_version: SCHEMA_VERSION,
            sources: vec![ReindexEntry {
                repo: "github.com/o/r".to_owned(),
                name: "r".to_owned(),
                result: "skipped_removed".to_owned(),
                chunks: 0,
                index: None,
                error: Some(ErrorBody {
                    code: "not_found".to_owned(),
                    message: "the source was removed while the command ran".to_owned(),
                    hint: None,
                }),
            }],
            failed: 0,
            message: None,
        };
        assert_eq!(
            render_reindex_human(&response),
            "r: skipped, the source was removed while the command ran\n"
        );
    }

    #[test]
    fn secret_skipped_files_are_listed_by_path() {
        let mut with_secrets = report(COMMIT_A);
        with_secrets.skipped_secret_paths = vec![".env".to_owned(), "keys/id_rsa".to_owned()];
        let text = render_add_human(&AddResponse {
            index: with_secrets,
            ..added(false)
        });
        assert!(
            text.contains("left out as secrets (2): .env, keys/id_rsa"),
            "{text}"
        );
    }

    #[test]
    fn a_clone_that_could_not_be_deleted_says_why() {
        let removed = RemoveResponse {
            schema_version: SCHEMA_VERSION,
            repo: "github.com/o/r".to_owned(),
            name: "r".to_owned(),
            checkout_removed: false,
            checkout_error: Some("access denied".to_owned()),
        };
        let text = render_remove_human(&removed);
        assert!(
            text.contains("could not be deleted now (access denied)"),
            "{text}"
        );
    }

    #[test]
    fn reindex_human_lists_the_result_and_chunks_per_source() {
        let response = ReindexResponse {
            schema_version: SCHEMA_VERSION,
            sources: vec![ReindexEntry {
                repo: "github.com/o/r".to_owned(),
                name: "r".to_owned(),
                result: "up_to_date".to_owned(),
                chunks: 14,
                index: None,
                error: None,
            }],
            failed: 0,
            message: None,
        };
        assert_eq!(
            render_reindex_human(&response),
            "r: up_to_date, 14 chunks\n"
        );
        assert!(render_reindex_json(&response).contains(r#""result":"up_to_date""#));
    }
}
