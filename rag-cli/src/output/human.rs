//! Human output (`--human`): compact plain text, no ANSI, one result per block.
//! Every chunk, path, and name is untrusted data: control and format characters
//! are replaced so a repository cannot write to the terminal.

use rag_core::application::dto::{ListResponse, SearchResponse, SourceSummary};
use rag_core::application::error::AppError;
use rag_core::domain::untrusted_text::sanitize_untrusted;

/// Repository text made safe for a terminal: control and invisible characters are
/// replaced (`sanitize_untrusted`, the one rule the core also applies). Newlines and
/// tabs are kept.
pub fn terminal_safe(text: &str) -> String {
    sanitize_untrusted(text)
}

/// `terminal_safe` on one line: newlines and tabs become spaces, so a cell stays one
/// line and a table stays aligned.
pub fn cell(text: &str) -> String {
    terminal_safe(text).replace(['\n', '\t'], " ")
}
/// Indent of snippet lines and of the detail lines under a result.
const INDENT: &str = "    ";

/// Search results as plain text blocks: a header, one block per result, then the
/// paging notes and the hint.
pub fn render_search_human(response: &SearchResponse) -> String {
    let at_least = if response.total_is_exact {
        ""
    } else {
        "at least "
    };
    let mut out = format!(
        "query \"{}\": {} of {at_least}{} matches (mode {})\n",
        cell(&response.query),
        response.returned,
        response.total_hits,
        response.mode
    );
    for result in &response.results {
        out += &format!(
            "{}. {} {}:{}-{}  score_rel {:.2}\n",
            result.rank,
            cell(&result.repo),
            cell(&result.path),
            result.lines[0],
            result.lines[1],
            result.score_rel
        );
        if let Some(heading) = &result.heading {
            out += &format!("{INDENT}heading: {}\n", cell(heading));
        }
        for line in result.snippet.split('\n') {
            out += &format!("{INDENT}{}\n", terminal_safe(line));
        }
        out += &format!("{INDENT}show: {}\n", cell(&result.show));
    }
    if response.has_more {
        out += &format!(
            "next page: --offset {}\n",
            response.offset + response.returned
        );
    }
    if response.truncated {
        out += "output cut to the size budget: narrow the query or raise --max-chars\n";
    }
    for notice in &response.notices {
        out += &format!("notice: {}\n", cell(notice));
    }
    if let Some(hint) = &response.hint {
        out += &format!("hint: {}\n", cell(hint));
    }
    out
}

const NO_SOURCES: &str = "no sources yet; run `rag add <url|path>` to index a repository\n";

/// Registered sources as an aligned table. Columns are separated by two spaces;
/// the last column is not padded.
pub fn render_list_human(response: &ListResponse) -> String {
    if response.sources.is_empty() {
        return NO_SOURCES.to_owned();
    }
    let header = ["NAME", "REPO", "STATUS", "COMMIT", "AGE", "CHUNKS"].map(str::to_owned);
    let rows: Vec<[String; 6]> = response.sources.iter().map(source_cells).collect();
    let mut table = vec![header];
    table.extend(rows);
    let widths: Vec<usize> = (0..5)
        .map(|column| {
            table
                .iter()
                .map(|row| row[column].chars().count())
                .max()
                .unwrap_or(0)
        })
        .collect();
    let mut out = String::new();
    for row in &table {
        let mut line = String::new();
        for (column, width) in widths.iter().enumerate() {
            line += &format!("{:<width$}  ", row[column]);
        }
        line += &row[5];
        out += line.trim_end();
        out.push('\n');
    }
    for summary in &response.sources {
        if let Some(error) = &summary.last_error {
            out += &format!(
                "{}: last error {} ago ({}): {}\n",
                cell(&summary.name),
                age(Some(error.age_seconds)),
                cell(&error.code),
                cell(&error.message)
            );
        }
    }
    out
}

