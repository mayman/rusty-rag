//! Update: fetch and re-index what changed.

use serde::Serialize;

use crate::application::dto::{ErrorBody, SCHEMA_VERSION};
use crate::application::error::AppError;
use crate::application::index_pipeline::IndexReport;
use crate::application::indexer::{error_body, resolve_source, Indexer, NO_SOURCES_MESSAGE};
use crate::application::ports::{ChunkStore, Clock, GitClient, OperationLock, SourceRepository};
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
    /// index), or `failed`.
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
    /// How many sources failed (only possible when updating all of them).
    pub failed: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
}

pub struct UpdateSource<'a> {
    sources: &'a dyn SourceRepository,
    store: &'a dyn ChunkStore,
    git: &'a dyn GitClient,
    locks: &'a dyn OperationLock,
    clock: &'a dyn Clock,
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
        }
    }

    pub fn execute(&self, request: &UpdateRequest) -> Result<UpdateResponse, AppError> {
        if let Some(name) = &request.name {
            let source = resolve_source(self.sources, name)?;
            let entry = self.update_one(source.id())?;
            return Ok(Self::response(vec![entry], None));
        }
        let all = self.sources.list()?;
        if all.is_empty() {
            return Ok(Self::response(Vec::new(), Some(NO_SOURCES_MESSAGE)));
        }
        let entries = all
            .iter()
            .map(|source| {
                self.update_one(source.id())
                    .unwrap_or_else(|error| Self::failed_entry(source, &error))
            })
            .collect();
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

    fn failed_entry(source: &Source, error: &AppError) -> UpdateEntry {
        UpdateEntry {
            repo: source.id().slug().to_owned(),
            name: source.name().to_owned(),
            result: "failed".to_owned(),
            previous_commit: source.indexed().map(|revision| revision.commit.to_string()),
            index: None,
            error: Some(error_body(error)),
        }
    }

    /// Updates one source under its operation lock.
    fn update_one(&self, id: &SourceId) -> Result<UpdateEntry, AppError> {
        let _lock = self.locks.acquire(id)?;
        // Read the source under the lock: it may have changed since it was listed.
        let source = self
            .sources
            .get(id)?
            .ok_or_else(|| AppError::NotFound(format!("no source {id}").into()))?;
        let indexer = Indexer::new(self.sources, self.store, self.git, self.clock);
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
            return Ok(entry(&indexed.source, "indexed", None, indexed.report));
        }
        let refreshed = indexer.refresh(source)?;
        let result = if refreshed.unchanged {
            "unchanged"
        } else {
            "updated"
        };
        Ok(entry(
            &refreshed.source,
            result,
            Some(&refreshed.previous),
            refreshed.report,
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::application::error::{ErrorCode, GitError, StorageError};
    use crate::application::indexer_test_support::{GitSpy, World, REMOTE};
    use crate::application::ports::{ChunkStore, IndexChanges, ReindexedChunk, SourceRepository};
    use crate::application::testkit::StampStoredVersions;
    use crate::domain::chunk::Chunk;
    use crate::domain::chunk_id::ChunkId;
    use crate::domain::index_versions::IndexVersions;
    use crate::domain::manifest::Manifest;
    use crate::domain::oid::CommitId;
    use crate::domain::source::{Source, SourceStatus};
    use crate::domain::source_id::SourceId;

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
            .execute(&UpdateRequest::named("example.com/o/r"))
            .unwrap_err();
        assert_eq!(error.code().exit_code(), 6);
        drop(held);
        assert!(w
            .update_source()
            .execute(&UpdateRequest::named("example.com/o/r"))
            .is_ok());
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

        assert_eq!(error, AppError::Storage(StorageError::ReindexRequired));
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
