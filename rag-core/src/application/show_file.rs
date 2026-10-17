//! The show use case. Two ways in:
//!
//! - `execute_chunk`: the stored chunk with an id, as `rag search` printed it.
//! - `execute`: `<repo> <path> [--lines S-E] [--around N]`. The path is resolved
//!   through the manifest (the indexed files, never `ls_tree`) and read as a blob at
//!   the indexed commit, so it serves exactly what was indexed even while an update
//!   runs. The bytes pass `IndexPolicy::check_content` again, and the output is
//!   capped in lines and characters (`truncated` says so).

use crate::application::dto::{
    ShowResponse, SCHEMA_VERSION, SHOW_AROUND_RADIUS, SHOW_MAX_CHARS, SHOW_MAX_LINES,
};
use crate::application::error::{AppError, Scrubbed};
use crate::application::ports::{ChunkStore, GitClient, SourceRepository};
use crate::application::search::{find_sources, unknown_source};
use crate::domain::chunk_id::ChunkId;
use crate::domain::index_policy::IndexPolicy;
use crate::domain::language::Language;
use crate::domain::repo_path::RepoPath;
use crate::domain::source::{IndexedRevision, Source};

/// Input of `ShowFile::execute`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShowFileRequest {
    /// Source name or slug.
    pub repo: String,
    pub path: String,
    /// `--lines S-E`, inclusive and 1-based.
    pub lines: Option<(usize, usize)>,
    /// `--around N`: the lines near line N.
    pub around: Option<usize>,
}

/// `rag show`.
pub struct ShowFile<'a> {
    sources: &'a dyn SourceRepository,
    chunks: &'a dyn ChunkStore,
    git: &'a dyn GitClient,
}

impl<'a> ShowFile<'a> {
    pub fn new(
        sources: &'a dyn SourceRepository,
        chunks: &'a dyn ChunkStore,
        git: &'a dyn GitClient,
    ) -> Self {
        Self {
            sources,
            chunks,
            git,
        }
    }

    /// A slice of an indexed file, read at the indexed commit.
    pub fn execute(&self, request: &ShowFileRequest) -> Result<ShowResponse, AppError> {
        let source = self.source(&request.repo)?;
        let revision = revision_of(&source)?;
        let path = RepoPath::parse(&request.path)
            .map_err(|error| AppError::Usage(Scrubbed::from(error.to_string())))?;
        let Some(blob) = self.chunks.manifest(source.id())?.get(&path).cloned() else {
            return Err(AppError::NotFound(Scrubbed::from(format!(
                "{path} is not indexed in {}; it may be new, skipped by the index rules, or removed",
                source.name()
            ))));
        };
        let mut blobs = self
            .git
            .read_blobs(source.id(), std::slice::from_ref(&blob))?;
        let bytes = blobs.remove(&blob).ok_or_else(|| {
            AppError::NotFound(Scrubbed::from(format!("the content of {path} is missing")))
        })?;
        IndexPolicy::check_content(&path, &bytes).map_err(|reason| {
            AppError::NotFound(Scrubbed::from(format!(
                "{path} is not shown: {}",
                reason.as_str()
            )))
        })?;
        let text = String::from_utf8_lossy(&bytes);
        let lines: Vec<&str> = text.lines().collect();
        let (first, last) = select(lines.len(), request)?;
        let clipped = clip(&lines[first.saturating_sub(1)..last], first.max(1));
        Ok(ShowResponse {
            schema_version: SCHEMA_VERSION,
            repo: source.id().slug().to_owned(),
            commit: revision.commit.short().to_owned(),
            indexed_at: revision.at.to_rfc3339(),
            path: path.as_str().to_owned(),
            language: Language::from_path(&path).name().to_owned(),
            lines: clipped.lines,
            total_lines: Some(lines.len()),
            truncated: clipped.truncated,
            chunk: None,
            text: clipped.text,
        })
    }

