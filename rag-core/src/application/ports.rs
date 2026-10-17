//! Ports: everything the use cases need from the outside world, in terms of domain
//! types only. Adapters live in `rag-infra`; in-memory fakes live in `testkit`, and
//! the `testkit` contract functions pin the behavior every adapter must have.
//!
//! Conventions:
//! - Every port is `Send + Sync` and takes `&self` (adapters use interior mutability
//!   or a connection per call).
//! - Errors are the per-family enums of `error`. Adapters hold no business rules:
//!   query fallback, hit caps, and skip rules are domain policy applied by the caller
//!   (`retrieval::retrieve`, `hit_policy::select_hits`, `IndexPolicy`).
//! - `SourceRepository` and `ChunkStore` are two views of one store (the SQLite
//!   adapter implements both over the same database), so a source row and its index
//!   change together in `ChunkStore::commit_index`.

use std::collections::BTreeMap;

use crate::application::error::{GitError, StorageError};
use crate::domain::chunk::{Chunk, IndexFields};
use crate::domain::chunk_id::ChunkId;
use crate::domain::git_location::GitLocation;
use crate::domain::git_ref::GitRef;
use crate::domain::index_versions::IndexVersions;
use crate::domain::indexed_at::IndexedAt;
use crate::domain::manifest::Manifest;
use crate::domain::oid::{BlobOid, CommitId};
use crate::domain::query::{Query, QueryMode};
use crate::domain::repo_path::RepoPath;
use crate::domain::scored_chunk::ScoredChunk;
use crate::domain::search_filters::SearchFilters;
use crate::domain::source::{IndexedRevision, Source};
use crate::domain::source_id::SourceId;
use crate::domain::tree_entry::TreeEntry;

/// The current time. The only source of "now" in the application.
pub trait Clock: Send + Sync {
    fn now(&self) -> IndexedAt;
}

/// The catalog of registered sources.
///
/// A source must be saved here before its first `ChunkStore::commit_index`. There
/// is no `delete`: a source leaves the catalog together with its index, through
/// `ChunkStore::remove_source`.
pub trait SourceRepository: Send + Sync {
    fn get(&self, id: &SourceId) -> Result<Option<Source>, StorageError>;

    /// The source whose name equals `name` when compared through
    /// `domain::source::name_key` (case-insensitively).
    fn find_by_name(&self, name: &str) -> Result<Option<Source>, StorageError>;

    /// All sources, ordered by id.
    fn list(&self) -> Result<Vec<Source>, StorageError>;

    /// Inserts the source or replaces the stored one with the same id, storing its
    /// name, location, ref, and status. Fails with `StorageError::NameTaken` when
    /// another source has the same name key.
    ///
    /// `save` never writes or changes the indexed revision (commit, time, index
    /// versions): only `ChunkStore::commit_index` and `rewrite_index_fields` do. A
    /// new source is stored without one; an existing source keeps its stored
    /// revision whatever `source.indexed()` says, and its index is not touched.
    /// Consequently a `Ready` source can only be saved over a stored revision;
    /// otherwise the save fails with `Failed` and changes nothing.
    fn save(&self, source: &Source) -> Result<(), StorageError>;
}

/// One file ready to be stored: its identity at the indexed commit and its chunks.
/// Every chunk must belong to the source being committed and to this path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IndexedFile {
    pub path: RepoPath,
    pub blob: BlobOid,
    /// Size of the file in bytes.
    pub size: u64,
    pub chunks: Vec<Chunk>,
}

/// A batch of index changes for one source, applied as one unit of work.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct IndexChanges {
    /// Files to add or replace. A replaced file loses all its previous chunks. A
    /// path may appear only once.
    pub upserts: Vec<IndexedFile>,
    /// Files to drop with their chunks. Paths that are not stored are ignored. A
    /// path may not also be upserted.
    pub removals: Vec<RepoPath>,
}

