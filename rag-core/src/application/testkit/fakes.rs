//! In-memory fakes of every port. They behave like the real adapters (including
//! atomic `commit_index`, the documented search semantics, and a no-checkout git
//! object store) and can inject failures. The contract functions in `contracts`
//! pin what "behave like the real adapters" means.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

use super::contracts::StampStoredVersions;
use crate::application::error::{GitError, StorageError};
use crate::application::ports::{
    ChunkSearcher, ChunkStore, Clock, GitClient, IndexChanges, LockGuard, OperationLock,
    ReindexedChunk, SearchOutcome, SourceRepository, TreeListing, MAX_BLOB_BYTES, MAX_READ_BYTES,
    TOTAL_MATCHES_CAP,
};
use crate::domain::chunk::{Chunk, IndexFields};
use crate::domain::chunk_id::ChunkId;
use crate::domain::git_location::GitLocation;
use crate::domain::git_ref::GitRef;
use crate::domain::identifier_splitter::Word;
use crate::domain::index_versions::IndexVersions;
use crate::domain::indexed_at::IndexedAt;
use crate::domain::manifest::Manifest;
use crate::domain::oid::{BlobOid, CommitId};
use crate::domain::path_prior::path_prior;
use crate::domain::query::{Query, QueryMode};
use crate::domain::repo_path::RepoPath;
use crate::domain::scored_chunk::ScoredChunk;
use crate::domain::search_filters::SearchFilters;
use crate::domain::source::{name_key, IndexedRevision, Source};
use crate::domain::source_id::SourceId;
use crate::domain::tree_entry::TreeEntry;

fn locked<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex.lock().expect("fake state lock is not poisoned")
}

// --- Clock ------------------------------------------------------------------

/// A clock that only moves when told to.
#[derive(Debug)]
pub struct FixedClock(AtomicI64);

impl FixedClock {
    pub fn new(now: IndexedAt) -> Self {
        Self(AtomicI64::new(now.epoch_seconds()))
    }

    pub fn set(&self, now: IndexedAt) {
        self.0.store(now.epoch_seconds(), Ordering::SeqCst);
    }

    pub fn advance(&self, seconds: i64) {
        self.0.fetch_add(seconds, Ordering::SeqCst);
    }
}

impl Clock for FixedClock {
    fn now(&self) -> IndexedAt {
        IndexedAt::from_epoch_seconds(self.0.load(Ordering::SeqCst))
    }
}

// --- SourceRepository + ChunkStore + ChunkSearcher -----------------------------

#[derive(Debug, Clone)]
struct StoredFile {
    blob: BlobOid,
    /// Each chunk with the tokenized fields the store would index for it.
    chunks: Vec<(Chunk, IndexFields)>,
}

type SourceFiles = BTreeMap<RepoPath, StoredFile>;

#[derive(Debug, Default)]
struct StoreState {
    sources: BTreeMap<SourceId, Source>,
    files: BTreeMap<SourceId, SourceFiles>,
    fail_next_commit: bool,
}

/// The catalog, the chunk store, and the searcher over one shared in-memory state,
/// like the SQLite adapter over one database.
#[derive(Debug, Default)]
pub struct InMemoryStore {
    state: Mutex<StoreState>,
}

impl InMemoryStore {
    pub fn new() -> Self {
        Self::default()
    }

    /// Makes the next `commit_index` fail without changing anything.
    pub fn fail_next_commit(&self) {
        locked(&self.state).fail_next_commit = true;
    }
}

fn failed(message: impl Into<String>) -> StorageError {
    StorageError::Failed(message.into().into())
}

/// The index is built by this build's domain, so it may not claim other tokenizer or
/// policy versions.
fn ensure_current_tokenizer_and_policy(versions: IndexVersions) -> Result<(), StorageError> {
    if !versions.stamps_current_tokenizer_and_policy() {
        return Err(failed(format!(
            "an index is stamped with the current tokenizer and policy versions only, \
             not {versions:?}"
        )));
    }
    Ok(())
}