    /// The stored chunk with this id.
    pub fn execute_chunk(&self, id: &str) -> Result<ShowResponse, AppError> {
        let id = ChunkId::parse(id).ok_or_else(|| {
            AppError::Usage(Scrubbed::from(
                "a chunk id is 64 hexadecimal characters, as printed by `rag search`",
            ))
        })?;
        let gone = || {
            AppError::NotFound(Scrubbed::from(
                "that chunk is not in the index; an update may have replaced it, so run `rag search` again",
            ))
        };
        let chunk = self.chunks.chunk(&id)?.ok_or_else(gone)?;
        let source = self.sources.get(&chunk.source)?.ok_or_else(gone)?;
        let revision = revision_of(&source)?;
        let lines: Vec<&str> = chunk.text.lines().collect();
        let clipped = clip(&lines, chunk.lines.start());
        Ok(ShowResponse {
            schema_version: SCHEMA_VERSION,
            repo: chunk.source.slug().to_owned(),
            commit: revision.commit.short().to_owned(),
            indexed_at: revision.at.to_rfc3339(),
            path: chunk.path.as_str().to_owned(),
            language: chunk.language.name().to_owned(),
            lines: clipped.lines,
            total_lines: None,
            truncated: clipped.truncated,
            chunk: Some(chunk.id.as_str().to_owned()),
            text: clipped.text,
        })
    }

    fn source(&self, wanted: &str) -> Result<Source, AppError> {
        let mut found = find_sources(self.sources, wanted)?;
        match found.len() {
            0 => Err(unknown_source(wanted)),
            1 => Ok(found.remove(0)),
            _ => Err(AppError::Usage(Scrubbed::from(format!(
                "{wanted:?} names several sources; use the source name from `rag list`"
            )))),
        }
    }
}

fn revision_of(source: &Source) -> Result<&IndexedRevision, AppError> {
    source.indexed().ok_or_else(|| {
        AppError::NotFound(Scrubbed::from(format!(
            "{} has no indexed commit yet",
            source.name()
        )))
    })
}

/// The first and last line (1-based, inclusive) the request selects in a file of
/// `total` lines, before the output caps. `(1, 0)` for an empty file.
fn select(total: usize, request: &ShowFileRequest) -> Result<(usize, usize), AppError> {
    let usage = |message: String| AppError::Usage(Scrubbed::from(message));
    match (request.lines, request.around) {
        (Some(_), Some(_)) => Err(usage("use --lines or --around, not both".to_owned())),
        (Some((start, end)), None) => {
            if start == 0 || start > end {
                return Err(usage(format!(
                    "--lines {start}-{end} is not a range: lines are 1-based and the start may not pass the end"
                )));
            }
            if start > total {
                return Err(usage(format!("the file has only {total} lines")));
            }
            Ok((start, end.min(total)))
        }
        (None, Some(line)) => {
            if line == 0 || line > total {
                return Err(usage(format!(
                    "--around {line} is outside the file, which has {total} lines"
                )));
            }
            Ok((
                line.saturating_sub(SHOW_AROUND_RADIUS).max(1),
                (line + SHOW_AROUND_RADIUS).min(total),
            ))
        }
        (None, None) => Ok((1, total)),
    }
}

/// Text shown, with where it starts and ends and whether a cap cut it.
struct Clipped {
    text: String,
    lines: [usize; 2],
    truncated: bool,
}

/// Joins `lines` (the first of which is file line `first_line`) under the line and
/// character caps. Whole lines are kept; a first line longer than the character cap
/// is cut at a character boundary.
fn clip(lines: &[&str], first_line: usize) -> Clipped {
    if lines.is_empty() {
        return Clipped {
            text: String::new(),
            lines: [0, 0],
            truncated: false,
        };
    }
    let mut text = String::new();
    let mut chars = 0;
    let mut shown = 0;
    for line in lines.iter().take(SHOW_MAX_LINES) {
        let width = line.chars().count() + usize::from(shown > 0);
        if chars + width > SHOW_MAX_CHARS {
            break;
        }
        if shown > 0 {
            text.push('\n');
        }
        text.push_str(line);
        chars += width;
        shown += 1;
    }
    if shown == 0 {
        text = lines[0].chars().take(SHOW_MAX_CHARS).collect();
        shown = 1;
    }
    let cut_inside_a_line = shown == 1 && lines[0].chars().count() > SHOW_MAX_CHARS;
    Clipped {
        text,
        lines: [first_line, first_line + shown - 1],
        truncated: shown < lines.len() || cut_inside_a_line,
    }
}
#[cfg(test)]
mod tests {
    use super::{ShowFile, ShowFileRequest};
    use crate::application::dto::{
        ShowResponse, SHOW_AROUND_RADIUS, SHOW_MAX_CHARS, SHOW_MAX_LINES,
    };
    use crate::application::error::{AppError, ErrorCode};
    use crate::application::ports::{
        ChunkStore, GitClient, IndexChanges, IndexedFile, SourceRepository, MAX_BLOB_BYTES,
    };
    use crate::application::search::fixtures::INDEXED_AT;
    use crate::application::testkit::{FakeGitClient, InMemoryStore};
    use crate::domain::chunker::Chunker;
    use crate::domain::git_location::GitLocation;
    use crate::domain::git_ref::GitRef;
    use crate::domain::indexed_at::IndexedAt;
    use crate::domain::oid::CommitId;
    use crate::domain::repo_path::RepoPath;
    use crate::domain::source::Source;
    use crate::domain::source_id::SourceId;