/// The re-tokenized full-text fields of one stored chunk (see `rewrite_index_fields`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReindexedChunk {
    pub id: ChunkId,
    pub fields: IndexFields,
}

/// Stores the index of each source: files, chunks, and their full-text fields.
pub trait ChunkStore: Send + Sync {
    /// Writes the source row (status, ref, indexed commit, time, and index
    /// versions, all taken from `source`) and applies `changes` to its files, chunks,
    /// and full-text index in one transaction. After any `Err` nothing has changed.
    ///
    /// - The source must already be registered (`SourceRepository::save`) and must
    ///   have an indexed revision; otherwise `Failed`.
    /// - `expected_base` is the commit the caller read the index at (`None` for a
    ///   first index). It must equal the stored indexed commit, else `Conflict`. The
    ///   check and the write are one atomic step: of two concurrent commits from the
    ///   same base exactly one succeeds and the other gets `Conflict`.
    /// - When `source`'s index versions differ from the stored ones (a reindex),
    ///   `changes` must cover every stored file, each path in `upserts` or in
    ///   `removals`; otherwise `Failed`. A partial change set would leave chunks
    ///   from two versions in one index. The first index of a source carries any
    ///   versions.
    /// - The tokenizer and policy versions of `source`'s revision must be the current
    ///   ones (`IndexVersions::current()`): the adapter stores fields the current
    ///   domain computed, so it cannot stamp them as made by other rules. The
    ///   chunker version is stored as given and is covered by the rule above. Any
    ///   other value is `Failed`.
    /// - A name clash with another source gives `NameTaken`.
    /// - Changes naming a chunk of another source or path, the same path twice among
    ///   the upserts, or a path both upserted and removed are rejected with `Failed`.
    /// - Chunk ids are unique in the store: a chunk id that appears twice in
    ///   `changes`, or that a stored file which stays also has, is `Failed`. A file
    ///   that is replaced may reuse the ids of the chunks it replaces.
    /// - The adapter stores `Chunk::index_fields()` verbatim and never tokenizes on
    ///   its own; heading paths are stored as lists, not joined text.
    fn commit_index(
        &self,
        source: &Source,
        expected_base: Option<&CommitId>,
        changes: &IndexChanges,
    ) -> Result<(), StorageError>;

    /// Drops the source's files, chunks, and full-text rows together with its row in
    /// the catalog, in one transaction. Unknown sources are a no-op.
    fn remove_source(&self, id: &SourceId) -> Result<(), StorageError>;

    /// The stored files as path to blob. Empty for an unknown source.
    ///
    /// This is the one authority on which paths a source has: `rag show <repo>
    /// <path>` resolves the path here (path to blob), then reads the blob with
    /// `GitClient::read_blobs`. It never lists the tree with `GitClient::ls_tree`,
    /// so `show` serves exactly what was indexed, even while an update is running.
    fn manifest(&self, source: &SourceId) -> Result<Manifest, StorageError>;

    /// Number of stored chunks. Zero for an unknown source.
    fn chunk_count(&self, source: &SourceId) -> Result<usize, StorageError>;

    /// The stored chunk with this id, exactly as committed, or `None` when no such
    /// chunk is stored (never indexed, replaced or removed with its file, or its
    /// source removed). Backs `rag show --chunk <chunk_id>`. Works on a stale index.
    fn chunk(&self, id: &ChunkId) -> Result<Option<Chunk>, StorageError>;

    /// Every stored chunk of the source in document order (by path, then first
    /// line), exactly as committed. Works whatever the stored index versions are, so
    /// `rag reindex` can read a stale index. Empty for an unknown source.
    fn chunks(&self, source: &SourceId) -> Result<Vec<Chunk>, StorageError>;

    /// The tokenizer and chunker versions stored with the source's indexed revision;
    /// `None` when the source is unknown or has no indexed revision. Works on a
    /// stale index.
    fn stored_versions(&self, source: &SourceId) -> Result<Option<IndexVersions>, StorageError>;

