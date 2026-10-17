//! Update: fetch and re-index what changed.

use std::time::Duration;

use serde::Serialize;

use crate::application::dto::{ErrorBody, SCHEMA_VERSION};
use crate::application::error::AppError;
use crate::application::index_pipeline::IndexReport;
use crate::application::indexer::{
    error_body, exit_code_of_failures, is_contention, resolve_source, Indexer, DEFAULT_LOCK_WAIT,
    NO_SOURCES_MESSAGE,
};
use crate::application::ports::{ChunkStore, Clock, GitClient, OperationLock, SourceRepository};
use crate::application::progress::{Progress, NO_PROGRESS};
use crate::application::search::unknown_source;
use crate::domain::oid::CommitId;
use crate::domain::source::Source;
use crate::domain::source_id::SourceId;

/// Input of the update use case.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct UpdateRequest {
    /// Source name or slug; `None` updates every source.
    pub name: Option<String>,
}

impl UpdateRequest {
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
pub struct UpdateEntry {
    pub repo: String,
    pub name: String,
    /// `updated`, `unchanged`, `indexed` (a source that never finished its first
    /// index), `failed`, `skipped_busy` (another process was working on the source),
    /// or `skipped_removed` (the source was removed while the command ran). The two
    /// skipped results are not failures.
    pub result: String,
    pub previous_commit: Option<String>,
    pub index: Option<IndexReport>,
    pub error: Option<ErrorBody>,
}

/// Output of the update use case.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct UpdateResponse {
    pub schema_version: u32,
    pub sources: Vec<UpdateEntry>,
    /// How many sources failed (only possible when updating all of them). Sources
    /// skipped as busy or removed are not counted.
    pub failed: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
}

impl UpdateResponse {
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

pub struct UpdateSource<'a> {
    sources: &'a dyn SourceRepository,
    store: &'a dyn ChunkStore,
    git: &'a dyn GitClient,
    locks: &'a dyn OperationLock,
    clock: &'a dyn Clock,
    progress: &'a dyn Progress,
    lock_wait: Duration,
}

