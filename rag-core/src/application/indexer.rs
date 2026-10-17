//! Shared flows of the indexing use cases: first index (clone), refresh (fetch and
//! index what changed), and rebuild (reindex after a version change), plus name
//! resolution. The use cases take the `OperationLock` of a source around these; the
//! flows themselves never lock, so one use case can call another's flow while it
//! holds the lock (add of a registered location is a refresh).
//!
//! Failure policy: when a flow fails after the source row was touched, the source is
//! marked `Failed` and saved. The stored index and its revision are never changed by
//! a failure (`commit_index` is the only writer and is all-or-nothing), so a failed
//! update keeps serving the old index. A `Conflict` is not marked: the other writer
//! owns the source's status.

use crate::application::dto::ErrorBody;
use crate::application::error::{AppError, GitError, StorageError};
use crate::application::index_pipeline::{IndexPipeline, IndexReport, IndexScope};
use crate::application::ports::{ChunkStore, Clock, GitClient, ReindexedChunk, SourceRepository};
use crate::domain::index_versions::IndexVersions;
use crate::domain::oid::CommitId;
use crate::domain::source::Source;

/// Shown when update or reindex runs with no registered source.
pub const NO_SOURCES_MESSAGE: &str =
    "no sources are registered; run `rag add <url|path>` to index a repository";

/// The source whose name or slug is `key`.
pub fn resolve_source(sources: &dyn SourceRepository, key: &str) -> Result<Source, AppError> {
    let key = key.trim();
    if let Some(source) = sources.find_by_name(key)? {
        return Ok(source);
    }
    sources
        .list()?
        .into_iter()
        .find(|source| source.id().slug() == key || source.id().as_str() == key)
        .ok_or_else(|| AppError::NotFound(format!("no source named {key:?}").into()))
}

/// The JSON error object for a failure that was recorded instead of returned.
pub fn error_body(error: &AppError) -> ErrorBody {
    let mut body = ErrorBody::from(error.code());
    body.message = error.to_string();
    body
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
        }
    }

    fn pipeline(&self) -> IndexPipeline<'a> {
        IndexPipeline::new(self.git, self.store, self.clock)
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
            if !matches!(error, AppError::Storage(StorageError::Conflict)) {
                source.mark_failed();
                // The original error is the one worth reporting.
                let _ = self.sources.save(source);
            }
        }
        outcome
    }

    /// Registers `source` (a new one, or one that never finished indexing), clones
    /// its branch, and indexes everything the policy accepts.
    pub fn first_index(&self, mut source: Source) -> Result<Indexed, AppError> {
        self.sources.save(&source)?;
        let report = self.guarded(&mut source, |source| {
            let git_ref = match source.git_ref() {
                Some(git_ref) => git_ref.clone(),
                None => self.git.default_branch(source.location())?,
            };
            source.track(&git_ref);
            let commit = self
                .git
                .clone_shallow(source.id(), source.location(), &git_ref)?;
            source.mark_indexing();
            self.sources.save(source)?;
            let listing = self.git.ls_tree(source.id(), &commit)?;
            self.pipeline()
                .run(source, &commit, &listing, IndexScope::Changes)
        })?;
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
            return Err(StorageError::ReindexRequired.into());
        }
        let git_ref = source.git_ref().cloned().ok_or_else(|| {
            AppError::Storage(StorageError::Failed("the source tracks no branch".into()))
        })?;
        let (unchanged, report) = self.guarded(&mut source, |source| {
            source.mark_indexing();
            self.sources.save(source)?;
            let latest = self.git.fetch_update(source.id(), &git_ref)?;
            if latest == revision.commit {
                source.mark_ready(latest.clone(), revision.at);
                self.sources.save(source)?;
                return Ok((true, IndexReport::nothing_to_do(&latest)));
            }
            let listing = self.git.ls_tree(source.id(), &latest)?;
            let report = self
                .pipeline()
                .run(source, &latest, &listing, IndexScope::Changes)?;
            Ok((false, report))
        })?;
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
        let (mode, report) = self.guarded(&mut source, |source| {
            let at_indexed_commit = self
                .git
                .ls_tree(source.id(), &indexed_commit)
                .map_err(AppError::from)
                .and_then(|listing| {
                    self.pipeline()
                        .run(source, &indexed_commit, &listing, IndexScope::Everything)
                });
            match at_indexed_commit {
                Ok(report) => Ok((RebuildMode::Full, report)),
                Err(AppError::Git(GitError::NotFound(_))) => {
                    let git_ref = source.git_ref().cloned().ok_or_else(|| {
                        AppError::Storage(StorageError::Failed(
                            "the source tracks no branch".into(),
                        ))
                    })?;
                    let latest = self.git.fetch_update(source.id(), &git_ref)?;
                    let listing = self.git.ls_tree(source.id(), &latest)?;
                    let report =
                        self.pipeline()
                            .run(source, &latest, &listing, IndexScope::Everything)?;
                    Ok((RebuildMode::Refetched, report))
                }
                Err(error) => Err(error),
            }
        })?;
        let chunks = self.store.chunk_count(&id)?;
        Ok(Rebuilt {
            source,
            mode,
            chunks,
            report: Some(report),
        })
    }
}
