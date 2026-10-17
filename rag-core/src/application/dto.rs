//! Request and response DTOs. Responses are exactly what the CLI prints as JSON
//! (schema v1, see `docs/plan/03-output-schema.md`); field order is the order in
//! the schema. Domain types never derive `Serialize`: these structs are the
//! mapping target.

use serde::Serialize;

use crate::application::error::{AppError, ErrorCode};
use crate::domain::chunk_id::ChunkId;
use crate::domain::indexed_at::IndexedAt;
use crate::domain::source::Source;

/// Output schema version, frozen at 1.
pub const SCHEMA_VERSION: u32 = 1;
/// Default number of hits (`-k`).
pub const DEFAULT_K: usize = 5;
/// Default hard cap on the characters of one search response.
pub const DEFAULT_MAX_CHARS: usize = 8000;

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
    pub id: String,
    pub rank: usize,
    pub score: f64,
    pub score_rel: f64,
    /// The source slug, `host/owner/name`.
    pub repo: String,
    pub commit: String,
    pub indexed_at: String,
    pub path: String,
    /// First and last line of the snippet, 1-based.
    pub lines: [usize; 2],
    pub language: String,
    pub heading: Option<String>,
    pub matched_terms: Vec<String>,
    pub snippet: String,
    /// The command that shows this chunk: `rag show --chunk <chunk_id>` (see
    /// `show_chunk_command`).
    pub show: String,
}

/// Output of the search use case (schema v1).
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct SearchResponse {
    pub schema_version: u32,
    pub query: String,
    /// `and` or `or_fallback`.
    pub mode: String,
    /// Chunks that matched the query before the per-file cap and before paging
    /// (`Retrieval::total_matches`).
    pub total_hits: usize,
    /// Number of entries in `results`.
    pub returned: usize,
    /// The requested offset: hits skipped after the per-file cap.
    pub offset: usize,
    /// Whether more hits follow this page (`Retrieval::has_more`); also true when
    /// the output budget cut hits that were part of the page.
    pub has_more: bool,
    /// The output character budget dropped hits or shortened snippets.
    pub truncated: bool,
    pub results: Vec<SearchResultDto>,
    /// Present when results are empty or weak.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hint: Option<String>,
}

/// `rag show --chunk <chunk_id>`, for the `show` field of a hit. The id is hex
/// only, so the command is safe to paste into any shell.
pub fn show_chunk_command(chunk: &ChunkId) -> String {
    format!("rag show --chunk {}", chunk.as_str())
}

/// One registered source in `rag list`. Clones have no working tree, so there is no
/// clone path to show.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct SourceSummary {
    pub repo: String,
    pub name: String,
    pub status: String,
    pub commit: Option<String>,
    pub indexed_at: Option<String>,
    pub age_seconds: Option<u64>,
    pub chunks: usize,
}

impl SourceSummary {
    /// Summarises `source` as of `now`.
    pub fn new(source: &Source, chunks: usize, now: IndexedAt) -> Self {
        let indexed = source.indexed();
        Self {
            repo: source.id().slug().to_owned(),
            name: source.name().to_owned(),
            status: source.status().as_str().to_owned(),
            commit: indexed.map(|revision| revision.commit.to_string()),
            indexed_at: indexed.map(|revision| revision.at.to_rfc3339()),
            age_seconds: indexed.map(|revision| revision.at.age_seconds(now)),
            chunks,
        }
    }
}

/// Output of the list use case.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ListResponse {
    pub schema_version: u32,
    pub sources: Vec<SourceSummary>,
}

/// The `error` object of a failed command.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ErrorBody {
    pub code: String,
    pub message: String,
    pub hint: Option<String>,
}

/// Output of any failed command in JSON mode: `{"error":{"code","message","hint"}}`.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ErrorResponse {
    pub error: ErrorBody,
}

impl ErrorResponse {
    pub fn from_error(error: &AppError) -> Self {
        let mut body = ErrorBody::from(error.code());
        body.message = error.to_string();
        Self { error: body }
    }

    /// The process exit code that goes with this error.
    pub fn exit_code(error: &AppError) -> u8 {
        error.code().exit_code()
    }
}