impl<'a> UpdateSource<'a> {
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
            lock_wait: DEFAULT_LOCK_WAIT,
        }
    }

    /// Reports the phases of the update to `progress`.
    pub fn with_progress(mut self, progress: &'a dyn Progress) -> Self {
        self.progress = progress;
        self
    }

    /// How long to wait for the operation lock of each source (default
    /// `DEFAULT_LOCK_WAIT`); `Duration::ZERO` fails or skips at once (`--no-wait`).
    pub fn with_lock_wait(mut self, wait: Duration) -> Self {
        self.lock_wait = wait;
        self
    }

    pub fn execute(&self, request: &UpdateRequest) -> Result<UpdateResponse, AppError> {
        if let Some(name) = &request.name {
            let source = resolve_source(self.sources, name)?;
            let entry = self
                .update_one(source.id())?
                .ok_or_else(|| unknown_source(self.sources, name))?;
            return Ok(Self::response(vec![entry], None));
        }
        let all = self.sources.list()?;
        if all.is_empty() {
            return Ok(Self::response(Vec::new(), Some(NO_SOURCES_MESSAGE)));
        }
        let mut entries = Vec::with_capacity(all.len());
        for source in &all {
            entries.push(match self.update_one(source.id()) {
                Ok(Some(entry)) => entry,
                Ok(None) => Self::unfinished_entry(
                    source,
                    "skipped_removed",
                    &AppError::NotFound("the source was removed while the command ran".into()),
                ),
                // The budget of the whole command is spent: stop, do not go on to
                // the next source. The ones done are committed.
                Err(error) if error.is_deadline() => return Err(error),
                Err(error) if is_contention(&error) => {
                    Self::unfinished_entry(source, "skipped_busy", &error)
                }
                Err(error) => Self::unfinished_entry(source, "failed", &error),
            });
        }
        Ok(Self::response(entries, None))
    }

    fn response(sources: Vec<UpdateEntry>, message: Option<&str>) -> UpdateResponse {
        UpdateResponse {
            schema_version: SCHEMA_VERSION,
            failed: sources
                .iter()
                .filter(|entry| entry.result == "failed")
                .count(),
            sources,
            message: message.map(str::to_owned),
        }
    }

    /// A source the update did not finish: `failed`, or left alone (`skipped_*`, not
    /// a failure). `error` says why.
    fn unfinished_entry(source: &Source, result: &str, error: &AppError) -> UpdateEntry {
        UpdateEntry {
            repo: source.id().slug().to_owned(),
            name: source.name().to_owned(),
            result: result.to_owned(),
            previous_commit: source.indexed().map(|revision| revision.commit.to_string()),
            index: None,
            error: Some(error_body(error)),
        }
    }

    /// Updates one source under its operation lock. `None` when the source was
    /// removed after it was listed.
    fn update_one(&self, id: &SourceId) -> Result<Option<UpdateEntry>, AppError> {
        let _lock = self.locks.acquire_waiting(id, "update", self.lock_wait)?;
        // Read the source under the lock: it may have changed since it was listed.
        let Some(source) = self.sources.get(id)? else {
            return Ok(None);
        };
        let indexer = Indexer::new(self.sources, self.store, self.git, self.clock)
            .with_progress(self.progress);
        let entry =
            |source: &Source, result: &str, previous: Option<&CommitId>, report| UpdateEntry {
                repo: source.id().slug().to_owned(),
                name: source.name().to_owned(),
                result: result.to_owned(),
                previous_commit: previous.map(ToString::to_string),
                index: Some(report),
                error: None,
            };
        if source.indexed().is_none() {
            let indexed = indexer.first_index(source)?;
            return Ok(Some(entry(
                &indexed.source,
                "indexed",
                None,
                indexed.report,
            )));
        }
        let refreshed = indexer.refresh(source)?;
        let result = if refreshed.unchanged {
            "unchanged"
        } else {
            "updated"
        };
        Ok(Some(entry(
            &refreshed.source,
            result,
            Some(&refreshed.previous),
            refreshed.report,
        )))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::application::error::{ErrorCode, GitError, StorageError};
    use crate::application::indexer_test_support::{GitSpy, SpiedStore, World, REMOTE};
    use crate::application::ports::{ChunkStore, IndexChanges, ReindexedChunk, SourceRepository};
    use crate::application::testkit::StampStoredVersions;
    use crate::domain::chunk::Chunk;
    use crate::domain::chunk_id::ChunkId;
    use crate::domain::index_versions::IndexVersions;
    use crate::domain::indexed_at::IndexedAt;
    use crate::domain::manifest::Manifest;
    use crate::domain::oid::CommitId;
    use crate::domain::source::{Source, SourceStatus};
    use crate::domain::source_id::SourceId;
    use std::time::Duration;

    fn paths(manifest: &Manifest) -> Vec<String> {
        manifest
            .iter()
            .map(|(p, _)| p.as_str().to_owned())
            .collect()
    }

    fn added(w: &World, files: &[(&str, &str)]) -> CommitId {
        w.commit(files);
        let response = w.add().unwrap();
        CommitId::parse(&response.commit).unwrap()
    }

    #[test]
    fn update_indexes_only_changed_and_new_files_and_drops_removed_ones() {
        let w = World::new();
        added(
            &w,
            &[
                ("a.rs", "fn a() {}\n"),
                ("b.rs", "fn b() {}\n"),
                ("c.rs", "fn c() {}\n"),
            ],
        );
        let second = w.commit(&[
            ("a.rs", "fn a_changed() {}\n"),
            ("c.rs", "fn c() {}\n"),
            ("d.rs", "fn d() {}\n"),
        ]);
        let spy = GitSpy::new(&w.git);

        let response = w
            .update_source_via(&spy)
            .execute(&UpdateRequest::all())
            .unwrap();

        let entry = &response.sources[0];
        assert_eq!(entry.result, "updated");
        let report = entry.index.as_ref().unwrap();
        assert_eq!(
            (
                report.files_added,
                report.files_changed,
                report.files_removed,
                report.files_unchanged
            ),
            (1, 1, 1, 1)
        );
        assert_eq!(report.commit, second.to_string());
        assert_eq!(
            paths(&w.store.manifest(&w.id()).unwrap()),
            ["a.rs", "c.rs", "d.rs"]
        );
        assert_eq!(
            spy.reads(),
            [(2, 28)],
            "only the changed and new blobs are read"
        );
        let source = w.store.get(&w.id()).unwrap().unwrap();
        assert_eq!(source.indexed().unwrap().commit, second);
        assert_eq!(source.status(), SourceStatus::Ready);
    }

    #[test]
    fn update_replaces_the_chunks_of_a_changed_file() {
        let w = World::new();
        added(&w, &[("a.rs", "fn old_name() {}\n")]);
        w.commit(&[("a.rs", "fn new_name() {}\n")]);
        w.update_source().execute(&UpdateRequest::all()).unwrap();
        let text: Vec<String> = w
            .store
            .chunks(&w.id())
            .unwrap()
            .into_iter()
            .map(|c: Chunk| c.text)
            .collect();
        assert_eq!(text, ["fn new_name() {}"]);
    }

    #[test]
    fn a_renamed_file_is_a_removal_plus_an_addition() {
        let w = World::new();
        added(
            &w,
            &[("old.rs", "fn same() {}\n"), ("keep.rs", "fn keep() {}\n")],
        );
        w.commit(&[("new.rs", "fn same() {}\n"), ("keep.rs", "fn keep() {}\n")]);

        let response = w.update_source().execute(&UpdateRequest::all()).unwrap();

        let report = response.sources[0].index.as_ref().unwrap();
        assert_eq!(
            (
                report.files_added,
                report.files_removed,
                report.files_changed
            ),
            (1, 1, 0)
        );
        assert_eq!(
            paths(&w.store.manifest(&w.id()).unwrap()),
            ["keep.rs", "new.rs"]
        );
    }

    #[test]
    fn an_unchanged_commit_is_a_no_op() {
        let w = World::new();
        let head = added(&w, &[("a.rs", "fn a() {}\n")]);
        let before = w.store.get(&w.id()).unwrap().unwrap();
        w.clock.advance(500);
        let spy = GitSpy::new(&w.git);

        let response = w
            .update_source_via(&spy)
            .execute(&UpdateRequest::named("example.com/o/r"))
            .unwrap();

        assert_eq!(response.sources[0].result, "unchanged");
        assert_eq!(
            response.sources[0].previous_commit.as_deref(),
            Some(head.as_str())
        );
        assert!(!spy.called("ls_tree") && !spy.called("read_blobs"));
        let after = w.store.get(&w.id()).unwrap().unwrap();
        assert_eq!(after.indexed(), before.indexed(), "same commit, same time");
        assert_eq!(after.status(), SourceStatus::Ready);
    }

    #[test]
    fn a_new_commit_that_changes_no_indexed_file_still_advances_the_commit() {
        let w = World::new();
        added(&w, &[("a.rs", "fn a() {}\n")]);
        let second = w.commit(&[("a.rs", "fn a() {}\n"), ("logo.png", "png")]);
        let spy = GitSpy::new(&w.git);

        let response = w
            .update_source_via(&spy)
            .execute(&UpdateRequest::all())
            .unwrap();

        assert_eq!(response.sources[0].result, "updated");
        assert!(!spy.called("read_blobs"));
        assert_eq!(
            w.store
                .get(&w.id())
                .unwrap()
                .unwrap()
                .indexed()
                .unwrap()
                .commit,
            second
        );
    }

    #[test]
    fn a_file_skipped_by_content_is_not_read_again_while_its_blob_is_unchanged() {
        let w = World::new();
        added(&w, &[("data/blob", "a\0b"), ("a.rs", "fn a() {}\n")]);
        w.commit(&[("data/blob", "a\0b"), ("a.rs", "fn a2() {}\n")]);
        let spy = GitSpy::new(&w.git);

        w.update_source_via(&spy)
            .execute(&UpdateRequest::all())
            .unwrap();

        assert_eq!(spy.reads().len(), 1);
        assert_eq!(spy.reads()[0].0, 1, "only a.rs was read");
    }

    #[test]
    fn a_changed_file_that_turns_secret_leaves_the_index() {
        let w = World::new();
        added(&w, &[("a.rs", "fn a() {}\n")]);
        let token = format!("const T: &str = \"ghp_{}\";\n", "aB3".repeat(12));
        w.commit(&[("a.rs", &token)]);

        w.update_source().execute(&UpdateRequest::all()).unwrap();

        assert!(w.store.manifest(&w.id()).unwrap().is_empty());
        assert_eq!(w.store.chunk_count(&w.id()).unwrap(), 0);
    }

    #[test]
    fn update_finds_a_source_by_name_or_slug_and_reports_unknown_names() {
        let w = World::new();
        w.commit(&[("a.rs", "fn a() {}\n")]);
        w.add_source()
            .execute(&crate::application::add_source::AddRequest::named(
                REMOTE, "Docs",
            ))
            .unwrap();
        w.commit(&[("a.rs", "fn a2() {}\n")]);

        assert_eq!(
            w.update_source()
                .execute(&UpdateRequest::named("docs"))
                .unwrap()
                .sources[0]
                .result,
            "updated"
        );
        w.commit(&[("a.rs", "fn a3() {}\n")]);
        assert_eq!(
            w.update_source()
                .execute(&UpdateRequest::named("example.com/o/r"))
                .unwrap()
                .sources[0]
                .result,
            "updated"
        );
        let error = w
            .update_source()
            .execute(&UpdateRequest::named("nope"))
            .unwrap_err();
        assert_eq!(error.code(), ErrorCode::NotFound);
    }

    #[test]
    fn a_failed_fetch_keeps_serving_the_old_index_and_a_retry_succeeds() {
        let w = World::new();
        let first = added(&w, &[("a.rs", "fn a() {}\n")]);
        w.commit(&[("a.rs", "fn a2() {}\n")]);
        w.git.fail_next(GitError::NetworkOrAuth("offline".into()));

        let error = w
            .update_source()
            .execute(&UpdateRequest::named("example.com/o/r"))
            .unwrap_err();

        assert_eq!(error.code(), ErrorCode::NetworkOrAuth);
        let source = w.store.get(&w.id()).unwrap().unwrap();
        assert_eq!(source.status(), SourceStatus::Failed);
        assert_eq!(source.indexed().unwrap().commit, first);
        assert!(source.is_searchable(), "the old index is still served");
        assert_eq!(w.store.chunks(&w.id()).unwrap()[0].text, "fn a() {}");

        w.update_source().execute(&UpdateRequest::all()).unwrap();
        assert_eq!(
            w.store.get(&w.id()).unwrap().unwrap().status(),
            SourceStatus::Ready
        );
        assert_eq!(w.store.chunks(&w.id()).unwrap()[0].text, "fn a2() {}");
    }

    #[test]
    fn a_failed_commit_leaves_the_old_index_untouched() {
        let w = World::new();
        let first = added(&w, &[("a.rs", "fn a() {}\n")]);
        w.commit(&[("a.rs", "fn a2() {}\n"), ("b.rs", "fn b() {}\n")]);
        w.store.fail_next_commit();

        assert!(w
            .update_source()
            .execute(&UpdateRequest::named("example.com/o/r"))
            .is_err());

        let source = w.store.get(&w.id()).unwrap().unwrap();
        assert_eq!(source.indexed().unwrap().commit, first);
        assert_eq!(paths(&w.store.manifest(&w.id()).unwrap()), ["a.rs"]);
        assert_eq!(w.store.chunks(&w.id()).unwrap()[0].text, "fn a() {}");
    }

    /// A store that lets another writer commit between this writer's read of the
    /// base and its own `commit_index`.
    struct RacingStore<'a> {
        inner: &'a crate::application::testkit::InMemoryStore,
        rival_commit: CommitId,
    }

    impl SourceRepository for RacingStore<'_> {
        fn get(&self, id: &SourceId) -> Result<Option<Source>, StorageError> {
            self.inner.get(id)
        }
        fn find_by_name(&self, name: &str) -> Result<Option<Source>, StorageError> {
            self.inner.find_by_name(name)
        }
        fn list(&self) -> Result<Vec<Source>, StorageError> {
            self.inner.list()
        }
        fn save(&self, source: &Source) -> Result<(), StorageError> {
            self.inner.save(source)
        }
    }

    impl ChunkStore for RacingStore<'_> {
        fn commit_index(
            &self,
            source: &Source,
            expected_base: Option<&CommitId>,
            changes: &IndexChanges,
        ) -> Result<(), StorageError> {
            let mut rival = self.inner.get(source.id())?.unwrap();
            rival.mark_ready(self.rival_commit.clone(), source.indexed().unwrap().at);
            self.inner
                .commit_index(&rival, expected_base, &IndexChanges::default())?;
            self.inner.commit_index(source, expected_base, changes)
        }
        fn remove_source(&self, id: &SourceId) -> Result<(), StorageError> {
            self.inner.remove_source(id)
        }
        fn manifest(&self, source: &SourceId) -> Result<Manifest, StorageError> {
            self.inner.manifest(source)
        }
        fn chunk_count(&self, source: &SourceId) -> Result<usize, StorageError> {
            self.inner.chunk_count(source)
        }
        fn chunk(&self, id: &ChunkId) -> Result<Option<Chunk>, StorageError> {
            self.inner.chunk(id)
        }
        fn chunks(&self, source: &SourceId) -> Result<Vec<Chunk>, StorageError> {
            self.inner.chunks(source)
        }
        fn stored_versions(
            &self,
            source: &SourceId,
        ) -> Result<Option<IndexVersions>, StorageError> {
            self.inner.stored_versions(source)
        }
        fn rewrite_index_fields(
            &self,
            source: &SourceId,
            fields: &[ReindexedChunk],
            versions: IndexVersions,
        ) -> Result<(), StorageError> {
            self.inner.rewrite_index_fields(source, fields, versions)
        }
    }

    #[test]
    fn losing_a_race_to_another_writer_is_a_conflict_with_exit_six() {
        let w = World::new();
        added(&w, &[("a.rs", "fn a() {}\n")]);
        w.commit(&[("a.rs", "fn a2() {}\n")]);
        let racing = RacingStore {
            inner: &w.store,
            rival_commit: CommitId::parse(&"ee".repeat(20)).unwrap(),
        };
        let update = UpdateSource::new(&racing, &racing, &w.git, &w.locks, &w.clock);

        let error = update
            .execute(&UpdateRequest::named("example.com/o/r"))
            .unwrap_err();

        assert_eq!(error, AppError::Storage(StorageError::Conflict));
        assert_eq!(error.code().exit_code(), 6);
        let source = w.store.get(&w.id()).unwrap().unwrap();
        assert_eq!(
            source.status(),
            SourceStatus::Ready,
            "a lost race is not a failure and leaves no Indexing behind"
        );
        assert_eq!(source.last_error(), None);
        assert_eq!(source.indexed().unwrap().commit.as_str(), "ee".repeat(20));
        assert_eq!(
            w.store.chunks(&w.id()).unwrap()[0].text,
            "fn a() {}",
            "the loser wrote nothing"
        );
    }

    #[test]
    fn update_is_refused_while_the_source_is_locked() {
        let w = World::new();
        added(&w, &[("a.rs", "fn a() {}\n")]);
        let held = w.locks.acquire(&w.id()).unwrap();
        let error = w
            .update_source()
            .with_lock_wait(Duration::ZERO)
            .execute(&UpdateRequest::named("example.com/o/r"))
            .unwrap_err();
        assert_eq!(error.code().exit_code(), 6);
        drop(held);
        assert!(w
            .update_source()
            .execute(&UpdateRequest::named("example.com/o/r"))
            .is_ok());
    }

    fn two_sources(w: &World) -> (SourceId, SourceId) {
        added(w, &[("a.rs", "fn a() {}\n")]);
        let other = "https://example.com/o/other";
        w.git.add_remote(other, "main");
        w.git.commit(other, &[("b.rs", "fn b() {}\n")]);
        w.add_source()
            .execute(&crate::application::add_source::AddRequest::new(other))
            .unwrap();
        w.commit(&[("a.rs", "fn a2() {}\n")]);
        w.git.commit(other, &[("b.rs", "fn b2() {}\n")]);
        (
            w.id(),
            crate::application::indexer_test_support::id_of(other),
        )
    }

    fn results(response: &UpdateResponse) -> Vec<(&str, &str)> {
        response
            .sources
            .iter()
            .map(|e| (e.repo.as_str(), e.result.as_str()))
            .collect()
    }

    #[test]
    fn update_all_skips_a_busy_source_and_it_is_not_a_failure() {
        let w = World::new();
        let (_, other) = two_sources(&w);
        let _held = w.locks.acquire(&other).unwrap();

        let response = w
            .update_source()
            .with_lock_wait(Duration::ZERO)
            .execute(&UpdateRequest::all())
            .unwrap();

        assert_eq!(
            results(&response),
            [
                ("example.com/o/other", "skipped_busy"),
                ("example.com/o/r", "updated")
            ]
        );
        assert_eq!(response.failed, 0);
        assert_eq!(
            response.exit_code(),
            0,
            "skipped sources do not change the exit code"
        );
        assert_eq!(
            response.sources[0].error.as_ref().unwrap().code,
            "index_locked",
            "the body still says why"
        );
    }

    #[test]
    fn update_all_names_the_operation_that_made_a_source_busy() {
        let w = World::new();
        let (_, other) = two_sources(&w);
        let _held = w
            .locks
            .acquire_waiting(&other, "add", Duration::ZERO)
            .unwrap();

        let response = w
            .update_source()
            .with_lock_wait(Duration::ZERO)
            .execute(&UpdateRequest::all())
            .unwrap();

        let message = &response.sources[0].error.as_ref().unwrap().message;
        assert!(message.contains("`add`"), "{message}");
    }

    #[test]
    fn update_all_skips_a_source_removed_while_it_ran() {
        let w = World::new();
        let (_, other) = two_sources(&w);
        let spied = SpiedStore::new(&w.store).hiding(&other);

        let response = UpdateSource::new(&spied, &spied, &w.git, &w.locks, &w.clock)
            .execute(&UpdateRequest::all())
            .unwrap();

        assert_eq!(
            results(&response),
            [
                ("example.com/o/other", "skipped_removed"),
                ("example.com/o/r", "updated")
            ]
        );
        assert_eq!((response.failed, response.exit_code()), (0, 0));
    }

    #[test]
    fn a_skipped_source_beside_a_real_failure_leaves_the_failure_to_decide_the_exit_code() {
        let w = World::new();
        let (first, other) = two_sources(&w);
        let _held = w.locks.acquire(&other).unwrap();
        w.git.fail_next(GitError::Timeout);

        let response = w
            .update_source()
            .with_lock_wait(Duration::ZERO)
            .execute(&UpdateRequest::all())
            .unwrap();

        assert_eq!(
            results(&response),
            [
                ("example.com/o/other", "skipped_busy"),
                ("example.com/o/r", "failed")
            ]
        );
        assert_eq!(response.failed, 1);
        assert_eq!(response.exit_code(), 4);
        drop(first);
    }

    fn body(code: &str) -> ErrorBody {
        ErrorBody {
            code: code.to_owned(),
            message: String::new(),
            hint: None,
            details: None,
        }
    }

    fn failed(code: &str) -> UpdateEntry {
        UpdateEntry {
            repo: "r".into(),
            name: "r".into(),
            result: "failed".into(),
            previous_commit: None,
            index: None,
            error: Some(body(code)),
        }
    }

    fn response_of(entries: Vec<UpdateEntry>) -> UpdateResponse {
        UpdateResponse {
            schema_version: 1,
            failed: entries.iter().filter(|e| e.result == "failed").count(),
            sources: entries,
            message: None,
        }
    }

    #[test]
    fn the_exit_code_is_the_shared_failure_code_or_one() {
        assert_eq!(response_of(vec![]).exit_code(), 0);
        assert_eq!(response_of(vec![failed("not_found")]).exit_code(), 3);
        assert_eq!(
            response_of(vec![failed("not_found"), failed("not_found")]).exit_code(),
            3
        );
        assert_eq!(
            response_of(vec![failed("not_found"), failed("network_or_auth")]).exit_code(),
            1
        );
        assert_eq!(response_of(vec![failed("reindex_required")]).exit_code(), 7);
        assert_eq!(response_of(vec![failed("something_new")]).exit_code(), 1);
    }

    #[test]
    fn a_busy_commit_is_not_recorded_as_a_failure_and_saves_nothing() {
        let w = World::new();
        added(&w, &[("a.rs", "fn a() {}\n")]);
        w.commit(&[("a.rs", "fn a2() {}\n")]);
        let spied = SpiedStore::new(&w.store).failing_commits(StorageError::Busy);

        let error = UpdateSource::new(&spied, &spied, &w.git, &w.locks, &w.clock)
            .execute(&UpdateRequest::named("example.com/o/r"))
            .unwrap_err();

        assert_eq!(error, AppError::Storage(StorageError::Busy));
        let source = w.store.get(&w.id()).unwrap().unwrap();
        assert_eq!(source.status(), SourceStatus::Ready);
        assert_eq!(source.last_error(), None);
        assert!(
            spied.saved_statuses().is_empty(),
            "no save, so no second wait for the lock"
        );
    }

    #[test]
    fn a_busy_git_repository_is_not_recorded_as_a_failure_either() {
        let w = World::new();
        added(&w, &[("a.rs", "fn a() {}\n")]);
        w.commit(&[("a.rs", "fn a2() {}\n")]);
        w.git.fail_next(GitError::Busy);

        let error = w
            .update_source()
            .execute(&UpdateRequest::named("example.com/o/r"))
            .unwrap_err();

        assert_eq!(error.code(), ErrorCode::IndexLocked);
        let source = w.store.get(&w.id()).unwrap().unwrap();
        assert_eq!(
            (source.status(), source.last_error()),
            (SourceStatus::Ready, None)
        );
    }

    #[test]
    fn updating_an_indexed_source_never_writes_the_indexing_status() {
        let w = World::new();
        added(&w, &[("a.rs", "fn a() {}\n")]);
        w.commit(&[("a.rs", "fn a2() {}\n")]);
        let spied = SpiedStore::new(&w.store);

        UpdateSource::new(&spied, &spied, &w.git, &w.locks, &w.clock)
            .execute(&UpdateRequest::all())
            .unwrap();

        assert!(
            !spied.saved_statuses().contains(&SourceStatus::Indexing),
            "{:?}",
            spied.saved_statuses()
        );
    }

    #[test]
    fn a_failed_update_saves_failed_and_nothing_before_it() {
        let w = World::new();
        added(&w, &[("a.rs", "fn a() {}\n")]);
        w.commit(&[("a.rs", "fn a2() {}\n")]);
        w.git.fail_next(GitError::Timeout);
        let spied = SpiedStore::new(&w.store);

        UpdateSource::new(&spied, &spied, &w.git, &w.locks, &w.clock)
            .execute(&UpdateRequest::named("example.com/o/r"))
            .unwrap_err();

        assert_eq!(spied.saved_statuses(), [SourceStatus::Failed]);
    }

    #[test]
    fn a_failure_is_remembered_on_the_source_until_a_good_update_clears_it() {
        let w = World::new();
        added(&w, &[("a.rs", "fn a() {}\n")]);
        w.commit(&[("a.rs", "fn a2() {}\n")]);
        w.clock.advance(77);
        w.git.fail_next(GitError::NetworkOrAuth("offline".into()));
        w.update_source()
            .execute(&UpdateRequest::named("example.com/o/r"))
            .unwrap_err();

        let failed = w.store.get(&w.id()).unwrap().unwrap();
        let error = failed.last_error().expect("the failure is recorded");
        assert_eq!(error.code, "network_or_auth");
        assert!(error.message.contains("offline"), "{}", error.message);
        assert_eq!(error.at, IndexedAt::from_epoch_seconds(1_077));

        w.update_source().execute(&UpdateRequest::all()).unwrap();
        let healed = w.store.get(&w.id()).unwrap().unwrap();
        assert_eq!(healed.last_error(), None);
        assert_eq!(healed.status(), SourceStatus::Ready);
    }

    #[test]
    fn update_waits_the_default_lock_time_for_each_source() {
        use crate::application::indexer_test_support::RecordingLock;
        let w = World::new();
        added(&w, &[("a.rs", "fn a() {}\n")]);
        let locks = RecordingLock::new(&w.locks);

        w.update_source_locked(&locks)
            .execute(&UpdateRequest::all())
            .unwrap();

        assert_eq!(locks.waits(), [("update".to_owned(), DEFAULT_LOCK_WAIT)]);
    }

    #[test]
    fn an_unchanged_update_also_clears_a_remembered_failure() {
        let w = World::new();
        added(&w, &[("a.rs", "fn a() {}\n")]);
        w.git.fail_next(GitError::Timeout);
        w.update_source()
            .execute(&UpdateRequest::named("example.com/o/r"))
            .unwrap_err();
        assert!(w
            .store
            .get(&w.id())
            .unwrap()
            .unwrap()
            .last_error()
            .is_some());

        let response = w.update_source().execute(&UpdateRequest::all()).unwrap();

        assert_eq!(response.sources[0].result, "unchanged");
        assert_eq!(w.store.get(&w.id()).unwrap().unwrap().last_error(), None);
    }

    #[test]
    fn a_renamed_default_branch_is_followed_once() {
        let w = World::new();
        added(&w, &[("a.rs", "fn a() {}\n")]);
        w.git.rename_default_branch(REMOTE, "trunk");
        let second = w.commit(&[("a.rs", "fn a_on_trunk() {}\n")]);
        let spy = GitSpy::new(&w.git);

        let response = w
            .update_source_via(&spy)
            .execute(&UpdateRequest::all())
            .unwrap();

        assert_eq!(response.sources[0].result, "updated");
        let source = w.store.get(&w.id()).unwrap().unwrap();
        assert_eq!(source.git_ref().map(|r| r.as_str()), Some("trunk"));
        assert_eq!(source.indexed().unwrap().commit, second);
        assert_eq!(
            spy.calls()
                .iter()
                .filter(|c| **c == "default_branch")
                .count(),
            1,
            "the default branch is looked up once"
        );
    }

    #[test]
    fn a_vanished_branch_with_no_other_default_says_what_happened() {
        let w = World::new();
        added(&w, &[("a.rs", "fn a() {}\n")]);
        w.git
            .fail_next(GitError::NotFound("couldn't find remote ref main".into()));

        let error = w
            .update_source()
            .execute(&UpdateRequest::named("example.com/o/r"))
            .unwrap_err();

        assert_eq!(error.code(), ErrorCode::NotFound);
        let shown = error.to_string();
        assert!(shown.contains("tracked branch main"), "{shown}");
        assert!(
            shown.contains("rag remove") && shown.contains("rag add"),
            "{shown}"
        );
        let source = w.store.get(&w.id()).unwrap().unwrap();
        assert_eq!(source.status(), SourceStatus::Failed);
        assert!(source.is_searchable(), "the old index is still served");
    }

    #[test]
    fn update_reports_its_phases() {
        let w = World::new();
        added(&w, &[("a.rs", "fn a() {}\n")]);
        let progress = crate::application::indexer_test_support::RecordingProgress::default();

        w.update_source()
            .with_progress(&progress)
            .execute(&UpdateRequest::all())
            .unwrap();

        assert_eq!(
            progress.sequence(),
            ["begin:fetch", "end:fetch"],
            "unchanged: only the fetch"
        );
    }

    #[test]
    fn updating_an_empty_registry_says_so_without_failing() {
        let w = World::new();
        let response = w.update_source().execute(&UpdateRequest::all()).unwrap();
        assert!(response.sources.is_empty());
        assert_eq!(response.failed, 0);
        assert!(response.message.unwrap().contains("rag add"));
    }

    #[test]
    fn update_all_reports_each_sources_failure_and_goes_on() {
        let w = World::new();
        added(&w, &[("a.rs", "fn a() {}\n")]);
        let other = "https://example.com/o/other";
        w.git.add_remote(other, "main");
        w.git.commit(other, &[("b.rs", "fn b() {}\n")]);
        w.add_source()
            .execute(&crate::application::add_source::AddRequest::new(other))
            .unwrap();
        w.commit(&[("a.rs", "fn a2() {}\n")]);
        w.git.commit(other, &[("b.rs", "fn b2() {}\n")]);
        w.git.fail_next(GitError::Timeout);

        let response = w.update_source().execute(&UpdateRequest::all()).unwrap();

        assert_eq!(response.failed, 1);
        let results: Vec<(&str, &str)> = response
            .sources
            .iter()
            .map(|e| (e.repo.as_str(), e.result.as_str()))
            .collect();
        assert_eq!(
            results,
            [
                ("example.com/o/other", "failed"),
                ("example.com/o/r", "updated")
            ]
        );
        let failure = response.sources[0].error.as_ref().unwrap();
        assert_eq!(failure.code, "network_or_auth");
        assert_eq!(
            response.exit_code(),
            4,
            "the one failure decides the exit code"
        );
    }

    #[test]
    fn updating_a_source_that_never_finished_indexing_retries_the_first_index() {
        let w = World::new();
        let head = w.commit(&[("a.rs", "fn a() {}\n")]);
        w.git.fail_next(GitError::Timeout);
        assert!(w.add().is_err());

        let response = w.update_source().execute(&UpdateRequest::all()).unwrap();

        assert_eq!(response.sources[0].result, "indexed");
        assert_eq!(
            w.store
                .get(&w.id())
                .unwrap()
                .unwrap()
                .indexed()
                .unwrap()
                .commit,
            head
        );
    }

    #[test]
    fn updating_an_index_made_by_older_rules_asks_for_a_reindex() {
        let w = World::new();
        added(&w, &[("a.rs", "fn a() {}\n")]);
        let mut old = IndexVersions::current();
        old.chunker = 0;
        w.store.stamp_stored_versions(&w.id(), old);
        w.commit(&[("a.rs", "fn a2() {}\n")]);

        let error = w
            .update_source()
            .execute(&UpdateRequest::named("example.com/o/r"))
            .unwrap_err();

        assert_eq!(
            error.code(),
            crate::application::error::ErrorCode::ReindexRequired
        );
        assert!(error.stale_source().is_some(), "{error}");
        assert_eq!(error.code().exit_code(), 7);
    }

    #[test]
    fn response_serializes_without_a_message_when_there_is_none() {
        let w = World::new();
        added(&w, &[("a.rs", "fn a() {}\n")]);
        let json =
            serde_json::to_string(&w.update_source().execute(&UpdateRequest::all()).unwrap())
                .unwrap();
        assert!(
            json.starts_with(r#"{"schema_version":1,"sources":[{"repo":"example.com/o/r""#),
            "{json}"
        );
        assert!(!json.contains("message"), "{json}");
    }
}
