//! Shared flows of the indexing use cases: first index (clone), refresh (fetch and
//! index what changed), and rebuild (reindex after a version change), plus name
//! resolution. The use cases take the `OperationLock` of a source around these; the
//! flows themselves never lock, so one use case can call another's flow while it
//! holds the lock (add of a registered location is a refresh).
//!
//! Failure policy: when a flow fails after the source row was touched, the failure is
//! recorded on the source (`Source::record_failure`: status `Failed`, plus the error
//! code, message, and time, which `rag list` shows) and saved. The stored index and
//! its revision are never changed by a failure (`commit_index` is the only writer and
//! is all-or-nothing), so a failed update keeps serving the old index. A `Busy` or
//! `Conflict` is not recorded and nothing is saved: another writer owns the source
//! (its status and its next write), and saving behind its back would only wait for
//! that writer's lock again. A refresh of an already indexed source writes nothing
//! before its work: the status stays what it was until the commit, so a crash leaves
//! no stale `Indexing`.
//!
//! A tracked branch that vanished from the remote (`NotFound` on fetch) is looked up
//! again once: when the remote names another default branch, the source follows it.

use std::time::Duration;

use crate::application::dto::ErrorBody;
use crate::application::error::{AppError, ErrorCode, GitError, Scrubbed, StorageError};
use crate::application::index_pipeline::{IndexPipeline, IndexReport, IndexScope};
use crate::application::ports::{
    ChunkStore, Clock, GitClient, ReindexedChunk, SourceRepository, TreeListing,
};
use crate::application::progress::{ensure_in_time, Phase, Progress, Timings, NO_PROGRESS};
use crate::application::search::{ambiguous_source, find_sources, unknown_source};
use crate::domain::git_ref::GitRef;
use crate::domain::index_versions::IndexVersions;
use crate::domain::oid::CommitId;
use crate::domain::source::Source;
use crate::domain::source_id::SourceId;

/// How long a same-source `add` or `update` waits for the operation lock before it
/// gives up with `index_locked`. `--no-wait` makes it zero.
pub const DEFAULT_LOCK_WAIT: Duration = Duration::from_secs(20);

/// Shown when update or reindex runs with no registered source.
pub const NO_SOURCES_MESSAGE: &str =
    "no sources are registered; run `rag add <url|path>` to index a repository";

/// The one source whose name, slug, or full id is `key`. Names compare
/// case-insensitively; a slug is matched exactly first and case-insensitively only
/// when that finds nothing (slugs are not case-folded, `github.com/O/R` and
/// `github.com/o/r` can be two sources). Several matches are a usage error that
/// lists the candidates; the name or the full id from `rag list` settles it.
pub fn resolve_source(sources: &dyn SourceRepository, key: &str) -> Result<Source, AppError> {
    let key = key.trim();
    let mut found = find_sources(sources, key)?;
    match found.len() {
        0 => Err(unknown_source(sources, key)),
        1 => Ok(found.remove(0)),
        _ => Err(ambiguous_source(key, &found)),
    }
}

/// The JSON error object for a failure that was recorded instead of returned.
pub fn error_body(error: &AppError) -> ErrorBody {
    ErrorBody::from_error(error)
}

/// Whether a failure only says "someone else is working on this": the operation lock
/// is held, the index changed under the writer, or git's own lock is taken. These are
/// retried later, never recorded as the source's failure.
pub fn is_contention(error: &AppError) -> bool {
    matches!(
        error,
        AppError::Storage(StorageError::Busy | StorageError::LockedBy(_) | StorageError::Conflict)
            | AppError::Git(GitError::Busy)
    )
}

/// The exit code of a command over several sources: 0 when no source really failed
/// (sources skipped as busy or removed do not count), the shared code when every
/// failure has the same error code, else 1.
pub fn exit_code_of_failures<'e>(failures: impl IntoIterator<Item = &'e ErrorBody>) -> u8 {
    let mut codes = failures.into_iter().map(|body| body.code.as_str());
    let Some(first) = codes.next() else {
        return 0;
    };
    if codes.all(|code| code == first) {
        ErrorCode::parse(first).map_or(1, |code| code.exit_code())
    } else {
        1
    }
}

/// A source after its first index.
pub struct Indexed {
    pub source: Source,
    pub report: IndexReport,
}

/// A source after a refresh.
pub struct Refreshed {
    pub source: Source,
    /// The commit the source was indexed at before.
    pub previous: CommitId,
    /// The branch had not moved: nothing was read or written.
    pub unchanged: bool,
    pub report: IndexReport,
}