impl From<ErrorCode> for ErrorBody {
    fn from(code: ErrorCode) -> Self {
        Self {
            code: code.as_str().to_owned(),
            message: String::new(),
            hint: code.default_hint().map(str::to_owned),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::application::error::{AppError, StorageError};
    use crate::domain::chunk_id::ChunkId;
    use crate::domain::git_location::GitLocation;
    use crate::domain::oid::CommitId;
    use crate::domain::source_id::SourceId;

    const CHUNK_HEX: &str = "abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789";

    fn result_dto() -> SearchResultDto {
        SearchResultDto {
            id: CHUNK_HEX.into(),
            rank: 1,
            score: 2.5,
            score_rel: 1.0,
            repo: "github.com/o/r".into(),
            commit: "abc1234".into(),
            indexed_at: "2023-11-14T22:13:20Z".into(),
            path: "src/x.rs".into(),
            lines: [10, 22],
            language: "rust".into(),
            heading: None,
            matched_terms: vec!["parse".into()],
            snippet: "fn parse()".into(),
            show: format!("rag show --chunk {CHUNK_HEX}"),
        }
    }

    #[test]
    fn search_response_serializes_in_schema_v1_order() {
        let response = SearchResponse {
            schema_version: SCHEMA_VERSION,
            query: "parse".into(),
            mode: "and".into(),
            total_hits: 1,
            returned: 1,
            offset: 0,
            has_more: false,
            truncated: false,
            results: vec![result_dto()],
            hint: None,
        };
        let expected = concat!(
            r#"{"schema_version":1,"query":"parse","mode":"and","total_hits":1,"#,
            r#""returned":1,"offset":0,"has_more":false,"truncated":false,"results":["#,
            r#"{"id":"<ID>","rank":1,"score":2.5,"score_rel":1.0,"repo":"github.com/o/r","#,
            r#""commit":"abc1234","indexed_at":"2023-11-14T22:13:20Z","path":"src/x.rs","#,
            r#""lines":[10,22],"language":"rust","heading":null,"matched_terms":["parse"],"#,
            r#""snippet":"fn parse()","show":"rag show --chunk <ID>"}]}"#
        )
        .replace("<ID>", CHUNK_HEX);
        assert_eq!(serde_json::to_string(&response).unwrap(), expected);
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
    fn show_command_names_the_chunk_by_its_hex_id() {
        let id = ChunkId::parse(CHUNK_HEX).unwrap();
        assert_eq!(
            show_chunk_command(&id),
            format!("rag show --chunk {CHUNK_HEX}")
        );
        assert!(
            CHUNK_HEX.bytes().all(|b| b.is_ascii_hexdigit()),
            "hex only, so the command is shell-safe"
        );
    }

    #[test]
    fn pagination_fields_are_the_values_retrieval_derives() {
        // total_hits = Retrieval::total_matches (matching chunks before the per-file
        // cap), has_more = Retrieval::has_more, offset = the requested offset.
        let response = SearchResponse {
            schema_version: SCHEMA_VERSION,
            query: "parse".into(),
            mode: "and".into(),
            total_hits: 42,
            returned: 5,
            offset: 10,
            has_more: true,
            truncated: false,
            results: vec![],
            hint: None,
        };
        let json = serde_json::to_string(&response).unwrap();
        assert!(
            json.contains(r#""total_hits":42,"returned":5,"offset":10,"has_more":true"#),
            "{json}"
        );
    }

    #[test]
    fn hint_is_serialized_last_when_present() {
        let response = SearchResponse {
            schema_version: SCHEMA_VERSION,
            query: "zzz".into(),
            mode: "or_fallback".into(),
            total_hits: 0,
            returned: 0,
            offset: 0,
            has_more: false,
            truncated: false,
            results: vec![],
            hint: Some("try fewer words".into()),
        };
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
        assert_eq!(summary.status, "ready");
        assert_eq!(summary.commit.as_deref(), Some(&*"ab".repeat(20)));
        assert_eq!(summary.indexed_at.as_deref(), Some("2023-11-14T22:13:20Z"));
        assert_eq!(summary.age_seconds, Some(90));
        assert_eq!(summary.chunks, 12);
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
        assert_eq!(summary.commit.as_deref(), Some(&*"ab".repeat(20)));
        assert_eq!(summary.age_seconds, Some(10));
    }
    #[test]
    fn error_response_carries_code_message_and_hint() {
        let response = ErrorResponse::from_error(&AppError::Storage(StorageError::IndexMissing));
        assert_eq!(response.error.code, "index_missing");
        assert_eq!(response.error.message, "no index exists yet");
        assert!(response.error.hint.as_deref().unwrap().contains("rag add"));
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
                r#"{"schema_version":1,"sources":[{"repo":"github.com/o/r","name":"r","#,
                r#""status":"ready","commit":"abababababababababababababababababababab","#,
                r#""indexed_at":"2023-11-14T22:13:20Z","age_seconds":0,"chunks":1}]}"#
            ),
            "no clone path: clones have no working tree to browse"
        );
    }
}
