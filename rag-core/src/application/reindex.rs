//! Reindex: rebuild an index made by older rules.

use serde::Serialize;

use crate::application::dto::{ErrorBody, SCHEMA_VERSION};
use crate::application::error::AppError;
use crate::application::index_pipeline::IndexReport;
use crate::application::indexer::{
    error_body, exit_code_of_failures, is_contention, resolve_source, Indexer, NO_SOURCES_MESSAGE,
};
use crate::application::ports::{ChunkStore, Clock, GitClient, OperationLock, SourceRepository};
use crate::application::progress::{Progress, NO_PROGRESS};
use crate::application::search::unknown_source;
use crate::domain::source::Source;
use crate::domain::source_id::SourceId;

/// Input of the reindex use case.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ReindexRequest {
    /// Source name or slug; `None` reindexes every source.
    pub name: Option<String>,
}

impl ReindexRequest {
    pub fn all() -> Self {
        Self::default()
    }

    pub fn named(name: &str) -> Self {
        Self {
            name: Some(name.to_owned()),
        }
    }
}

/// The outcome for one source.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ReindexEntry {
    pub repo: String,
    pub name: String,
    /// `up_to_date`, `not_indexed` (nothing to rebuild yet), `tokenizer` (full-text
    /// fields rewritten), `full` (re-read and re-chunked at the indexed commit),
    /// `refetched` (the old objects were gone, so the branch was fetched again),
    /// `failed`, `skipped_busy` (another process was working on the source), or
    /// `skipped_removed` (the source was removed while the command ran). The two
    /// skipped results are not failures.
    pub result: String,
    pub chunks: usize,
    pub index: Option<IndexReport>,
    pub error: Option<ErrorBody>,
}

/// Output of the reindex use case.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ReindexResponse {
    pub schema_version: u32,
    pub sources: Vec<ReindexEntry>,
    /// How many sources failed; sources skipped as busy or removed are not counted.
    pub failed: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
}

impl ReindexResponse {
    /// The process exit code: 0 unless a source failed; the failures' shared error
    /// code when they all have one, else 1 (see `exit_code_of_failures`).
    pub fn exit_code(&self) -> u8 {
        exit_code_of_failures(
            self.sources
                .iter()
                .filter(|entry| entry.result == "failed")
                .filter_map(|entry| entry.error.as_ref()),
        )
    }
}

pub struct Reindex<'a> {
    sources: &'a dyn SourceRepository,
    store: &'a dyn ChunkStore,
    git: &'a dyn GitClient,
    locks: &'a dyn OperationLock,
    clock: &'a dyn Clock,
    progress: &'a dyn Progress,
}

impl<'a> Reindex<'a> {
    pub fn new(
        sources: &'a dyn SourceRepository,
        store: &'a dyn ChunkStore,
        git: &'a dyn GitClient,
        locks: &'a dyn OperationLock,
        clock: &'a dyn Clock,
    ) -> Self {
        Self {
            sources,
            store,
            git,
            locks,
            clock,
            progress: &NO_PROGRESS,
        }
    }

    /// Reports the phases of the rebuild to `progress`.
    pub fn with_progress(mut self, progress: &'a dyn Progress) -> Self {
        self.progress = progress;
        self
    }

    pub fn execute(&self, request: &ReindexRequest) -> Result<ReindexResponse, AppError> {
        if let Some(name) = &request.name {
            let source = resolve_source(self.sources, name)?;
            let entry = self
                .reindex_one(source.id())?
                .ok_or_else(|| unknown_source(self.sources, name))?;
            return Ok(Self::response(vec![entry], None));
        }
        let all = self.sources.list()?;
        if all.is_empty() {
            return Ok(Self::response(Vec::new(), Some(NO_SOURCES_MESSAGE)));
        }
        let entries = all
            .iter()
            .map(|source| match self.reindex_one(source.id()) {
                Ok(Some(entry)) => entry,
                Ok(None) => Self::unfinished_entry(
                    source,
                    "skipped_removed",
                    &AppError::NotFound("the source was removed while the command ran".into()),
                ),
                Err(error) if is_contention(&error) => {
                    Self::unfinished_entry(source, "skipped_busy", &error)
                }
                Err(error) => Self::unfinished_entry(source, "failed", &error),
            })
            .collect();
        Ok(Self::response(entries, None))
    }