/// What a rebuild had to do.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RebuildMode {
    /// The source has no indexed revision yet.
    NotIndexed,
    /// The index was made by the current rules.
    UpToDate,
    /// Only the tokenizer changed: the full-text fields were rewritten from the
    /// stored chunks.
    Tokenizer,
    /// Chunker or policy changed: files re-read at the indexed commit and re-chunked.
    Full,
    /// As `Full`, but the objects of the indexed commit were gone, so the branch was
    /// fetched and its latest commit indexed.
    Refetched,
}

impl RebuildMode {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::NotIndexed => "not_indexed",
            Self::UpToDate => "up_to_date",
            Self::Tokenizer => "tokenizer",
            Self::Full => "full",
            Self::Refetched => "refetched",
        }
    }
}

/// A source after a rebuild.
pub struct Rebuilt {
    pub source: Source,
    pub mode: RebuildMode,
    pub chunks: usize,
    pub report: Option<IndexReport>,
}

/// The flows over the ports of the indexing use cases.
pub struct Indexer<'a> {
    sources: &'a dyn SourceRepository,
    store: &'a dyn ChunkStore,
    git: &'a dyn GitClient,
    clock: &'a dyn Clock,
    progress: &'a dyn Progress,
}

impl<'a> Indexer<'a> {
    pub fn new(
        sources: &'a dyn SourceRepository,
        store: &'a dyn ChunkStore,
        git: &'a dyn GitClient,
        clock: &'a dyn Clock,
    ) -> Self {
        Self {
            sources,
            store,
            git,
            clock,
            progress: &NO_PROGRESS,
        }
    }

    /// Reports the phases of every flow to `progress`.
    pub fn with_progress(mut self, progress: &'a dyn Progress) -> Self {
        self.progress = progress;
        self
    }