    const URL: &str = "https://github.com/o/r";

    /// A remote with `files` committed and cloned, and a store that indexed the
    /// paths in `indexed` (all of them when `None`) at that commit.
    struct World {
        store: InMemoryStore,
        git: FakeGitClient,
        id: SourceId,
        commit: CommitId,
    }

    impl World {
        fn new(files: &[(&str, &str)]) -> Self {
            Self::with_indexed(files, None)
        }

        fn with_indexed(files: &[(&str, &str)], indexed: Option<&[&str]>) -> Self {
            let location = GitLocation::parse(URL).unwrap();
            let id = SourceId::from_location(&location).unwrap();
            let git = FakeGitClient::new();
            git.add_remote(URL, "main");
            let commit = git.commit(URL, files);
            git.clone_shallow(&id, &location, &GitRef::parse("main").unwrap())
                .unwrap();
            let store = InMemoryStore::new();
            let mut source = Source::register(id.clone(), "docs", location);
            store.save(&source).unwrap();
            source.mark_ready(commit.clone(), IndexedAt::from_epoch_seconds(INDEXED_AT));
            let listing = git.ls_tree(&id, source.indexed().map(|r| &r.commit).unwrap());
            let upserts = listing
                .unwrap()
                .entries
                .into_iter()
                .filter(|entry| indexed.is_none_or(|only| only.contains(&entry.path.as_str())))
                .map(|entry| {
                    let text = files
                        .iter()
                        .find(|(path, _)| *path == entry.path.as_str())
                        .map(|(_, text)| *text)
                        .unwrap();
                    IndexedFile {
                        chunks: Chunker::chunk(&id, &entry.path, text),
                        blob: entry.blob,
                        size: entry.size,
                        path: entry.path,
                    }
                })
                .collect();
            store
                .commit_index(
                    &source,
                    None,
                    &IndexChanges {
                        upserts,
                        removals: Vec::new(),
                    },
                )
                .unwrap();
            Self {
                store,
                git,
                id,
                commit,
            }
        }

        fn show(&self, request: &ShowFileRequest) -> Result<ShowResponse, AppError> {
            ShowFile::new(&self.store, &self.store, &self.git).execute(request)
        }

        fn show_chunk(&self, id: &str) -> Result<ShowResponse, AppError> {
            ShowFile::new(&self.store, &self.store, &self.git).execute_chunk(id)
        }
    }

    fn request(repo: &str, path: &str) -> ShowFileRequest {
        ShowFileRequest {
            repo: repo.to_owned(),
            path: path.to_owned(),
            lines: None,
            around: None,
        }
    }