    fn response(sources: Vec<ReindexEntry>, message: Option<&str>) -> ReindexResponse {
        ReindexResponse {
            schema_version: SCHEMA_VERSION,
            failed: sources
                .iter()
                .filter(|entry| entry.result == "failed")
                .count(),
            sources,
            message: message.map(str::to_owned),
        }
    }

    /// A source the reindex did not finish: `failed`, or left alone (`skipped_*`,
    /// not a failure). `error` says why.
    fn unfinished_entry(source: &Source, result: &str, error: &AppError) -> ReindexEntry {
        ReindexEntry {
            repo: source.id().slug().to_owned(),
            name: source.name().to_owned(),
            result: result.to_owned(),
            chunks: 0,
            index: None,
            error: Some(error_body(error)),
        }
    }

    /// Reindexes one source under its operation lock. `None` when the source was
    /// removed after it was listed.
    fn reindex_one(&self, id: &SourceId) -> Result<Option<ReindexEntry>, AppError> {
        let _lock = self.locks.acquire(id)?;
        let Some(source) = self.sources.get(id)? else {
            return Ok(None);
        };
        let indexer = Indexer::new(self.sources, self.store, self.git, self.clock)
            .with_progress(self.progress);
        let rebuilt = indexer.rebuild(source)?;
        Ok(Some(ReindexEntry {
            repo: rebuilt.source.id().slug().to_owned(),
            name: rebuilt.source.name().to_owned(),
            result: rebuilt.mode.as_str().to_owned(),
            chunks: rebuilt.chunks,
            index: rebuilt.report,
            error: None,
        }))
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    use crate::application::error::{ErrorCode, GitError};
    use crate::application::indexer_test_support::{GitSpy, SpiedStore, World};
    use crate::application::testkit::StampStoredVersions;
    use crate::domain::index_versions::IndexVersions;
    use crate::domain::oid::CommitId;

    const NAME: &str = "example.com/o/r";

    fn indexed_world() -> (World, CommitId) {
        let w = World::new();
        let head = w.commit(&[
            ("src/lib.rs", "pub fn parse_config() {}\n"),
            ("README.md", "# Title\nbody\n"),
        ]);
        w.add().unwrap();
        (w, head)
    }

    fn stored_versions(w: &World) -> IndexVersions {
        w.store.stored_versions(&w.id()).unwrap().unwrap()
    }

    #[test]
    fn reindexing_with_an_empty_registry_says_so_without_failing() {
        let w = World::new();
        let response = w.reindex().execute(&ReindexRequest::all()).unwrap();
        assert!(response.sources.is_empty());
        assert!(response.message.unwrap().contains("rag add"));
    }

    #[test]
    fn reindexing_an_unknown_source_is_not_found() {
        let (w, _) = indexed_world();
        let error = w
            .reindex()
            .execute(&ReindexRequest::named("nope"))
            .unwrap_err();
        assert_eq!(error.code(), ErrorCode::NotFound);
    }

    #[test]
    fn a_current_index_is_up_to_date_and_untouched() {
        let (w, _) = indexed_world();
        let before = w.store.chunks(&w.id()).unwrap();
        let spy = GitSpy::new(&w.git);

        let response = w
            .reindex_via(&spy)
            .execute(&ReindexRequest::named(NAME))
            .unwrap();

        assert_eq!(response.sources[0].result, "up_to_date");
        assert_eq!(w.store.chunks(&w.id()).unwrap(), before);
        assert!(spy.calls().is_empty());
    }

    #[test]
    fn a_tokenizer_change_rewrites_the_fields_without_touching_git() {
        let (w, _) = indexed_world();
        let before = w.store.chunks(&w.id()).unwrap();
        let mut old = IndexVersions::current();
        old.tokenizer = 0;
        w.store.stamp_stored_versions(&w.id(), old);
        let spy = GitSpy::new(&w.git);

        let response = w
            .reindex_via(&spy)
            .execute(&ReindexRequest::named(NAME))
            .unwrap();

        assert_eq!(response.sources[0].result, "tokenizer");
        assert_eq!(response.sources[0].chunks, before.len());
        assert_eq!(stored_versions(&w), IndexVersions::current());
        assert_eq!(
            w.store.chunks(&w.id()).unwrap(),
            before,
            "chunk rows are untouched"
        );
        assert!(spy.calls().is_empty(), "works offline");
    }

    #[test]
    fn a_chunker_change_rereads_the_blobs_and_rechunks_every_file() {
        let (w, head) = indexed_world();
        let before = w.store.chunks(&w.id()).unwrap();
        let mut old = IndexVersions::current();
        old.chunker = 0;
        w.store.stamp_stored_versions(&w.id(), old);
        let spy = GitSpy::new(&w.git);

        let response = w
            .reindex_via(&spy)
            .execute(&ReindexRequest::named(NAME))
            .unwrap();

        assert_eq!(response.sources[0].result, "full");
        assert_eq!(stored_versions(&w), IndexVersions::current());
        assert_eq!(w.store.chunks(&w.id()).unwrap(), before);
        assert!(spy.called("read_blobs"));
        assert!(!spy.called("fetch_update"), "no network needed");
        let source = w.store.get(&w.id()).unwrap().unwrap();
        assert_eq!(
            source.indexed().unwrap().commit,
            head,
            "same commit, new rules"
        );
    }

    #[test]
    fn a_policy_change_reselects_files_and_reindexes_every_one() {
        let (w, _) = indexed_world();
        let mut old = IndexVersions::current();
        old.policy = 0;
        old.tokenizer = 0;
        w.store.stamp_stored_versions(&w.id(), old);

        let response = w.reindex().execute(&ReindexRequest::all()).unwrap();

        assert_eq!(response.sources[0].result, "full");
        assert_eq!(response.sources[0].index.as_ref().unwrap().files_added, 2);
        assert_eq!(stored_versions(&w), IndexVersions::current());
    }

    #[test]
    fn when_the_old_objects_are_gone_the_branch_is_fetched_and_fully_indexed() {
        let (w, first) = indexed_world();
        let second = w.commit(&[("src/lib.rs", "pub fn parse_config_v2() {}\n")]);
        let mut old = IndexVersions::current();
        old.chunker = 0;
        w.store.stamp_stored_versions(&w.id(), old);
        let spy = GitSpy::new(&w.git);
        spy.hide_commit(&first);

        let response = w
            .reindex_via(&spy)
            .execute(&ReindexRequest::named(NAME))
            .unwrap();

        assert_eq!(response.sources[0].result, "refetched");
        let source = w.store.get(&w.id()).unwrap().unwrap();
        assert_eq!(source.indexed().unwrap().commit, second);
        assert_eq!(stored_versions(&w), IndexVersions::current());
        let manifest = w.store.manifest(&w.id()).unwrap();
        assert_eq!(manifest.len(), 1, "the new commit has one file");
    }

    #[test]
    fn a_failed_full_reindex_keeps_the_stale_index_readable_for_a_retry() {
        let (w, _) = indexed_world();
        let before = w.store.chunks(&w.id()).unwrap();
        let mut old = IndexVersions::current();
        old.chunker = 0;
        w.store.stamp_stored_versions(&w.id(), old);
        w.store.fail_next_commit();

        assert!(w.reindex().execute(&ReindexRequest::named(NAME)).is_err());

        assert_eq!(w.store.chunks(&w.id()).unwrap(), before);
        assert_eq!(stored_versions(&w).chunker, 0);
        assert_eq!(
            w.reindex()
                .execute(&ReindexRequest::named(NAME))
                .unwrap()
                .sources[0]
                .result,
            "full"
        );
    }

    #[test]
    fn reindex_all_continues_past_a_failing_source() {
        let (w, _) = indexed_world();
        let other = "https://example.com/o/other";
        w.git.add_remote(other, "main");
        w.git.commit(other, &[("b.rs", "fn b() {}\n")]);
        w.add_source()
            .execute(&crate::application::add_source::AddRequest::new(other))
            .unwrap();
        let other_id = crate::application::indexer_test_support::id_of(other);
        let mut old = IndexVersions::current();
        old.chunker = 0;
        w.store.stamp_stored_versions(&w.id(), old);
        w.store.stamp_stored_versions(&other_id, old);
        w.git.fail_next(GitError::Timeout);

        let response = w.reindex().execute(&ReindexRequest::all()).unwrap();

        assert_eq!(response.failed, 1);
        let results: Vec<&str> = response.sources.iter().map(|e| e.result.as_str()).collect();
        assert_eq!(results, ["failed", "full"]);
        assert!(response.sources[0].error.is_some());
    }

    #[test]
    fn reindex_is_refused_while_the_source_is_locked() {
        let (w, _) = indexed_world();
        let _held = w.locks.acquire(&w.id()).unwrap();
        let error = w
            .reindex()
            .execute(&ReindexRequest::named(NAME))
            .unwrap_err();
        assert_eq!(error.code().exit_code(), 6);
    }

    fn second_source(w: &World) -> crate::domain::source_id::SourceId {
        let other = "https://example.com/o/other";
        w.git.add_remote(other, "main");
        w.git.commit(other, &[("b.rs", "fn b() {}\n")]);
        w.add_source()
            .execute(&crate::application::add_source::AddRequest::new(other))
            .unwrap();
        crate::application::indexer_test_support::id_of(other)
    }

    #[test]
    fn reindex_all_skips_a_busy_source_and_it_is_not_a_failure() {
        let (w, _) = indexed_world();
        let other = second_source(&w);
        let _held = w.locks.acquire(&other).unwrap();

        let response = w.reindex().execute(&ReindexRequest::all()).unwrap();

        let results: Vec<&str> = response.sources.iter().map(|e| e.result.as_str()).collect();
        assert_eq!(results, ["skipped_busy", "up_to_date"]);
        assert_eq!((response.failed, response.exit_code()), (0, 0));
        assert_eq!(
            response.sources[0].error.as_ref().unwrap().code,
            "index_locked"
        );
    }

    #[test]
    fn reindex_all_skips_a_source_removed_while_it_ran() {
        let (w, _) = indexed_world();
        let other = second_source(&w);
        let spied = SpiedStore::new(&w.store).hiding(&other);

        let response = Reindex::new(&spied, &spied, &w.git, &w.locks, &w.clock)
            .execute(&ReindexRequest::all())
            .unwrap();

        let results: Vec<&str> = response.sources.iter().map(|e| e.result.as_str()).collect();
        assert_eq!(results, ["skipped_removed", "up_to_date"]);
        assert_eq!((response.failed, response.exit_code()), (0, 0));
    }

    #[test]
    fn a_failing_source_decides_the_exit_code_of_reindex_all() {
        let (w, _) = indexed_world();
        let other = second_source(&w);
        let mut old = IndexVersions::current();
        old.chunker = 0;
        w.store.stamp_stored_versions(&w.id(), old);
        w.store.stamp_stored_versions(&other, old);
        w.git.fail_next(GitError::Timeout);

        let response = w.reindex().execute(&ReindexRequest::all()).unwrap();

        assert_eq!(response.failed, 1);
        assert_eq!(response.exit_code(), 4);
    }

    #[test]
    fn a_reindex_failure_is_remembered_on_the_source() {
        let (w, _) = indexed_world();
        let mut old = IndexVersions::current();
        old.chunker = 0;
        w.store.stamp_stored_versions(&w.id(), old);
        w.store.fail_next_commit();

        assert!(w.reindex().execute(&ReindexRequest::named(NAME)).is_err());

        let source = w.store.get(&w.id()).unwrap().unwrap();
        assert_eq!(source.last_error().unwrap().code, "error");
        w.reindex().execute(&ReindexRequest::named(NAME)).unwrap();
        assert_eq!(w.store.get(&w.id()).unwrap().unwrap().last_error(), None);
    }

    #[test]
    fn a_full_reindex_reports_its_phases() {
        let (w, _) = indexed_world();
        let mut old = IndexVersions::current();
        old.chunker = 0;
        w.store.stamp_stored_versions(&w.id(), old);

        let response = w.reindex().execute(&ReindexRequest::all()).unwrap();

        let phases: Vec<&str> = response.sources[0]
            .index
            .as_ref()
            .unwrap()
            .phases
            .iter()
            .map(|p| p.phase)
            .collect();
        assert_eq!(phases, ["list", "read", "commit"]);
    }

    #[test]
    fn a_source_that_never_finished_indexing_is_not_reindexed() {
        let w = World::new();
        w.commit(&[("a.rs", "fn a() {}\n")]);
        w.git.fail_next(GitError::Timeout);
        assert!(w.add().is_err());

        let response = w.reindex().execute(&ReindexRequest::all()).unwrap();

        assert_eq!(response.sources[0].result, "not_indexed");
        assert_eq!(response.failed, 0);
    }
}