    fn pipeline(&self) -> IndexPipeline<'a> {
        IndexPipeline::new(self.git, self.store, self.clock).with_progress(self.progress)
    }

    /// Runs `work`; if it fails, records the failure on the source (see the module
    /// documentation) and returns the error.
    fn guarded<T>(
        &self,
        source: &mut Source,
        work: impl FnOnce(&mut Source) -> Result<T, AppError>,
    ) -> Result<T, AppError> {
        let outcome = work(source);
        if let Err(error) = &outcome {
            if !is_contention(error) && !error.is_deadline() {
                source.record_failure(error.code().as_str(), &error.to_string(), self.clock.now());
                // The original error is the one worth reporting.
                let _ = self.sources.save(source);
            }
        }
        outcome
    }

    /// Lists the files of `commit`.
    fn list(
        &self,
        timings: &mut Timings,
        id: &SourceId,
        commit: &CommitId,
    ) -> Result<TreeListing, AppError> {
        ensure_in_time(self.progress)?;
        timings.run(self.progress, Phase::List, "listing files", || {
            let listing = self.git.ls_tree(id, commit)?;
            self.progress
                .detail(Phase::List, &format!("{} files", listing.entries.len()));
            Ok(listing)
        })
    }

    /// Gets the newest commit of the tracked branch with `fetch`. When the branch is
    /// gone from the remote and `recover` is set (the branch came from an earlier
    /// run, not from this one's own lookup), the remote's default branch is looked up
    /// once; if it is another branch the source follows it and `fetch` runs again.
    fn fetch_tracked(
        &self,
        timings: &mut Timings,
        source: &mut Source,
        recover: bool,
        fetch: impl Fn(&Source, &GitRef) -> Result<CommitId, GitError>,
    ) -> Result<CommitId, AppError> {
        ensure_in_time(self.progress)?;
        let tracked = source
            .git_ref()
            .cloned()
            .ok_or_else(|| StorageError::Failed("the source tracks no branch".into()))?;
        let attempt = timings.run(
            self.progress,
            Phase::Fetch,
            &format!("fetching {tracked}"),
            || fetch(source, &tracked),
        );
        match attempt {
            Err(GitError::NotFound(_)) if recover => {
                let detected = timings.run(
                    self.progress,
                    Phase::Resolve,
                    &format!("{tracked} is gone from the remote; looking up its default branch"),
                    || self.git.default_branch(source.location()),
                );
                match detected {
                    Ok(new_ref) if new_ref != tracked => {
                        source.track(&new_ref);
                        let retried = timings.run(
                            self.progress,
                            Phase::Fetch,
                            &format!("fetching {new_ref}"),
                            || fetch(source, &new_ref),
                        );
                        Ok(retried?)
                    }
                    _ => Err(GitError::NotFound(
                        format!(
                            "the tracked branch {tracked} no longer exists on the remote; \
                             ask the user to run `rag remove` and `rag add` for this repository again"
                        )
                        .into(),
                    )
                    .into()),
                }
            }
            other => Ok(other?),
        }
    }

    /// Registers `source` (a new one, or one that never finished indexing), clones
    /// its branch, and indexes everything the policy accepts.
    pub fn first_index(&self, mut source: Source) -> Result<Indexed, AppError> {
        ensure_in_time(self.progress)?;
        self.sources.save(&source)?;
        let mut timings = Timings::new();
        let mut report = self.guarded(&mut source, |source| {
            // A branch stored by an earlier, failed attempt may have vanished since.
            let recover = source.git_ref().is_some();
            if !recover {
                let git_ref = timings.run(
                    self.progress,
                    Phase::Resolve,
                    "resolving the default branch",
                    || self.git.default_branch(source.location()),
                )?;
                source.track(&git_ref);
            }
            let commit = self.fetch_tracked(&mut timings, source, recover, |source, git_ref| {
                self.git
                    .clone_shallow(source.id(), source.location(), git_ref)
            })?;
            source.mark_indexing();
            self.sources.save(source)?;
            let listing = self.list(&mut timings, source.id(), &commit)?;
            self.pipeline()
                .run(source, &commit, &listing, IndexScope::Changes, &mut timings)
        })?;
        report.phases = timings.into_vec();
        Ok(Indexed { source, report })
    }

    /// Fetches the tracked branch and indexes what changed since the indexed
    /// commit. `source` must have an indexed revision made by the current rules.
    pub fn refresh(&self, mut source: Source) -> Result<Refreshed, AppError> {
        let revision = source.indexed().cloned().ok_or_else(|| {
            AppError::Storage(StorageError::Failed(
                "the source has no indexed revision".into(),
            ))
        })?;
        if !revision.versions.is_current() {
            return Err(AppError::StaleIndex {
                source: Scrubbed::new(source.name()),
                also_stale: 0,
            });
        }
        if source.git_ref().is_none() {
            return Err(AppError::Storage(StorageError::Failed(
                "the source tracks no branch".into(),
            )));
        }
        let mut timings = Timings::new();
        // Nothing is written before the work (no `Indexing` status): the source stays
        // as it was until the commit, or until a real failure is recorded.
        let (unchanged, mut report) = self.guarded(&mut source, |source| {
            let latest = self.fetch_tracked(&mut timings, source, true, |source, git_ref| {
                self.git.fetch_update(source.id(), git_ref)
            })?;
            if latest == revision.commit {
                source.mark_ready(latest.clone(), revision.at);
                self.sources.save(source)?;
                return Ok((true, IndexReport::nothing_to_do(&latest)));
            }
            let listing = self.list(&mut timings, source.id(), &latest)?;
            let report = self.pipeline().run(
                source,
                &latest,
                &listing,
                IndexScope::Changes,
                &mut timings,
            )?;
            Ok((false, report))
        })?;
        report.phases = timings.into_vec();
        Ok(Refreshed {
            source,
            previous: revision.commit,
            unchanged,
            report,
        })
    }

    /// Brings an index made by older rules up to the current ones: rewrites the
    /// full-text fields when only the tokenizer changed, else re-reads and re-chunks
    /// every file (from the indexed commit if git still has it, else from a fresh
    /// fetch). Needs no network unless the old objects are gone.
    pub fn rebuild(&self, mut source: Source) -> Result<Rebuilt, AppError> {
        let id = source.id().clone();
        let Some(stored) = self.store.stored_versions(&id)? else {
            return Ok(Rebuilt {
                source,
                mode: RebuildMode::NotIndexed,
                chunks: 0,
                report: None,
            });
        };
        let current = IndexVersions::current();
        if stored == current {
            let chunks = self.store.chunk_count(&id)?;
            return Ok(Rebuilt {
                source,
                mode: RebuildMode::UpToDate,
                chunks,
                report: None,
            });
        }
        if stored.same_chunks_as(&current) {
            let fields: Vec<ReindexedChunk> = self
                .store
                .chunks(&id)?
                .iter()
                .map(|chunk| ReindexedChunk {
                    id: chunk.id.clone(),
                    fields: chunk.index_fields(),
                })
                .collect();
            self.store.rewrite_index_fields(&id, &fields, current)?;
            return Ok(Rebuilt {
                source,
                mode: RebuildMode::Tokenizer,
                chunks: fields.len(),
                report: None,
            });
        }

        let indexed_commit = source
            .indexed()
            .map(|revision| revision.commit.clone())
            .ok_or_else(|| {
                AppError::Storage(StorageError::Failed(
                    "the source has no indexed revision".into(),
                ))
            })?;
        let mut timings = Timings::new();
        let (mode, mut report) = self.guarded(&mut source, |source| {
            let at_indexed_commit = self
                .list(&mut timings, source.id(), &indexed_commit)
                .and_then(|listing| {
                    self.pipeline().run(
                        source,
                        &indexed_commit,
                        &listing,
                        IndexScope::Everything,
                        &mut timings,
                    )
                });
            match at_indexed_commit {
                Ok(report) => Ok((RebuildMode::Full, report)),
                Err(AppError::Git(GitError::NotFound(_))) => {
                    let latest =
                        self.fetch_tracked(&mut timings, source, true, |source, git_ref| {
                            self.git.fetch_update(source.id(), git_ref)
                        })?;
                    let listing = self.list(&mut timings, source.id(), &latest)?;
                    let report = self.pipeline().run(
                        source,
                        &latest,
                        &listing,
                        IndexScope::Everything,
                        &mut timings,
                    )?;
                    Ok((RebuildMode::Refetched, report))
                }
                Err(error) => Err(error),
            }
        })?;
        report.phases = timings.into_vec();
        let chunks = self.store.chunk_count(&id)?;
        Ok(Rebuilt {
            source,
            mode,
            chunks,
            report: Some(report),
        })
    }
}
#[cfg(test)]
mod tests {
    use super::resolve_source;
    use crate::application::error::ErrorCode;
    use crate::application::search::fixtures::index;
    use crate::application::testkit::InMemoryStore;