fn source_cells(summary: &SourceSummary) -> [String; 6] {
    let commit = summary
        .commit
        .as_deref()
        .map_or_else(|| "-".to_owned(), |commit| commit.chars().take(7).collect());
    [
        cell(&summary.name),
        cell(&summary.repo),
        cell(&summary.status),
        commit,
        age(summary.age_seconds),
        summary.chunks.to_string(),
    ]
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

/// An error as `error: <message>` and, when the code has one, `hint: <hint>`.
pub fn render_error_human(error: &AppError) -> String {
    let mut out = format!("error: {}\n", cell(&error.to_string()));
    if let Some(hint) = error.code().default_hint() {
        out += &format!("hint: {hint}\n");
    }
    out
}

#[cfg(test)]
mod tests {
    use super::{render_error_human, render_list_human, render_search_human, terminal_safe};
    use crate::output::test_support::{response, result, INDEXED_AT};
    use rag_core::application::dto::{
        LastErrorDto, ListResponse, SearchResponse, SourceSummary, SCHEMA_VERSION,
    };
    use rag_core::application::error::{AppError, StorageError};
    use rag_core::application::search::NO_MATCH_HINT;
    use rag_core::domain::git_location::GitLocation;
    use rag_core::domain::indexed_at::IndexedAt;
    use rag_core::domain::oid::CommitId;
    use rag_core::domain::source::Source;

    fn two_hits() -> SearchResponse {
        response(
            "parse config",
            vec![
                result(
                    1,
                    "src/parse.rs",
                    10,
                    "fn parse_config() {\n    load()\n}",
                    2.5,
                    1.0,
                ),
                result(
                    2,
                    "docs/config.md",
                    3,
                    "Use `parse config` to start.",
                    1.25,
                    0.5,
                ),
            ],
        )
    }

    #[test]
    fn search_blocks_show_rank_location_score_snippet_and_show_command() {
        let id1 = format!("{:064x}", 1);
        let id2 = format!("{:064x}", 2);
        let expected = format!(
            "query \"parse config\": 2 of 2 matches (mode and)\n\
             1. github.com/o/r src/parse.rs:10-12  score_rel 1.00\n\
             \x20   fn parse_config() {{\n\
             \x20       load()\n\
             \x20   }}\n\
             \x20   show: rag show --chunk {id1}\n\
             2. github.com/o/r docs/config.md:3-3  score_rel 0.50\n\
             \x20   Use `parse config` to start.\n\
             \x20   show: rag show --chunk {id2}\n"
        );
        assert_eq!(render_search_human(&two_hits()), expected);
    }

    #[test]
    fn a_heading_is_shown_under_the_location() {
        let mut headed = result(3, "guide.md", 5, "Run `rag add`.", 1.0, 1.0);
        headed.heading = Some("Guide > Setup".to_owned());
        let text = render_search_human(&response("rag", vec![headed]));
        assert!(text.contains("\n    heading: Guide > Setup\n"), "{text}");
    }

    #[test]
    fn an_empty_search_says_so_and_gives_the_hint() {
        let mut empty = response("alpha beta", vec![]);
        empty.mode = "or_fallback".to_owned();
        empty.hint = Some(NO_MATCH_HINT.to_owned());
        assert_eq!(
            render_search_human(&empty),
            format!(
                "query \"alpha beta\": 0 of 0 matches (mode or_fallback)\nhint: {NO_MATCH_HINT}\n"
            )
        );
    }

    #[test]
    fn a_page_with_more_hits_names_the_next_offset() {
        let mut page = response("alpha", vec![result(5, "a.rs", 1, "alpha", 1.0, 1.0)]);
        page.offset = 4;
        page.has_more = true;
        let text = render_search_human(&page);
        assert!(text.contains("next page: --offset 5\n"), "{text}");
    }

    #[test]
    fn an_inexact_total_is_shown_as_a_lower_bound() {
        let mut page = response("alpha", vec![result(1, "a.rs", 1, "alpha", 1.0, 1.0)]);
        page.total_hits = 1000;
        page.total_is_exact = false;
        let text = render_search_human(&page);
        assert!(text.contains("1 of at least 1000 matches"), "{text}");
    }

    #[test]
    fn terminal_escapes_in_chunk_text_are_not_written_out() {
        let page = response(
            "alpha",
            vec![result(
                1,
                "a.rs",
                1,
                "alpha \u{1b}[2J\u{7}\u{202e}end",
                1.0,
                1.0,
            )],
        );
        let text = render_search_human(&page);
        assert!(!text.contains('\u{1b}') && !text.contains('\u{7}') && !text.contains('\u{202e}'));
        assert!(text.contains("alpha ?[2J??end"), "{text}");
    }

    #[test]
    fn terminal_safe_keeps_newlines_and_plain_text() {
        assert_eq!(terminal_safe("a\nb\tc"), "a\nb\tc");
        assert_eq!(terminal_safe("hÃ©llo æ—¥æœ¬"), "hÃ©llo æ—¥æœ¬");
    }

    fn summary(name: &str, url: &str, ready_at: Option<i64>, chunks: usize) -> SourceSummary {
        let location = GitLocation::parse(url).unwrap();
        let id = rag_core::domain::source_id::SourceId::from_location(&location).unwrap();
        let mut source = Source::register(id, name, location);
        if let Some(at) = ready_at {
            source.mark_ready(
                CommitId::parse(&"ab".repeat(20)).unwrap(),
                IndexedAt::from_epoch_seconds(at),
            );
        }
        SourceSummary::new(
            &source,
            chunks,
            IndexedAt::from_epoch_seconds(INDEXED_AT + 90),
        )
    }

    #[test]
    fn list_is_an_aligned_table_without_trailing_spaces() {
        let response = ListResponse {
            schema_version: SCHEMA_VERSION,
            sources: vec![
                summary("r", "https://github.com/o/r", Some(INDEXED_AT), 2),
                summary("x", "https://github.com/o/x", None, 0),
            ],
        };
        let expected = "\
NAME  REPO            STATUS   COMMIT   AGE  CHUNKS
r     github.com/o/r  ready    abababa  1m   2
x     github.com/o/x  cloning  -        -    0
";
        assert_eq!(render_list_human(&response), expected);
    }

    #[test]
    fn an_empty_registry_says_how_to_start() {
        let response = ListResponse {
            schema_version: SCHEMA_VERSION,
            sources: vec![],
        };
        assert_eq!(
            render_list_human(&response),
            "no sources yet; run `rag add <url|path>` to index a repository\n"
        );
    }

    #[test]
    fn a_source_name_with_control_characters_cannot_break_the_table() {
        let response = ListResponse {
            schema_version: SCHEMA_VERSION,
            sources: vec![summary("a\u{1b}[31mb", "https://github.com/o/r", None, 0)],
        };
        let text = render_list_human(&response);
        assert!(!text.contains('\u{1b}'));
        assert_eq!(text.lines().count(), 2, "{text}");
    }

    #[test]
    fn notices_come_before_the_hint() {
        let mut page = response("alpha", vec![]);
        page.notices = vec!["r is still being indexed".to_owned()];
        page.hint = Some("ask the user".to_owned());
        let text = render_search_human(&page);
        assert!(
            text.ends_with("notice: r is still being indexed\nhint: ask the user\n"),
            "{text}"
        );
    }

    #[test]
    fn a_failed_source_shows_why_under_the_table() {
        let mut broken = summary("r", "https://github.com/o/r", Some(INDEXED_AT), 2);
        broken.last_error = Some(LastErrorDto {
            code: "network_or_auth".to_owned(),
            message: "git: operation timed out".to_owned(),
            at: "2023-11-14T22:14:10Z".to_owned(),
            age_seconds: 80,
        });
        let response = ListResponse {
            schema_version: SCHEMA_VERSION,
            sources: vec![broken],
        };
        let text = render_list_human(&response);
        assert!(
            text.ends_with("r: last error 1m ago (network_or_auth): git: operation timed out\n"),
            "{text}"
        );
    }
    #[test]
    fn errors_show_the_message_and_the_hint() {
        assert_eq!(
            render_error_human(&AppError::Storage(StorageError::IndexMissing)),
            "error: no index exists yet\nhint: run `rag add <url|path>` to index a repository\n"
        );
    }

    #[test]
    fn errors_without_a_hint_are_one_line() {
        assert_eq!(
            render_error_human(&AppError::Usage("bad flag".into())),
            "error: bad flag\n"
        );
    }

    #[test]
    fn human_output_has_no_ansi_escape_bytes() {
        let text = render_search_human(&two_hits());
        assert!(!text.contains('\u{1b}'));
    }
}