/// Fails when a source other than `id` already has `name`.
fn check_name_free(
    sources: &BTreeMap<SourceId, Source>,
    id: &SourceId,
    name: &str,
) -> Result<(), StorageError> {
    let key = name_key(name);
    if sources
        .values()
        .any(|other| other.id() != id && other.name_key() == key)
    {
        return Err(StorageError::NameTaken(name.into()));
    }
    Ok(())
}

impl SourceRepository for InMemoryStore {
    fn get(&self, id: &SourceId) -> Result<Option<Source>, StorageError> {
        Ok(locked(&self.state).sources.get(id).cloned())
    }

    fn find_by_name(&self, name: &str) -> Result<Option<Source>, StorageError> {
        let key = name_key(name);
        Ok(locked(&self.state)
            .sources
            .values()
            .find(|source| source.name_key() == key)
            .cloned())
    }

    fn list(&self) -> Result<Vec<Source>, StorageError> {
        Ok(locked(&self.state).sources.values().cloned().collect())
    }

    fn save(&self, source: &Source) -> Result<(), StorageError> {
        let mut state = locked(&self.state);
        check_name_free(&state.sources, source.id(), source.name())?;
        // Only `commit_index` and `rewrite_index_fields` write a revision: keep the
        // stored one (none for a new source), whatever the caller's copy carries.
        let stored_revision = state
            .sources
            .get(source.id())
            .and_then(|stored| stored.indexed().cloned());
        let kept = Source::restore(
            source.id().clone(),
            source.name().to_owned(),
            source.location().clone(),
            source.git_ref().cloned(),
            source.status(),
            stored_revision,
        )
        .map_err(|e| failed(e.to_string()))?
        .with_last_error(source.last_error().cloned());
        state.sources.insert(source.id().clone(), kept);
        Ok(())
    }
}

impl ChunkStore for InMemoryStore {
    fn commit_index(
        &self,
        source: &Source,
        expected_base: Option<&CommitId>,
        changes: &IndexChanges,
    ) -> Result<(), StorageError> {
        let mut state = locked(&self.state);
        if std::mem::take(&mut state.fail_next_commit) {
            return Err(failed("injected commit failure"));
        }
        let stored = state
            .sources
            .get(source.id())
            .ok_or_else(|| failed("the source is not registered"))?;
        if source.indexed().is_none() {
            return Err(failed("the source has no indexed revision to commit"));
        }
        if stored.indexed().map(|revision| &revision.commit) != expected_base {
            return Err(StorageError::Conflict);
        }
        check_name_free(&state.sources, source.id(), source.name())?;

        let mut files = state.files.get(source.id()).cloned().unwrap_or_default();
        let stored_versions = stored.indexed().map(|revision| revision.versions);
        let new_versions = source.indexed().map(|revision| revision.versions);
        if let Some(versions) = new_versions {
            ensure_current_tokenizer_and_policy(versions)?;
        }
        if stored_versions.is_some() && stored_versions != new_versions {
            let covered: BTreeSet<&RepoPath> = changes
                .upserts
                .iter()
                .map(|file| &file.path)
                .chain(&changes.removals)
                .collect();
            if let Some(uncovered) = files.keys().find(|path| !covered.contains(path)) {
                return Err(failed(format!(
                    "a version change must rewrite every stored file, but {uncovered} is not covered"
                )));
            }
        }
        let mut upserted = BTreeSet::new();
        for file in &changes.upserts {
            if !upserted.insert(&file.path) {
                return Err(failed(format!("{} is upserted twice", file.path)));
            }
            if file
                .chunks
                .iter()
                .any(|c| &c.source != source.id() || c.path != file.path)
            {
                return Err(failed(format!(
                    "a chunk of {} belongs to another source or path",
                    file.path
                )));
            }
        }
        for path in &changes.removals {
            if upserted.contains(path) {
                return Err(failed(format!("{path} is both removed and upserted")));
            }
            files.remove(path);
        }
        for path in &upserted {
            files.remove(*path);
        }
        // Chunk ids are unique across the whole store, so the new chunks may not
        // repeat each other or a chunk of a file that stays.
        let mut ids: BTreeSet<&ChunkId> = state
            .files
            .iter()
            .filter(|(id, _)| *id != source.id())
            .flat_map(|(_, other)| other.values())
            .chain(files.values())
            .flat_map(|file| file.chunks.iter().map(|(chunk, _)| &chunk.id))
            .collect();
        for chunk in changes.upserts.iter().flat_map(|file| &file.chunks) {
            if !ids.insert(&chunk.id) {
                return Err(failed(format!("chunk id {} is stored twice", chunk.id)));
            }
        }
        for file in &changes.upserts {
            files.insert(
                file.path.clone(),
                StoredFile {
                    blob: file.blob.clone(),
                    chunks: file
                        .chunks
                        .iter()
                        .map(|chunk| (chunk.clone(), chunk.index_fields()))
                        .collect(),
                },
            );
        }
        state.files.insert(source.id().clone(), files);
        state.sources.insert(source.id().clone(), source.clone());
        Ok(())
    }