    fn two_servers_one_slug() -> InMemoryStore {
        let store = InMemoryStore::new();
        index(
            &store,
            "https://git.example.com/o/r",
            "plain",
            &[("a.rs", "x")],
        );
        index(
            &store,
            "https://git.example.com:8443/o/r",
            "on-port",
            &[("a.rs", "y")],
        );
        store
    }

    #[test]
    fn a_source_is_found_by_name_slug_or_id() {
        let store = InMemoryStore::new();
        let id = index(&store, "https://github.com/o/r", "Docs", &[("a.rs", "x")]);
        for key in ["docs", "DOCS", " Docs ", "github.com/o/r", id.as_str()] {
            let found = resolve_source(&store, key).unwrap();
            assert_eq!(found.id(), &id, "{key:?}");
        }
    }

    #[test]
    fn an_unknown_key_is_not_found_and_points_at_list() {
        let store = InMemoryStore::new();
        index(&store, "https://github.com/o/r", "Docs", &[("a.rs", "x")]);
        let error = resolve_source(&store, "nope").unwrap_err();
        assert_eq!(error.code(), ErrorCode::NotFound);
        assert!(error.to_string().contains("\"nope\""), "{error}");
        assert!(error.to_string().contains("rag list"), "{error}");
    }

    #[test]
    fn a_slug_shared_by_two_sources_is_a_usage_error_listing_the_candidates() {
        let store = two_servers_one_slug();
        let error = resolve_source(&store, "git.example.com/o/r").unwrap_err();
        assert_eq!(error.code(), ErrorCode::Usage);
        let shown = error.to_string();
        assert!(shown.contains("several sources"), "{shown}");
        assert!(
            shown.contains("plain") && shown.contains("on-port"),
            "{shown}"
        );
        assert!(
            shown.contains("git.example.com/o/r-"),
            "the ids are listed: {shown}"
        );
    }

    #[test]
    fn the_name_or_the_full_id_settles_an_ambiguous_slug() {
        let store = two_servers_one_slug();
        let plain = resolve_source(&store, "plain").unwrap();
        assert_eq!(plain.name(), "plain");
        let on_port = resolve_source(&store, "ON-PORT").unwrap();
        let by_id = resolve_source(&store, on_port.id().as_str()).unwrap();
        assert_eq!(by_id.name(), "on-port");
    }

