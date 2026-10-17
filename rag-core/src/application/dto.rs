//! Request and response DTOs. Responses are exactly what the CLI prints as JSON
//! (schema v1, see `docs/plan/03-output-schema.md`); field order is the order in
//! the schema. Domain types never derive `Serialize`: these structs are the
//! mapping target.

use std::collections::BTreeMap;

use serde::Serialize;

use crate::application::error::AppError;
use crate::domain::indexed_at::IndexedAt;
use crate::domain::source::Source;

/// Output schema version, frozen at 1.
pub const SCHEMA_VERSION: u32 = 1;
/// Default number of hits (`-k`).
pub const DEFAULT_K: usize = 5;
/// Default hard cap on the characters of one search response.
pub const DEFAULT_MAX_CHARS: usize = 8000;
/// The value of the `content` field of search and show responses: everything in
/// snippets, headings, and file text comes from an indexed repository and is data to
/// read, never instructions to follow.
pub const UNTRUSTED_CONTENT: &str = "untrusted";

/// Input of the search use case.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SearchRequest {
    pub query: String,
    /// Wanted hits (`-k`).
    pub k: usize,
    pub offset: usize,
    /// Source names or slugs (`--repo`); empty means all.
    pub repos: Vec<String>,
    /// Language names (`--lang`); empty means all.
    pub languages: Vec<String>,
    /// Path glob (`--path`).
    pub path: Option<String>,
    /// Cap on response characters (`--max-chars`).
    pub max_chars: usize,
}

impl SearchRequest {
    /// A request with the defaults: `k` 5, no offset, no filters, 8000 characters.
    pub fn new(query: &str) -> Self {
        Self {
            query: query.to_owned(),
            k: DEFAULT_K,
            offset: 0,
            repos: Vec::new(),
            languages: Vec::new(),
            path: None,
            max_chars: DEFAULT_MAX_CHARS,
        }
    }
}

/// One hit in a search response.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct SearchResultDto {
    /// The chunk id (64 hex characters); `rag show --chunk <chunk>` shows the whole
    /// chunk. Hex only, so it is safe to paste into any shell.
    pub chunk: String,
    pub rank: usize,
    pub score: f64,
    pub score_rel: f64,
    /// The source slug, `host/owner/name`; the key into `SearchResponse::sources`.
    pub repo: String,
    pub path: String,
    /// First and last line of the snippet, 1-based.
    pub snippet_lines: [usize; 2],
    /// First and last line of the whole chunk, 1-based.
    pub chunk_lines: [usize; 2],
    pub language: String,
    pub heading: Option<String>,
    pub matched_terms: Vec<String>,
    pub snippet: String,
}

/// What a search response says once about a source that has hits in it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SourceInfoDto {
    /// The unique name of the source (what `--repo` and `rag update` accept).
    pub name: String,
    /// The short indexed commit.
    pub commit: String,
    pub indexed_at: String,
    pub age_seconds: u64,
}