    fn remove_source(&self, id: &SourceId) -> Result<(), StorageError> {
        let mut state = locked(&self.state);
        state.files.remove(id);
        state.sources.remove(id);
        Ok(())
    }

    fn manifest(&self, source: &SourceId) -> Result<Manifest, StorageError> {
        Ok(locked(&self.state)
            .files
            .get(source)
            .map(|files| {
                files
                    .iter()
                    .map(|(p, f)| (p.clone(), f.blob.clone()))
                    .collect()
            })
            .unwrap_or_default())
    }

    fn chunk_count(&self, source: &SourceId) -> Result<usize, StorageError> {
        Ok(locked(&self.state)
            .files
            .get(source)
            .map_or(0, |files| files.values().map(|f| f.chunks.len()).sum()))
    }

    fn chunk_count_for_paths(
        &self,
        source: &SourceId,
        paths: &[RepoPath],
    ) -> Result<usize, StorageError> {
        let wanted: BTreeSet<&RepoPath> = paths.iter().collect();
        Ok(locked(&self.state).files.get(source).map_or(0, |files| {
            files
                .iter()
                .filter(|(path, _)| wanted.contains(path))
                .map(|(_, file)| file.chunks.len())
                .sum()
        }))
    }

    fn chunk(&self, id: &ChunkId) -> Result<Option<Chunk>, StorageError> {
        Ok(locked(&self.state)
            .files
            .values()
            .flat_map(|files| files.values())
            .flat_map(|file| file.chunks.iter())
            .find(|(chunk, _)| &chunk.id == id)
            .map(|(chunk, _)| chunk.clone()))
    }

    fn chunks(&self, source: &SourceId) -> Result<Vec<Chunk>, StorageError> {
        Ok(locked(&self.state)
            .files
            .get(source)
            .map(|files| {
                files
                    .values()
                    .flat_map(|file| file.chunks.iter().map(|(chunk, _)| chunk.clone()))
                    .collect()
            })
            .unwrap_or_default())
    }

    fn stored_versions(&self, source: &SourceId) -> Result<Option<IndexVersions>, StorageError> {
        Ok(locked(&self.state)
            .sources
            .get(source)
            .and_then(|stored| stored.indexed())
            .map(|revision| revision.versions))
    }