    /// Replaces the full-text fields of every stored chunk of the source and stamps
    /// `versions` on its indexed revision, in one transaction (tokenizer-only
    /// reindex; chunk rows are untouched). `fields` must name exactly the stored
    /// chunks, each once; otherwise, or when the source has no indexed revision, or
    /// when `versions` changes the chunker or policy version (those need
    /// `commit_index` with every file re-chunked), or when `versions` is not the
    /// current tokenizer and policy (`IndexVersions::current()`: the fields were
    /// computed by this build), `Failed` and nothing changes. Works on a stale
    /// index; its job is to bring the tokenizer version up to the current one.
    fn rewrite_index_fields(
        &self,
        source: &SourceId,
        fields: &[ReindexedChunk],
        versions: IndexVersions,
    ) -> Result<(), StorageError>;
}

/// The result of one search, read in one transaction.
#[derive(Debug, Clone, PartialEq)]
pub struct SearchOutcome {
    /// Matching chunks, best first in `ScoredChunk::ranking_order`, at most `limit`.
    pub hits: Vec<ScoredChunk>,
    /// The indexed revision of every source that appears in `hits`, as read in the
    /// same transaction as the hits.
    pub revisions: BTreeMap<SourceId, IndexedRevision>,
    /// How many chunks match in all, before `limit` (and before the per-file cap,
    /// which the caller applies), counted only up to `TOTAL_MATCHES_CAP`. Read it
    /// together with `total_is_exact`: when that is false, the real count is larger
    /// and this value is `TOTAL_MATCHES_CAP`, a lower bound ("at least").
    pub total_matches: usize,
    /// Whether `total_matches` is the exact count (it did not reach the cap). A
    /// searcher counts at most `TOTAL_MATCHES_CAP + 1` matches, so the cost of a
    /// search stays bounded however common the query terms are.
    pub total_is_exact: bool,
}

/// The most matches a search counts. A larger number of matches is reported as this
/// value with `SearchOutcome::total_is_exact` false.
pub const TOTAL_MATCHES_CAP: usize = 1000;

/// Full-text search over stored chunks.
pub trait ChunkSearcher: Send + Sync {
    /// Chunks matching `query` under `mode` (`And`: every word; `OrFallback`: any
    /// word) that satisfy `filters`. Filters apply before `limit`. Only searchable
    /// sources are searched (`Source::is_searchable`: an indexed revision, and not
    /// `Removing`).
    ///
    /// A word matches a chunk when the chunk contains its joined form, or every
    /// searchable part of it (`parseConfig` finds `parse_config`); the parts may be
    /// spread over path, heading, and body. Matching is case-insensitive and does not
    /// fold diacritics. Scores are higher for better matches (the adapter negates
    /// bm25) and comparable within one search only. The only ordering guarantees are
    /// determinism and the tie-break of `ScoredChunk::ranking_order`.
    ///
    /// The cost of a search is that of ranking every match, whatever `limit` is; the
    /// match count is capped (`TOTAL_MATCHES_CAP`, see `SearchOutcome`).
    ///
    /// Fails with `ReindexRequired` when a source the query touches (one of
    /// `filters.sources`, or any searchable source when that list is empty) was
    /// indexed with other tokenizer or chunker versions.
    fn search(
        &self,
        query: &Query,
        mode: QueryMode,
        filters: &SearchFilters,
        limit: usize,
    ) -> Result<SearchOutcome, StorageError>;
}

/// The files of a commit and how many paths were left out as unsafe.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct TreeListing {
    /// Regular files with a safe path, ordered by path.
    pub entries: Vec<TreeEntry>,
    /// Paths that failed `RepoPath::from_git` and were skipped.
    pub skipped_unsafe: usize,
}

/// The largest blob `GitClient::read_blobs` reads. Larger ones fail with
/// `GitError::TooLarge`. Twice the size cap of the index policy (1 MiB), so every
/// file the policy accepts can be read.
pub const MAX_BLOB_BYTES: u64 = 2 * 1024 * 1024;