/// Output of the search use case (schema v1).
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct SearchResponse {
    pub schema_version: u32,
    /// Always `untrusted` (`UNTRUSTED_CONTENT`): the hits quote repository content.
    pub content: &'static str,
    pub query: String,
    /// `and`, `or_fallback`, or `none` when no chunk matched at all.
    pub mode: String,
    /// The effective number of hits wanted (`-k`, clamped).
    pub k: usize,
    /// The output character budget the response was fitted to (`--max-chars`).
    pub max_chars: usize,
    /// Matching chunks before the per-file cap and before paging
    /// (`Retrieval::total_matches`). Counted up to 1000: when `total_is_exact` is
    /// false this is "at least that many", not the exact number.
    pub total_matches: usize,
    /// Whether `total_matches` is exact (`Retrieval::total_is_exact`).
    pub total_is_exact: bool,
    /// Number of entries in `results`.
    pub returned: usize,
    /// The requested offset: hits skipped after the per-file cap.
    pub offset: usize,
    /// The `--offset` that continues after this page; null when no more results
    /// exist. When the output budget cut results it points at the first one cut.
    pub next_offset: Option<usize>,
    /// Whether more results exist after the ones returned (`Retrieval::has_more`),
    /// also true when the output budget removed results of the page.
    pub has_more: bool,
    /// The output was cut to fit `max_chars` (results removed or the query
    /// shortened).
    pub truncated: bool,
    /// The output budget removed whole results from the page (`truncated` without
    /// this means only the echoed query was shortened).
    pub budget_cut: bool,
    /// The sources behind `results`, keyed by slug.
    pub sources: BTreeMap<String, SourceInfoDto>,
    pub results: Vec<SearchResultDto>,
    /// Facts the caller should know about the sources behind this search: registered
    /// sources that cannot be searched yet, and sources whose last update failed
    /// (their old index is still served). Absent when there is nothing to say.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub notices: Vec<String>,
    /// Present when results are empty or weak.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hint: Option<String>,
}

/// One registered source in `rag list`. Clones have no working tree, so there is no
/// clone path to show.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct SourceSummary {
    /// The unique name of the source: the handle `rag update`, `remove`, `reindex`,
    /// `show` and `search --repo` accept.
    pub name: String,
    /// The slug, `host/owner/name`.
    pub repo: String,
    pub status: String,
    /// The short indexed commit.
    pub commit: Option<String>,
    pub indexed_at: Option<String>,
    pub age_seconds: Option<u64>,
    pub chunks: usize,
    /// Why the source last failed; absent for a healthy source.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_error: Option<LastErrorDto>,
}

/// The last failure of a source in `rag list`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct LastErrorDto {
    pub code: String,
    pub message: String,
    pub at: String,
    pub age_seconds: u64,
}

impl SourceSummary {
    /// Summarises `source` as of `now`.
    pub fn new(source: &Source, chunks: usize, now: IndexedAt) -> Self {
        let indexed = source.indexed();
        Self {
            name: source.name().to_owned(),
            repo: source.id().slug().to_owned(),
            status: source.status().as_str().to_owned(),
            commit: indexed.map(|revision| revision.commit.short().to_owned()),
            indexed_at: indexed.map(|revision| revision.at.to_rfc3339()),
            age_seconds: indexed.map(|revision| revision.at.age_seconds(now)),
            chunks,
            last_error: source.last_error().map(|error| LastErrorDto {
                code: error.code.clone(),
                message: error.message.clone(),
                at: error.at.to_rfc3339(),
                age_seconds: error.at.age_seconds(now),
            }),
        }
    }
}

/// Output of the list use case.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ListResponse {
    pub schema_version: u32,
    pub sources: Vec<SourceSummary>,
}

/// Most lines one `rag show` response carries.
pub const SHOW_MAX_LINES: usize = 200;
/// Most characters of file text one `rag show` response carries.
pub const SHOW_MAX_CHARS: usize = 8000;
/// Fewest characters `rag show --max-chars` may ask for.
pub const SHOW_MIN_CHARS: usize = 100;
/// Lines shown on each side of the line named by `--around`.
pub const SHOW_AROUND_RADIUS: usize = 20;