    fn rewrite_index_fields(
        &self,
        source: &SourceId,
        fields: &[ReindexedChunk],
        versions: IndexVersions,
    ) -> Result<(), StorageError> {
        let mut state = locked(&self.state);
        let stored = state
            .sources
            .get(source)
            .ok_or_else(|| failed("the source is not registered"))?;
        let revision = stored
            .indexed()
            .cloned()
            .ok_or_else(|| failed("the source has no indexed revision"))?;
        if !versions.same_chunks_as(&revision.versions) {
            return Err(failed(
                "a rewrite may only change the tokenizer version; re-chunk with commit_index",
            ));
        }
        ensure_current_tokenizer_and_policy(versions)?;

        let mut replacement: BTreeMap<&ChunkId, &IndexFields> = BTreeMap::new();
        for entry in fields {
            if replacement.insert(&entry.id, &entry.fields).is_some() {
                return Err(failed(format!(
                    "chunk {} is listed twice",
                    entry.id.as_str()
                )));
            }
        }
        let mut files = state.files.get(source).cloned().unwrap_or_default();
        let stored_ids: BTreeSet<&ChunkId> = files
            .values()
            .flat_map(|file| file.chunks.iter().map(|(chunk, _)| &chunk.id))
            .collect();
        if stored_ids != replacement.keys().copied().collect() {
            return Err(failed("fields must be given for exactly the stored chunks"));
        }
        for file in files.values_mut() {
            for (chunk, indexed) in &mut file.chunks {
                if let Some(new_fields) = replacement.get(&chunk.id) {
                    *indexed = (*new_fields).clone();
                }
            }
        }
        let updated = Source::restore(
            stored.id().clone(),
            stored.name().to_owned(),
            stored.location().clone(),
            stored.git_ref().cloned(),
            stored.status(),
            Some(IndexedRevision {
                versions,
                ..revision
            }),
        )
        .map_err(|e| failed(e.to_string()))?
        .with_last_error(stored.last_error().cloned());
        state.files.insert(source.clone(), files);
        state.sources.insert(source.clone(), updated);
        Ok(())
    }
}

impl StampStoredVersions for InMemoryStore {
    fn stamp_stored_versions(&self, source: &SourceId, versions: IndexVersions) {
        let mut state = locked(&self.state);
        let stored = state.sources.get(source).expect("a registered source");
        let revision = stored.indexed().cloned().expect("an indexed source");
        let aged = Source::restore(
            stored.id().clone(),
            stored.name().to_owned(),
            stored.location().clone(),
            stored.git_ref().cloned(),
            stored.status(),
            Some(IndexedRevision {
                versions,
                ..revision
            }),
        )
        .expect("a restorable source")
        .with_last_error(stored.last_error().cloned());
        state.sources.insert(source.clone(), aged);
    }
}

impl ChunkSearcher for InMemoryStore {
    fn search(
        &self,
        query: &Query,
        mode: QueryMode,
        filters: &SearchFilters,
        limit: usize,
    ) -> Result<SearchOutcome, StorageError> {
        let state = locked(&self.state);
        let searched: Vec<&Source> = state
            .sources
            .values()
            .filter(|source| source.is_searchable())
            .filter(|source| filters.sources.is_empty() || filters.sources.contains(source.id()))
            .collect();
        if searched
            .iter()
            .filter_map(|source| source.indexed())
            .any(|revision| !revision.versions.is_current())
        {
            return Err(StorageError::ReindexRequired);
        }

        let mut hits: Vec<ScoredChunk> = searched
            .iter()
            .filter_map(|source| state.files.get(source.id()))
            .flat_map(|files| files.values())
            .flat_map(|file| file.chunks.iter())
            .filter(|(chunk, _)| filters.matches(chunk))
            .filter_map(|(chunk, fields)| {
                score(query, mode, fields).map(|score| ScoredChunk {
                    chunk: chunk.clone(),
                    score: score * path_prior(&chunk.path),
                })
            })
            .collect();
        hits.sort_by(ScoredChunk::ranking_order);
        let total_matches = hits.len().min(TOTAL_MATCHES_CAP);
        let total_is_exact = hits.len() <= TOTAL_MATCHES_CAP;
        hits.truncate(limit);

        let revisions = hits
            .iter()
            .filter_map(|hit| {
                let source = state.sources.get(&hit.chunk.source)?;
                Some((hit.chunk.source.clone(), source.indexed()?.clone()))
            })
            .collect();
        Ok(SearchOutcome {
            hits,
            revisions,
            total_matches,
            total_is_exact,
        })
    }
}

/// Weighted occurrences of `word` over the three columns: its joined form, else
/// (for multi-part words) the rarest searchable part when every part is present
/// somewhere. Path matches weigh 3, heading matches 2, body matches 1.
fn word_weight(path: &[&str], heading: &[&str], body: &[&str], word: &Word) -> usize {
    let count = |tokens: &[&str], needle: &str| tokens.iter().filter(|t| **t == needle).count();
    let weighted =
        |needle: &str| 3 * count(path, needle) + 2 * count(heading, needle) + count(body, needle);
    let joined = weighted(&word.joined);
    if joined > 0 || word.parts.len() < 2 {
        return joined;
    }
    word.searchable_parts().map(weighted).min().unwrap_or(0)
}