/// The most content one `GitClient::read_blobs` call reads (the sum of the blob
/// sizes). A larger request fails with `GitError::TooLarge`; callers slice.
pub const MAX_READ_BYTES: u64 = 32 * 1024 * 1024;

/// Operations on the local object database of each source, by the system `git`.
/// The adapter decides where it lives (a directory named by
/// `SourceId::checkout_dir_name`); callers address it by `SourceId`.
///
/// Clones are `--no-checkout`: the directory holds git objects only, with no working
/// tree. Files are read from blobs (`read_blobs`), never from disk, which is also
/// why there is no browsable clone path to list. Subprocesses run with
/// `GIT_TERMINAL_PROMPT=0` and `LC_ALL=C`, and error messages never contain
/// credentials.
pub trait GitClient: Send + Sync {
    /// The default branch of the remote at `location`.
    fn default_branch(&self, location: &GitLocation) -> Result<GitRef, GitError>;

    /// Shallow, single-branch, no-checkout clone of `git_ref` into the source's
    /// directory. Returns the commit `git_ref` points to. Idempotent: an existing or
    /// half-cloned directory is reused or replaced, so a retry after a crash works.
    fn clone_shallow(
        &self,
        source: &SourceId,
        location: &GitLocation,
        git_ref: &GitRef,
    ) -> Result<CommitId, GitError>;

    /// Shallow fetch of `git_ref`. Returns the new commit. Earlier commits stay
    /// readable until the next fetch.
    fn fetch_update(&self, source: &SourceId, git_ref: &GitRef) -> Result<CommitId, GitError>;

    /// The regular files of `commit`, ordered by path. Paths unsafe for `RepoPath`
    /// are counted, not listed; submodules and symlinks are not listed. Used to
    /// decide what to index; `show` never uses it (it resolves paths through
    /// `ChunkStore::manifest`).
    fn ls_tree(&self, source: &SourceId, commit: &CommitId) -> Result<TreeListing, GitError>;

    /// The bytes of each requested blob, keyed by blob id (duplicates collapse).
    /// Fails with `NotFound` when any blob is missing from the source, with
    /// `Failed` when an id is not a blob, and with `TooLarge` when a blob is larger
    /// than `MAX_BLOB_BYTES` or the blobs together are larger than `MAX_READ_BYTES`.
    /// (The index policy skips files above its own, smaller cap before they get
    /// here, so a `TooLarge` blob means the caller asked for one it should not.)
    ///
    /// The whole result is held in memory, so **callers must slice their
    /// requests**: ask for blobs whose sizes (known from `ls_tree`) add up to at
    /// most `MAX_READ_BYTES` per call, and call again for the rest.
    fn read_blobs(
        &self,
        source: &SourceId,
        blobs: &[BlobOid],
    ) -> Result<BTreeMap<BlobOid, Vec<u8>>, GitError>;

    /// Deletes the source's directory. An absent directory is a no-op.
    fn remove_checkout(&self, source: &SourceId) -> Result<(), GitError>;
}

/// Proof that the operation lock of one source is held. The lock is released when
/// the guard is dropped.
pub struct LockGuard {
    _release: Box<dyn Send>,
}

impl LockGuard {
    /// Wraps whatever releases the lock when dropped (an open lock file, a registry
    /// entry).
    pub fn new(release_on_drop: impl Send + 'static) -> Self {
        Self {
            _release: Box::new(release_on_drop),
        }
    }
}

impl std::fmt::Debug for LockGuard {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("LockGuard")
    }
}

/// Serializes whole operations (add, update, remove, reindex) on one source across
/// processes. A use case holds the guard from its first read to its last write;
/// searches never take it.
pub trait OperationLock: Send + Sync {
    /// Takes the lock of `source`. Contention gives `StorageError::Busy`.
    fn acquire(&self, source: &SourceId) -> Result<LockGuard, StorageError>;
}