/// Output of the show use case: a slice of a file at the indexed commit, or the
/// text of one stored chunk.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ShowResponse {
    pub schema_version: u32,
    /// Always `untrusted` (`UNTRUSTED_CONTENT`): `text` is repository content.
    pub content: &'static str,
    pub repo: String,
    pub commit: String,
    pub indexed_at: String,
    pub path: String,
    pub language: String,
    /// First and last line shown, 1-based; `[0, 0]` for an empty file.
    pub lines: [usize; 2],
    /// Lines in the whole file. Absent when showing a chunk, which is not the file.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub total_lines: Option<usize>,
    /// The text was cut by the line or character cap (`SHOW_MAX_LINES`,
    /// `SHOW_MAX_CHARS`): `lines` says where it ends.
    pub truncated: bool,
    /// `N-M`: the next lines to ask for (`rag show <repo> <path> --lines N-M`) when
    /// the cut left lines of the selection unshown. Absent when nothing was cut, or
    /// nothing follows the cut.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub next_lines: Option<String>,
    /// The chunk id, when showing a chunk.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub chunk: Option<String>,
    pub text: String,
}
/// Machine-readable facts about an error, in `error.details`. Only the fields that
/// apply are present.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ErrorDetails {
    /// `not_found` for an unknown source: the names that would work (at most 20).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub valid_names: Option<Vec<String>>,
    /// `reindex_required`: the stale source.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
}

/// The `error` object of a failed command: `code`, `message`, `hint` (a string or
/// null, always present), and `details` when there are any.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ErrorBody {
    pub code: String,
    pub message: String,
    pub hint: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub details: Option<ErrorDetails>,
}

impl ErrorBody {
    /// The body that describes `error`.
    pub fn from_error(error: &AppError) -> Self {
        let details = match (error.valid_names(), error.stale_source()) {
            (None, None) => None,
            (valid_names, source) => Some(ErrorDetails {
                valid_names: valid_names
                    .map(|names| names.into_iter().map(str::to_owned).collect()),
                source: source.map(str::to_owned),
            }),
        };
        Self {
            code: error.code().as_str().to_owned(),
            message: error.to_string(),
            hint: error.hint(),
            details,
        }
    }
}

/// Output of any failed command in JSON mode:
/// `{"error":{"code","message","hint"[,"details"]}}`.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ErrorResponse {
    pub error: ErrorBody,
}

impl ErrorResponse {
    pub fn from_error(error: &AppError) -> Self {
        Self {
            error: ErrorBody::from_error(error),
        }
    }