/// Relevance of a chunk to the query, or `None` when it does not match under
/// `mode`. Every matched word adds a flat 10 to its weight.
fn score(query: &Query, mode: QueryMode, fields: &IndexFields) -> Option<f64> {
    let path: Vec<&str> = fields.path.split_whitespace().collect();
    let heading: Vec<&str> = fields.heading.split_whitespace().collect();
    let body: Vec<&str> = fields.body.split_whitespace().collect();
    let mut matched = 0usize;
    let mut total = 0.0;
    for word in query.words() {
        let weighted = word_weight(&path, &heading, &body, word);
        if weighted > 0 {
            matched += 1;
            total += 10.0 + weighted as f64;
        }
    }
    let wanted = match mode {
        QueryMode::And => query.words().len(),
        QueryMode::OrFallback => 1,
    };
    (matched >= wanted).then_some(total)
}

// --- OperationLock ------------------------------------------------------------

/// The held locks (source to the operation that holds it, when it was named) and a
/// signal for waiters.
#[derive(Debug, Default)]
struct LockTable {
    held: Mutex<BTreeMap<SourceId, Option<String>>>,
    released: Condvar,
}

/// Releases one source's entry when dropped and wakes the waiters.
struct HeldLock {
    table: Arc<LockTable>,
    source: SourceId,
}

impl Drop for HeldLock {
    fn drop(&mut self) {
        locked(&self.table.held).remove(&self.source);
        self.table.released.notify_all();
    }
}

/// An in-process operation lock: a second `acquire` of a held source is `Busy`.
/// `acquire_waiting` waits on a condition variable for the holder to let go and
/// reports the holder's operation when the wait runs out. Clones share the same
/// held locks.
#[derive(Debug, Clone, Default)]
pub struct FakeOperationLock {
    table: Arc<LockTable>,
}

impl FakeOperationLock {
    pub fn new() -> Self {
        Self::default()
    }

    fn guard(&self, source: &SourceId) -> LockGuard {
        LockGuard::new(HeldLock {
            table: Arc::clone(&self.table),
            source: source.clone(),
        })
    }
}

impl OperationLock for FakeOperationLock {
    fn acquire(&self, source: &SourceId) -> Result<LockGuard, StorageError> {
        let mut held = locked(&self.table.held);
        if held.contains_key(source) {
            return Err(StorageError::Busy);
        }
        held.insert(source.clone(), None);
        Ok(self.guard(source))
    }

    fn acquire_waiting(
        &self,
        source: &SourceId,
        operation: &str,
        max_wait: Duration,
    ) -> Result<LockGuard, StorageError> {
        let mut held = locked(&self.table.held);
        if !max_wait.is_zero() {
            held = self
                .table
                .released
                .wait_timeout_while(held, max_wait, |held| held.contains_key(source))
                .expect("fake lock state is not poisoned")
                .0;
        }
        if let Some(running) = held.get(source) {
            return Err(match running {
                Some(running) => StorageError::locked_by(running),
                None => StorageError::Busy,
            });
        }
        held.insert(source.clone(), Some(operation.to_owned()));
        Ok(self.guard(source))
    }
}

// --- GitClient --------------------------------------------------------------

#[derive(Debug)]
struct FakeCommit {
    id: CommitId,
    files: BTreeMap<RepoPath, Vec<u8>>,
    /// Paths of this commit that `RepoPath::from_git` refused.
    unsafe_paths: usize,
}

#[derive(Debug)]
struct FakeRemote {
    branch: String,
    commits: Vec<FakeCommit>,
}

#[derive(Debug)]
struct FakeCheckout {
    location: GitLocation,
    /// Index into the remote's commits of the newest fetched commit.
    head: usize,
}

#[derive(Debug, Default)]
struct GitState {
    remotes: BTreeMap<GitLocation, FakeRemote>,
    checkouts: BTreeMap<SourceId, FakeCheckout>,
    next_error: Option<GitError>,
}