    fn numbered(count: usize) -> String {
        (1..=count)
            .map(|i| format!("line {i}"))
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn a_small_file_is_shown_whole_with_its_commit() {
        let world = World::new(&[("src/a.rs", "fn a() {}\nfn b() {}\n")]);
        let shown = world.show(&request("docs", "src/a.rs")).unwrap();
        assert_eq!(shown.repo, "github.com/o/r");
        assert_eq!(shown.path, "src/a.rs");
        assert_eq!(shown.language, "rust");
        assert_eq!(shown.commit, world.commit.short());
        assert_eq!(shown.indexed_at, "2023-11-14T22:13:20Z");
        assert_eq!(shown.lines, [1, 2]);
        assert_eq!(shown.total_lines, Some(2));
        assert!(!shown.truncated && shown.chunk.is_none());
        assert_eq!(shown.text, "fn a() {}\nfn b() {}");
    }

    #[test]
    fn the_repo_resolves_by_name_or_slug() {
        let world = World::new(&[("a.rs", "x")]);
        for repo in ["docs", "DOCS", "github.com/o/r"] {
            assert!(world.show(&request(repo, "a.rs")).is_ok(), "{repo}");
        }
        let error = world.show(&request("nope", "a.rs")).unwrap_err();
        assert_eq!(error.code(), ErrorCode::NotFound);
    }

    #[test]
    fn lines_selects_an_inclusive_range() {
        let world = World::new(&[("a.txt", &numbered(10))]);
        let mut req = request("docs", "a.txt");
        req.lines = Some((3, 5));
        let shown = world.show(&req).unwrap();
        assert_eq!(shown.lines, [3, 5]);
        assert_eq!(shown.text, "line 3\nline 4\nline 5");
        assert_eq!(shown.total_lines, Some(10));
    }

    #[test]
    fn a_range_past_the_end_is_clamped_to_the_last_line() {
        let world = World::new(&[("a.txt", &numbered(10))]);
        let mut req = request("docs", "a.txt");
        req.lines = Some((8, 99));
        let shown = world.show(&req).unwrap();
        assert_eq!(shown.lines, [8, 10]);
        assert!(!shown.truncated);
    }

    #[test]
    fn around_shows_a_window_centred_on_the_line() {
        let world = World::new(&[("a.txt", &numbered(100))]);
        let mut req = request("docs", "a.txt");
        req.around = Some(50);
        let shown = world.show(&req).unwrap();
        assert_eq!(
            shown.lines,
            [50 - SHOW_AROUND_RADIUS, 50 + SHOW_AROUND_RADIUS]
        );
        req.around = Some(2);
        assert_eq!(world.show(&req).unwrap().lines[0], 1, "clamped at the top");
        req.around = Some(100);
        assert_eq!(
            world.show(&req).unwrap().lines[1],
            100,
            "clamped at the end"
        );
    }

    #[test]
    fn invalid_line_selections_are_usage_errors() {
        let world = World::new(&[("a.txt", &numbered(10))]);
        let usage = |lines: Option<(usize, usize)>, around: Option<usize>| {
            let mut req = request("docs", "a.txt");
            req.lines = lines;
            req.around = around;
            world.show(&req).unwrap_err()
        };
        for error in [
            usage(Some((0, 3)), None),
            usage(Some((5, 3)), None),
            usage(Some((11, 12)), None),
            usage(Some((1, 2)), Some(3)),
            usage(None, Some(0)),
            usage(None, Some(11)),
        ] {
            assert_eq!(error.code(), ErrorCode::Usage, "{error}");
        }
    }

    #[test]
    fn an_empty_file_has_no_lines() {
        let world = World::new(&[("empty.txt", "")]);
        let shown = world.show(&request("docs", "empty.txt")).unwrap();
        assert_eq!((shown.lines, shown.total_lines), ([0, 0], Some(0)));
        assert_eq!(shown.text, "");
    }

    #[test]
    fn crlf_files_count_one_line_per_line() {
        let world = World::new(&[("a.txt", "one\r\ntwo\r\nthree")]);
        let mut req = request("docs", "a.txt");
        req.lines = Some((2, 3));
        let shown = world.show(&req).unwrap();
        assert_eq!(shown.text, "two\nthree");
        assert_eq!(shown.total_lines, Some(3));
    }

    #[test]
    fn long_files_are_cut_at_the_line_cap() {
        let world = World::new(&[("a.txt", &numbered(SHOW_MAX_LINES + 50))]);
        let shown = world.show(&request("docs", "a.txt")).unwrap();
        assert!(shown.truncated);
        assert_eq!(shown.lines, [1, SHOW_MAX_LINES]);
        assert_eq!(shown.total_lines, Some(SHOW_MAX_LINES + 50));
    }

    #[test]
    fn wide_files_are_cut_at_the_character_cap_on_a_line_boundary() {
        let line = "é".repeat(99);
        let text = vec![line.as_str(); 150].join("\n");
        let world = World::new(&[("a.txt", &text)]);
        let shown = world.show(&request("docs", "a.txt")).unwrap();
        assert!(shown.truncated);
        assert!(shown.text.chars().count() <= SHOW_MAX_CHARS);
        let shown_lines = shown.text.split('\n').count();
        assert_eq!(shown.lines, [1, shown_lines]);
        assert!(
            shown.text.split('\n').all(|l| l == line),
            "whole lines only"
        );
    }

    #[test]
    fn one_enormous_line_is_cut_without_splitting_a_character() {
        let world = World::new(&[("a.txt", &"日".repeat(SHOW_MAX_CHARS * 2))]);
        let shown = world.show(&request("docs", "a.txt")).unwrap();
        assert!(shown.truncated);
        assert_eq!(shown.text.chars().count(), SHOW_MAX_CHARS);
        assert_eq!(shown.lines, [1, 1]);
    }

    #[test]
    fn a_path_outside_the_manifest_is_not_found_even_if_git_has_it() {
        let world = World::with_indexed(&[("a.rs", "x"), ("skipped.rs", "y")], Some(&["a.rs"]));
        let error = world.show(&request("docs", "skipped.rs")).unwrap_err();
        assert_eq!(error.code(), ErrorCode::NotFound);
        assert!(error.to_string().contains("not indexed"), "{error}");
    }

    #[test]
    fn unsafe_paths_are_usage_errors() {
        let world = World::new(&[("a.rs", "x")]);
        for path in [
            "../a.rs",
            "/etc/passwd",
            "C:\\x",
            "a.rs:stream",
            ".git/config",
        ] {
            let error = world.show(&request("docs", path)).unwrap_err();
            assert_eq!(error.code(), ErrorCode::Usage, "{path}");
        }
    }

    #[test]
    fn backslash_paths_are_normalised_like_user_input() {
        let world = World::new(&[("src/a.rs", "x")]);
        assert!(world.show(&request("docs", "src\\a.rs")).is_ok());
    }

    #[test]
    fn content_the_policy_now_rejects_is_not_shown() {
        let key = "-----BEGIN RSA PRIVATE KEY-----\nabc\n-----END RSA PRIVATE KEY-----\n";
        let world = World::new(&[("k.txt", key)]);
        let error = world.show(&request("docs", "k.txt")).unwrap_err();
        assert_eq!(error.code(), ErrorCode::NotFound);
        assert!(error.to_string().contains("secret_content"), "{error}");
    }

    #[test]
    fn a_blob_over_the_read_cap_is_a_git_error_not_a_crash() {
        let big = "x".repeat(MAX_BLOB_BYTES as usize + 1);
        let world = World::new(&[("big.txt", &big)]);
        let error = world.show(&request("docs", "big.txt")).unwrap_err();
        assert_eq!(error.code(), ErrorCode::Generic);
        assert!(error.to_string().contains("too large"), "{error}");
    }

    #[test]
    fn a_blob_missing_from_the_clone_is_not_found() {
        let world = World::new(&[("a.rs", "x")]);
        world.git.remove_checkout(&world.id).unwrap();
        let error = world.show(&request("docs", "a.rs")).unwrap_err();
        assert_eq!(error.code(), ErrorCode::NotFound);
    }

    #[test]
    fn a_source_without_an_indexed_revision_is_not_found() {
        let world = World::new(&[("a.rs", "x")]);
        let location = GitLocation::parse("https://github.com/o/new").unwrap();
        let id = SourceId::from_location(&location).unwrap();
        world
            .store
            .save(&Source::register(id, "new", location))
            .unwrap();
        let error = world.show(&request("new", "a.rs")).unwrap_err();
        assert_eq!(error.code(), ErrorCode::NotFound);
    }

    fn first_chunk_id(world: &World, path: &str) -> String {
        let path = RepoPath::parse(path).unwrap();
        world
            .store
            .chunks(&world.id)
            .unwrap()
            .into_iter()
            .find(|chunk| chunk.path == path)
            .unwrap()
            .id
            .as_str()
            .to_owned()
    }

    #[test]
    fn a_chunk_is_shown_by_id_with_its_place_in_the_file() {
        let world = World::new(&[("docs/g.md", "# Guide\n\nHello there.\n")]);
        let id = first_chunk_id(&world, "docs/g.md");
        let shown = world.show_chunk(&id).unwrap();
        assert_eq!(shown.path, "docs/g.md");
        assert_eq!(shown.repo, "github.com/o/r");
        assert_eq!(shown.language, "markdown");
        assert_eq!(shown.chunk.as_deref(), Some(id.as_str()));
        assert_eq!(shown.total_lines, None);
        assert_eq!(shown.lines, [1, 3]);
        assert_eq!(shown.text, "# Guide\n\nHello there.");
        assert_eq!(shown.commit, world.commit.short());
    }

    #[test]
    fn a_vanished_chunk_tells_the_caller_to_search_again() {
        let world = World::new(&[("a.rs", "x")]);
        let error = world.show_chunk(&"ab".repeat(32)).unwrap_err();
        assert_eq!(error.code(), ErrorCode::NotFound);
        assert!(error.to_string().contains("rag search"), "{error}");
    }

    #[test]
    fn a_malformed_chunk_id_is_a_usage_error() {
        let world = World::new(&[("a.rs", "x")]);
        for id in ["", "xyz", "--flag", &"ab".repeat(3)] {
            assert_eq!(
                world.show_chunk(id).unwrap_err().code(),
                ErrorCode::Usage,
                "{id:?}"
            );
        }
    }
}