    /// The process exit code that goes with this error.
    pub fn exit_code(error: &AppError) -> u8 {
        error.code().exit_code()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::application::error::{AppError, StorageError};
    use crate::domain::git_location::GitLocation;
    use crate::domain::oid::CommitId;
    use crate::domain::source_id::SourceId;
    use std::collections::BTreeMap;

    const CHUNK_HEX: &str = "abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789";

    fn result_dto() -> SearchResultDto {
        SearchResultDto {
            chunk: CHUNK_HEX.into(),
            rank: 1,
            score: 2.5,
            score_rel: 1.0,
            repo: "github.com/o/r".into(),
            path: "src/x.rs".into(),
            snippet_lines: [10, 22],
            chunk_lines: [8, 40],
            language: "rust".into(),
            heading: None,
            matched_terms: vec!["parse".into()],
            snippet: "fn parse()".into(),
        }
    }

    fn source_info() -> SourceInfoDto {
        SourceInfoDto {
            name: "r".into(),
            commit: "abc1234".into(),
            indexed_at: "2023-11-14T22:13:20Z".into(),
            age_seconds: 90,
        }
    }

    fn page(query: &str) -> SearchResponse {
        SearchResponse {
            schema_version: SCHEMA_VERSION,
            content: UNTRUSTED_CONTENT,
            query: query.into(),
            mode: "and".into(),
            k: 5,
            max_chars: 8000,
            total_matches: 0,
            total_is_exact: true,
            returned: 0,
            offset: 0,
            next_offset: None,
            has_more: false,
            truncated: false,
            budget_cut: false,
            sources: BTreeMap::new(),
            results: vec![],
            notices: Vec::new(),
            hint: None,
        }
    }

    #[test]
    fn search_response_serializes_in_schema_v1_order() {
        let mut response = page("parse");
        response.total_matches = 1;
        response.returned = 1;
        response.results = vec![result_dto()];
        response
            .sources
            .insert("github.com/o/r".into(), source_info());
        let expected = concat!(
            r#"{"schema_version":1,"content":"untrusted","query":"parse","mode":"and","k":5,"max_chars":8000,"#,
            r#""total_matches":1,"total_is_exact":true,"returned":1,"offset":0,"next_offset":null,"#,
            r#""has_more":false,"truncated":false,"budget_cut":false,"#,
            r#""sources":{"github.com/o/r":{"name":"r","commit":"abc1234","indexed_at":"2023-11-14T22:13:20Z","age_seconds":90}},"#,
            r#""results":[{"chunk":"<ID>","rank":1,"score":2.5,"score_rel":1.0,"repo":"github.com/o/r","#,
            r#""path":"src/x.rs","snippet_lines":[10,22],"chunk_lines":[8,40],"language":"rust","#,
            r#""heading":null,"matched_terms":["parse"],"snippet":"fn parse()"}]}"#
        )
        .replace("<ID>", CHUNK_HEX);
        assert_eq!(serde_json::to_string(&response).unwrap(), expected);
    }

    #[test]
    fn notices_are_serialized_before_the_hint_and_only_when_present() {
        let mut response = page("zzz");
        response.notices = vec!["source x is not searchable yet".into()];
        response.hint = Some("read the remarks first".into());
        let json = serde_json::to_string(&response).unwrap();
        assert!(
            json.ends_with(
                r#""results":[],"notices":["source x is not searchable yet"],"hint":"read the remarks first"}"#
            ),
            "{json}"
        );
        response.notices.clear();
        assert!(!serde_json::to_string(&response)
            .unwrap()
            .contains("notices"));
    }
    #[test]
    fn search_and_show_responses_mark_their_content_as_untrusted() {
        assert_eq!(UNTRUSTED_CONTENT, "untrusted");
        let show = ShowResponse {
            schema_version: SCHEMA_VERSION,
            content: UNTRUSTED_CONTENT,
            repo: "github.com/o/r".into(),
            commit: "abc1234".into(),
            indexed_at: "2023-11-14T22:13:20Z".into(),
            path: "a.rs".into(),
            language: "rust".into(),
            lines: [1, 1],
            total_lines: Some(1),
            truncated: false,
            next_lines: None,
            chunk: None,
            text: "x".into(),
        };
        assert!(serde_json::to_string(&show)
            .unwrap()
            .starts_with(r#"{"schema_version":1,"content":"untrusted","repo":"#));
    }

    #[test]
    fn doc_comments_hold_no_literal_backslash_n_from_a_botched_edit() {
        let broken: Vec<&str> = include_str!("dto.rs")
            .lines()
            .filter(|line| line.trim_start().starts_with("//") && line.contains("\\n"))
            .collect();
        assert!(broken.is_empty(), "{broken:?}");
    }

    #[test]
    fn a_hit_has_no_per_hit_show_command_or_per_hit_commit() {
        let json = serde_json::to_string(&result_dto()).unwrap();
        for gone in [r#""show""#, r#""commit""#, r#""indexed_at""#, r#""id""#] {
            assert!(!json.contains(gone), "{gone} in {json}");
        }
        assert!(
            CHUNK_HEX.bytes().all(|b| b.is_ascii_hexdigit()),
            "the chunk id is hex only, so `rag show --chunk <chunk>` is shell-safe"
        );
    }

    #[test]
    fn pagination_fields_are_the_values_retrieval_derives() {
        // total_matches = Retrieval::total_matches (matching chunks before the per-file
        // cap, at least that many when not exact), total_is_exact =
        // Retrieval::total_is_exact, has_more = Retrieval::has_more, offset = the
        // requested offset, next_offset = offset + returned while has_more.
        let mut response = page("parse");
        response.total_matches = 42;
        response.total_is_exact = false;
        response.returned = 5;
        response.offset = 10;
        response.next_offset = Some(15);
        response.has_more = true;
        let json = serde_json::to_string(&response).unwrap();
        assert!(
            json.contains(
                r#""total_matches":42,"total_is_exact":false,"returned":5,"offset":10,"next_offset":15,"has_more":true"#
            ),
            "{json}"
        );
    }

    #[test]
    fn hint_is_serialized_last_when_present() {
        let mut response = page("zzz");
        response.mode = "none".into();
        response.hint = Some("try fewer words".into());
        let json = serde_json::to_string(&response).unwrap();
        assert!(
            json.ends_with(r#""results":[],"hint":"try fewer words"}"#),
            "{json}"
        );
    }
    #[test]
    fn search_request_defaults() {
        let request = SearchRequest::new("parse");
        assert_eq!(request.query, "parse");
        assert_eq!((request.k, request.offset), (5, 0));
        assert_eq!(request.max_chars, 8000);
        assert!(request.repos.is_empty() && request.languages.is_empty() && request.path.is_none());
    }

    fn location() -> GitLocation {
        GitLocation::parse("https://github.com/o/r").unwrap()
    }

    fn ready_source() -> Source {
        let id = SourceId::from_location(&GitLocation::parse("https://github.com/o/r").unwrap())
            .unwrap();
        let mut source = Source::register(id, "r", location());
        source.mark_ready(
            CommitId::parse(&"ab".repeat(20)).unwrap(),
            IndexedAt::from_epoch_seconds(1_700_000_000),
        );
        source
    }

    #[test]
    fn source_summary_reports_commit_time_and_age() {
        let now = IndexedAt::from_epoch_seconds(1_700_000_090);
        let summary = SourceSummary::new(&ready_source(), 12, now);
        assert_eq!(summary.repo, "github.com/o/r");
        assert_eq!(summary.name, "r");
        assert_eq!(summary.status, "ready");
        assert_eq!(
            summary.commit.as_deref(),
            Some("abababa"),
            "the short commit"
        );
        assert_eq!(summary.indexed_at.as_deref(), Some("2023-11-14T22:13:20Z"));
        assert_eq!(summary.age_seconds, Some(90));
        assert_eq!(summary.chunks, 12);
    }

    #[test]
    fn a_failed_source_summary_carries_its_last_error() {
        let mut source = ready_source();
        source.record_failure(
            "network_or_auth",
            "git: operation timed out",
            IndexedAt::from_epoch_seconds(1_700_000_050),
        );
        let summary = SourceSummary::new(&source, 3, IndexedAt::from_epoch_seconds(1_700_000_060));
        let error = summary.last_error.as_ref().unwrap();
        assert_eq!(error.code, "network_or_auth");
        assert_eq!(error.message, "git: operation timed out");
        assert_eq!(error.at, "2023-11-14T22:14:10Z");
        assert_eq!(error.age_seconds, 10);
        let json = serde_json::to_string(&summary).unwrap();
        assert!(
            json.ends_with(
                r#""chunks":3,"last_error":{"code":"network_or_auth","message":"git: operation timed out","at":"2023-11-14T22:14:10Z","age_seconds":10}}"#
            ),
            "{json}"
        );
    }

    #[test]
    fn a_healthy_source_summary_has_no_last_error_field() {
        let summary = SourceSummary::new(
            &ready_source(),
            1,
            IndexedAt::from_epoch_seconds(1_700_000_000),
        );
        assert!(summary.last_error.is_none());
        assert!(!serde_json::to_string(&summary)
            .unwrap()
            .contains("last_error"));
    }

    #[test]
    fn source_summary_of_an_unindexed_source_has_no_commit_or_age() {
        let id = SourceId::from_location(&GitLocation::parse("https://github.com/o/r").unwrap())
            .unwrap();
        let source = Source::register(id, "r", location());
        let summary = SourceSummary::new(&source, 0, IndexedAt::from_epoch_seconds(5));
        assert_eq!(summary.status, "cloning");
        assert!(
            summary.commit.is_none()
                && summary.indexed_at.is_none()
                && summary.age_seconds.is_none()
        );
    }

    #[test]
    fn a_failed_update_still_reports_the_commit_being_served() {
        let mut source = ready_source();
        source.mark_failed();
        let summary = SourceSummary::new(&source, 3, IndexedAt::from_epoch_seconds(1_700_000_010));
        assert_eq!(summary.status, "failed");
        assert_eq!(summary.commit.as_deref(), Some("abababa"));
        assert_eq!(summary.age_seconds, Some(10));
    }
    #[test]
    fn error_response_carries_code_message_and_hint() {
        let response = ErrorResponse::from_error(&AppError::Storage(StorageError::IndexMissing));
        assert_eq!(response.error.code, "index_missing");
        assert_eq!(response.error.message, "no index exists yet");
        assert!(response
            .error
            .hint
            .as_deref()
            .unwrap()
            .starts_with("ask the user to run `rag add"));
        assert_eq!(response.error.details, None);
        assert_eq!(
            ErrorResponse::exit_code(&AppError::Storage(StorageError::IndexMissing)),
            5
        );
    }

    #[test]
    fn error_response_serializes_as_a_single_error_object() {
        let response = ErrorResponse::from_error(&AppError::Usage("bad flag".into()));
        assert_eq!(
            serde_json::to_string(&response).unwrap(),
            r#"{"error":{"code":"usage","message":"bad flag","hint":null}}"#
        );
    }

    #[test]
    fn an_unknown_source_error_lists_the_valid_names_in_details() {
        let error = AppError::UnknownSource(crate::application::error::UnknownSource::new(
            "nope",
            ["docs".to_owned(), "other".to_owned()],
        ));
        assert_eq!(
            serde_json::to_string(&ErrorResponse::from_error(&error)).unwrap(),
            concat!(
                r#"{"error":{"code":"not_found","message":"no source named \"nope\"; run `rag list` to see the registered ones (valid: docs, other)","#,
                r#""hint":"use one of the names in details.valid_names, or run `rag list`","#,
                r#""details":{"valid_names":["docs","other"]}}}"#
            )
        );
    }

    #[test]
    fn a_reindex_required_error_names_its_source_in_details() {
        let error = AppError::StaleIndex {
            source: "docs".into(),
            also_stale: 0,
        };
        let json = serde_json::to_string(&ErrorResponse::from_error(&error)).unwrap();
        assert!(
            json.ends_with(
                r#""hint":"ask the user to run `rag reindex docs`","details":{"source":"docs"}}}"#
            ),
            "{json}"
        );
    }

    #[test]
    fn the_hint_is_always_present_as_a_string_or_null() {
        for error in [
            AppError::Usage("x".into()),
            AppError::NotFound("x".into()),
            AppError::Storage(StorageError::IndexMissing),
        ] {
            let json = serde_json::to_string(&ErrorResponse::from_error(&error)).unwrap();
            assert!(json.contains(r#""hint":"#), "{json}");
            assert!(!json.contains("details"), "{json}");
        }
    }

    #[test]
    fn list_response_serializes_sources() {
        let summary = SourceSummary::new(
            &ready_source(),
            1,
            IndexedAt::from_epoch_seconds(1_700_000_000),
        );
        let json = serde_json::to_string(&ListResponse {
            schema_version: SCHEMA_VERSION,
            sources: vec![summary],
        })
        .unwrap();
        assert_eq!(
            json,
            concat!(
                r#"{"schema_version":1,"sources":[{"name":"r","repo":"github.com/o/r","#,
                r#""status":"ready","commit":"abababa","#,
                r#""indexed_at":"2023-11-14T22:13:20Z","age_seconds":0,"chunks":1}]}"#
            ),
            "no clone path: clones have no working tree to browse"
        );
    }
}