    #[test]
    fn slugs_are_not_case_folded_when_an_exact_match_exists() {
        let store = InMemoryStore::new();
        index(&store, "https://github.com/O/R", "upper", &[("a.rs", "x")]);
        index(&store, "https://github.com/o/r", "lower", &[("a.rs", "y")]);
        assert_eq!(
            resolve_source(&store, "github.com/O/R").unwrap().name(),
            "upper"
        );
        assert_eq!(
            resolve_source(&store, "github.com/o/r").unwrap().name(),
            "lower"
        );
        let error = resolve_source(&store, "GITHUB.com/o/R").unwrap_err();
        assert_eq!(error.code(), ErrorCode::Usage, "{error}");
        assert!(error.to_string().contains("upper") && error.to_string().contains("lower"));
    }

    #[test]
    fn a_unique_owner_and_name_suffix_finds_the_source() {
        let store = InMemoryStore::new();
        let id = index(
            &store,
            "https://github.com/serde-rs/serde",
            "docs",
            &[("a.rs", "x")],
        );
        assert_eq!(resolve_source(&store, "serde-rs/serde").unwrap().id(), &id);
        assert_eq!(resolve_source(&store, "Serde-Rs/SERDE").unwrap().id(), &id);
    }

    #[test]
    fn a_unique_last_segment_finds_the_source() {
        let store = InMemoryStore::new();
        let id = index(
            &store,
            "https://github.com/serde-rs/serde",
            "docs",
            &[("a.rs", "x")],
        );
        assert_eq!(resolve_source(&store, "serde").unwrap().id(), &id);
    }

    #[test]
    fn a_suffix_must_start_on_a_path_segment_boundary() {
        let store = InMemoryStore::new();
        index(
            &store,
            "https://github.com/serde-rs/serde",
            "docs",
            &[("a.rs", "x")],
        );
        for key in ["rs/serde", "erde", "de-rs/serde", "github.com"] {
            let error = resolve_source(&store, key).unwrap_err();
            assert_eq!(error.code(), ErrorCode::NotFound, "{key:?}");
        }
    }

    #[test]
    fn an_ambiguous_suffix_is_a_usage_error_listing_the_candidates() {
        let store = InMemoryStore::new();
        index(
            &store,
            "https://github.com/a/serde",
            "one",
            &[("a.rs", "x")],
        );
        index(
            &store,
            "https://gitlab.com/b/serde",
            "two",
            &[("a.rs", "y")],
        );
        let error = resolve_source(&store, "serde").unwrap_err();
        assert_eq!(error.code(), ErrorCode::Usage);
        let shown = error.to_string();
        assert!(shown.contains("several sources"), "{shown}");
        assert!(shown.contains("one") && shown.contains("two"), "{shown}");
    }

    #[test]
    fn a_name_or_exact_slug_is_preferred_to_a_suffix_match() {
        let store = InMemoryStore::new();
        index(
            &store,
            "https://github.com/a/serde",
            "serde",
            &[("a.rs", "x")],
        );
        index(
            &store,
            "https://gitlab.com/b/serde",
            "other",
            &[("a.rs", "y")],
        );
        assert_eq!(resolve_source(&store, "serde").unwrap().name(), "serde");
        assert_eq!(
            resolve_source(&store, "gitlab.com/b/serde").unwrap().name(),
            "other"
        );
    }

    #[test]
    fn a_differently_cased_slug_still_finds_the_only_source() {
        let store = InMemoryStore::new();
        index(&store, "https://github.com/o/r", "docs", &[("a.rs", "x")]);
        assert_eq!(
            resolve_source(&store, "GitHub.com/O/R").unwrap().name(),
            "docs"
        );
    }
}

#[cfg(test)]
mod deadline_tests {
    use crate::application::add_source::AddRequest;
    use crate::application::error::{AppError, ErrorCode};
    use crate::application::indexer_test_support::{DeadlineAfter, GitSpy, World, REMOTE};
    use crate::application::ports::{ChunkStore, SourceRepository};
    use crate::application::progress::Phase;
    use crate::application::reindex::ReindexRequest;
    use crate::application::testkit::StampStoredVersions;
    use crate::application::update_source::UpdateRequest;
    use crate::domain::index_versions::IndexVersions;
    use crate::domain::source::SourceStatus;