/// Remote repositories and object-only local clones in memory. Set up remotes with
/// `add_remote` and `commit`; operations behave like a shallow, no-checkout clone
/// workflow: a fetched commit is readable, a newer one is not until `fetch_update`.
#[derive(Debug, Default)]
pub struct FakeGitClient {
    state: Mutex<GitState>,
}

fn hex40(bytes: &[u8]) -> String {
    blake3::hash(bytes).to_hex()[..40].to_owned()
}

fn blob_of(content: &[u8]) -> BlobOid {
    BlobOid::parse(&hex40(content)).expect("40 hex characters")
}

fn remote_key(location: &str) -> GitLocation {
    GitLocation::parse(location).expect("valid test location")
}

impl FakeGitClient {
    pub fn new() -> Self {
        Self::default()
    }

    /// Creates an empty remote at `location` whose default branch is `branch`.
    ///
    /// # Panics
    /// When `location` is not a valid `GitLocation`.
    pub fn add_remote(&self, location: &str, branch: &str) {
        locked(&self.state).remotes.insert(
            remote_key(location),
            FakeRemote {
                branch: branch.to_owned(),
                commits: Vec::new(),
            },
        );
    }

    /// Adds a commit holding exactly `files` (path, text) to the remote. Paths that
    /// `RepoPath::from_git` rejects are kept out of the tree and counted as unsafe,
    /// as the real adapter does.
    ///
    /// # Panics
    /// When the remote was not created with `add_remote`.
    pub fn commit(&self, location: &str, files: &[(&str, &str)]) -> CommitId {
        let mut state = locked(&self.state);
        let remote = state
            .remotes
            .get_mut(&remote_key(location))
            .expect("remote exists");
        let mut safe = BTreeMap::new();
        let mut unsafe_paths = 0;
        for (path, text) in files {
            match RepoPath::from_git(path) {
                Ok(path) => {
                    safe.insert(path, text.as_bytes().to_vec());
                }
                Err(_) => unsafe_paths += 1,
            }
        }
        let mut seed = remote
            .commits
            .last()
            .map(|c| c.id.to_string())
            .unwrap_or_default();
        for (path, content) in &safe {
            seed.push_str(path.as_str());
            seed.push_str(&hex40(content));
        }
        seed.push_str(&unsafe_paths.to_string());
        let id = CommitId::parse(&hex40(seed.as_bytes())).expect("40 hex characters");
        remote.commits.push(FakeCommit {
            id: id.clone(),
            files: safe,
            unsafe_paths,
        });
        id
    }

    /// Renames the default branch of the remote at `location` to `branch`, as when
    /// a repository moves from `master` to `main`. Clones tracking the old name can
    /// no longer fetch it; `default_branch` reports the new one.
    ///
    /// # Panics
    /// When the remote was not created with `add_remote`.
    pub fn rename_default_branch(&self, location: &str, branch: &str) {
        locked(&self.state)
            .remotes
            .get_mut(&remote_key(location))
            .expect("remote exists")
            .branch = branch.to_owned();
    }

    /// Makes the next operation fail with `error`.
    pub fn fail_next(&self, error: GitError) {
        locked(&self.state).next_error = Some(error);
    }

    fn begin(&self) -> Result<std::sync::MutexGuard<'_, GitState>, GitError> {
        let mut state = locked(&self.state);
        match state.next_error.take() {
            Some(error) => Err(error),
            None => Ok(state),
        }
    }
}

impl GitState {
    /// The remote behind a source's checkout, and the checkout's head index.
    fn checked_out(&self, source: &SourceId) -> Result<(&FakeRemote, usize), GitError> {
        let checkout = self
            .checkouts
            .get(source)
            .ok_or_else(|| GitError::NotFound(format!("no clone for {source}").into()))?;
        let remote = self
            .remotes
            .get(&checkout.location)
            .ok_or_else(|| GitError::NotFound(checkout.location.to_string().into()))?;
        Ok((remote, checkout.head))
    }