    #[test]
    fn a_deadline_that_has_already_passed_stops_add_before_anything_is_done() {
        let w = World::new();
        w.commit(&[("a.rs", "fn a() {}\n")]);
        let spy = GitSpy::new(&w.git);
        let late = DeadlineAfter::already_passed();

        let error = w
            .add_source_via(&spy)
            .with_progress(&late)
            .execute(&AddRequest::new(REMOTE))
            .unwrap_err();

        assert_eq!(error, AppError::DeadlineExceeded);
        assert!(spy.calls().is_empty(), "{:?}", spy.calls());
        assert!(
            w.store.get(&w.id()).unwrap().is_none(),
            "nothing registered"
        );
    }

    #[test]
    fn a_deadline_after_the_clone_leaves_the_source_resumable_and_a_rerun_finishes_it() {
        let w = World::new();
        let head = w.commit(&[("a.rs", "fn a() {}\n")]);
        let late = DeadlineAfter::after(Phase::Fetch);

        let error = w
            .add_source_with(&late)
            .execute(&AddRequest::new(REMOTE))
            .unwrap_err();

        assert_eq!(error.code(), ErrorCode::DeadlineExceeded);
        let source = w.store.get(&w.id()).unwrap().unwrap();
        assert_ne!(source.status(), SourceStatus::Failed, "not a failure");
        assert!(
            source.last_error().is_none(),
            "no misleading error recorded"
        );
        assert!(source.indexed().is_none());

        let spy = GitSpy::new(&w.git);
        let response = w
            .add_source_via(&spy)
            .execute(&AddRequest::new(REMOTE))
            .unwrap();
        assert_eq!(response.status, "ready");
        assert_eq!(response.commit, head.to_string());
        assert!(
            !spy.called("default_branch"),
            "the branch found by the first run is reused"
        );
    }

    #[test]
    fn a_deadline_during_an_update_keeps_the_last_good_index_and_records_no_failure() {
        let w = World::new();
        let first = w.commit(&[("a.rs", "fn a() {}\n")]);
        w.add().unwrap();
        let second = w.commit(&[("a.rs", "fn a() {}\n"), ("b.rs", "fn b() {}\n")]);

        for stop_after in [Phase::Fetch, Phase::List, Phase::Read] {
            let late = DeadlineAfter::after(stop_after);
            let error = w
                .update_source()
                .with_progress(&late)
                .execute(&UpdateRequest::all())
                .unwrap_err();

            assert_eq!(error, AppError::DeadlineExceeded, "{stop_after:?}");
            let source = w.store.get(&w.id()).unwrap().unwrap();
            assert_eq!(source.status(), SourceStatus::Ready, "{stop_after:?}");
            assert_eq!(source.indexed().unwrap().commit, first, "{stop_after:?}");
            assert!(source.last_error().is_none(), "{stop_after:?}");
            assert_eq!(
                w.store.manifest(&w.id()).unwrap().len(),
                1,
                "{stop_after:?}"
            );
        }

        let response = w.update_source().execute(&UpdateRequest::all()).unwrap();
        assert_eq!(response.sources[0].result, "updated");
        assert_eq!(
            response.sources[0].index.as_ref().unwrap().commit,
            second.to_string()
        );
    }

    #[test]
    fn a_deadline_stops_update_of_a_named_source_too() {
        let w = World::new();
        w.commit(&[("a.rs", "fn a() {}\n")]);
        w.add().unwrap();
        w.commit(&[("b.rs", "fn b() {}\n")]);
        let late = DeadlineAfter::after(Phase::Fetch);

        let error = w
            .update_source()
            .with_progress(&late)
            .execute(&UpdateRequest::named("r"))
            .unwrap_err();

        assert_eq!(error.code(), ErrorCode::DeadlineExceeded);
    }

    #[test]
    fn a_deadline_during_a_reindex_leaves_the_stale_index_for_the_rerun() {
        let w = World::new();
        w.commit(&[("a.rs", "fn a() {}\n")]);
        w.add().unwrap();
        let mut old = IndexVersions::current();
        old.chunker = 0;
        w.store.stamp_stored_versions(&w.id(), old);
        let late = DeadlineAfter::after(Phase::List);

        let error = w
            .reindex()
            .with_progress(&late)
            .execute(&ReindexRequest::all())
            .unwrap_err();

        assert_eq!(error, AppError::DeadlineExceeded);
        assert_eq!(w.store.stored_versions(&w.id()).unwrap(), Some(old));
        let source = w.store.get(&w.id()).unwrap().unwrap();
        assert!(source.last_error().is_none());
        let response = w.reindex().execute(&ReindexRequest::all()).unwrap();
        assert_eq!(response.sources[0].result, "full");
    }
}