    fn fetched_commits(&self, source: &SourceId) -> Result<&[FakeCommit], GitError> {
        let (remote, head) = self.checked_out(source)?;
        Ok(&remote.commits[..=head])
    }
}

impl GitClient for FakeGitClient {
    fn default_branch(&self, location: &GitLocation) -> Result<GitRef, GitError> {
        let state = self.begin()?;
        let remote = state
            .remotes
            .get(location)
            .ok_or_else(|| GitError::NotFound(location.to_string().into()))?;
        GitRef::parse(&remote.branch).map_err(|e| GitError::Failed(e.to_string().into()))
    }

    fn clone_shallow(
        &self,
        source: &SourceId,
        location: &GitLocation,
        git_ref: &GitRef,
    ) -> Result<CommitId, GitError> {
        let mut state = self.begin()?;
        let remote = state
            .remotes
            .get(location)
            .ok_or_else(|| GitError::NotFound(location.to_string().into()))?;
        if remote.branch != git_ref.as_str() {
            return Err(GitError::NotFound(format!("ref {git_ref}").into()));
        }
        let head = remote
            .commits
            .len()
            .checked_sub(1)
            .ok_or_else(|| GitError::NotFound("repository has no commits".into()))?;
        let id = remote.commits[head].id.clone();
        // Idempotent: an existing clone of this source is replaced.
        state.checkouts.insert(
            source.clone(),
            FakeCheckout {
                location: location.clone(),
                head,
            },
        );
        Ok(id)
    }

    fn fetch_update(&self, source: &SourceId, git_ref: &GitRef) -> Result<CommitId, GitError> {
        let mut state = self.begin()?;
        let (remote, _) = state.checked_out(source)?;
        if remote.branch != git_ref.as_str() {
            return Err(GitError::NotFound(format!("ref {git_ref}").into()));
        }
        let head = remote.commits.len() - 1;
        let id = remote.commits[head].id.clone();
        if let Some(checkout) = state.checkouts.get_mut(source) {
            checkout.head = head;
        }
        Ok(id)
    }

    fn ls_tree(&self, source: &SourceId, commit: &CommitId) -> Result<TreeListing, GitError> {
        let state = self.begin()?;
        let commit = state
            .fetched_commits(source)?
            .iter()
            .find(|c| &c.id == commit)
            .ok_or_else(|| GitError::NotFound(format!("commit {commit}").into()))?;
        Ok(TreeListing {
            entries: commit
                .files
                .iter()
                .map(|(path, content)| TreeEntry {
                    path: path.clone(),
                    blob: blob_of(content),
                    size: content.len() as u64,
                })
                .collect(),
            skipped_unsafe: commit.unsafe_paths,
        })
    }

    fn read_blobs(
        &self,
        source: &SourceId,
        blobs: &[BlobOid],
    ) -> Result<BTreeMap<BlobOid, Vec<u8>>, GitError> {
        let state = self.begin()?;
        let available: BTreeMap<BlobOid, &Vec<u8>> = state
            .fetched_commits(source)?
            .iter()
            .flat_map(|commit| commit.files.values())
            .map(|content| (blob_of(content), content))
            .collect();
        let wanted: BTreeSet<&BlobOid> = blobs.iter().collect();
        let mut contents = BTreeMap::new();
        let mut total = 0u64;
        for blob in wanted {
            let content = available
                .get(blob)
                .ok_or_else(|| GitError::NotFound(format!("blob {blob}").into()))?;
            let size = content.len() as u64;
            if size > MAX_BLOB_BYTES {
                return Err(GitError::TooLarge(
                    format!("blob {blob} is {size} bytes, the limit is {MAX_BLOB_BYTES}").into(),
                ));
            }
            total += size;
            contents.insert(blob.clone(), (*content).clone());
        }
        if total > MAX_READ_BYTES {
            return Err(GitError::TooLarge(
                format!(
                    "{total} bytes requested, the limit is {MAX_READ_BYTES}; slice the request"
                )
                .into(),
            ));
        }
        Ok(contents)
    }

    fn remove_checkout(&self, source: &SourceId) -> Result<(), GitError> {
        self.begin()?.checkouts.remove(source);
        Ok(())
    }
}
