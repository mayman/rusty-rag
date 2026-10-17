//! The indexing pipeline shared by add, update, and reindex: from the files of one
//! commit to ONE `ChunkStore::commit_index`.
//!
//! Steps: apply `IndexPolicy::check_entry` to the tree listing, diff the accepted
//! files against the stored manifest, read the blobs of the files to (re)index in
//! batches (by count and by bytes, so memory holds one batch of blobs at a time),
//! apply `IndexPolicy::check_content`, chunk, enforce the per-source caps, and commit.
//!
//! Skipped files:
//! - Skipped by name, path, or size (`check_entry`) never enter the manifest and are
//!   never read: re-checking them on every run costs no I/O.
//! - Skipped by content (binary, invalid UTF-8, LFS pointer, minified) are stored as
//!   manifest entries with no chunks, so an update does not read them again while
//!   their blob is unchanged.
//! - Files with secret content are not recorded: the manifest is what `rag show`
//!   resolves paths through, and a file judged secret must not become showable. They
//!   are read again on every run, which costs only their bytes.

use std::collections::{BTreeMap, BTreeSet};

use serde::Serialize;

use crate::application::error::{AppError, GitError, StorageError};
use crate::application::ports::{
    ChunkStore, Clock, GitClient, IndexChanges, IndexedFile, TreeListing, MAX_READ_BYTES,
};
use crate::domain::chunker::Chunker;
use crate::domain::index_policy::{IndexPolicy, SkipReason, MAX_CHUNKS_PER_SOURCE};
use crate::domain::manifest::Manifest;
use crate::domain::oid::{BlobOid, CommitId};
use crate::domain::repo_path::RepoPath;
use crate::domain::source::Source;
use crate::domain::source_id::SourceId;
use crate::domain::tree_entry::TreeEntry;

/// The most files whose blobs one `read_blobs` call asks for.
pub const BATCH_MAX_FILES: usize = 200;
/// The most content (sum of `ls_tree` sizes) one `read_blobs` call asks for. Well
/// under `MAX_READ_BYTES`, so the largest single file always fits in a batch.
pub const BATCH_MAX_BYTES: u64 = MAX_READ_BYTES / 4;

/// How much of the tree to index.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IndexScope {
    /// Only files that are new or whose blob changed since the stored manifest.
    Changes,
    /// Every accepted file, as a reindex does (stored files that are no longer
    /// accepted are removed).
    Everything,
}

/// What one pipeline run did. Serialized into the responses of add, update, and
/// reindex.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct IndexReport {
    pub commit: String,
    pub files_added: usize,
    pub files_changed: usize,
    pub files_removed: usize,
    pub files_unchanged: usize,
    /// Chunks written by this run.
    pub chunks_written: usize,
    /// Files left out in this run, by `SkipReason::as_str`. Files skipped by content
    /// in an earlier run and unchanged since are not read again and not counted.
    pub skipped: BTreeMap<&'static str, usize>,
    /// Paths git listed that `RepoPath` refused (not indexed).
    pub skipped_unsafe_paths: usize,
}

impl IndexReport {
    /// The report of a run that found nothing to do at `commit`.
    pub fn nothing_to_do(commit: &CommitId) -> Self {
        Self {
            commit: commit.to_string(),
            files_added: 0,
            files_changed: 0,
            files_removed: 0,
            files_unchanged: 0,
            chunks_written: 0,
            skipped: BTreeMap::new(),
            skipped_unsafe_paths: 0,
        }
    }
}

/// The shared indexing pipeline.
pub struct IndexPipeline<'a> {
    git: &'a dyn GitClient,
    store: &'a dyn ChunkStore,
    clock: &'a dyn Clock,
}

impl<'a> IndexPipeline<'a> {
    pub fn new(git: &'a dyn GitClient, store: &'a dyn ChunkStore, clock: &'a dyn Clock) -> Self {
        Self { git, store, clock }
    }

    /// Indexes `listing` (the files of `commit`) into `source` and commits once,
    /// with `expected_base` set to the commit the source is indexed at (none for a
    /// first index). On success `source` is `Ready` at `commit`; on any error
    /// nothing was written and `source` is unchanged.
    pub fn run(
        &self,
        source: &mut Source,
        commit: &CommitId,
        listing: &TreeListing,
        scope: IndexScope,
    ) -> Result<IndexReport, AppError> {
        let id = source.id();
        let base = source.indexed().map(|revision| revision.commit.clone());
        let stored = self.store.manifest(id)?;

        let mut skipped: BTreeMap<&'static str, usize> = BTreeMap::new();
        let mut accepted: BTreeMap<&RepoPath, &TreeEntry> = BTreeMap::new();
        for entry in &listing.entries {
            match IndexPolicy::check_entry(entry) {
                Ok(()) => {
                    accepted.insert(&entry.path, entry);
                }
                Err(reason) => count(&mut skipped, reason),
            }
        }
        IndexPolicy::check_source_size(accepted.len(), 0).map_err(too_large)?;

        let wanted: Manifest = accepted
            .values()
            .map(|entry| (entry.path.clone(), entry.blob.clone()))
            .collect();
        let compared_with = match scope {
            IndexScope::Changes => stored.clone(),
            IndexScope::Everything => Manifest::new(),
        };
        let diff = Manifest::diff(&compared_with, &wanted);
        let mut removals: BTreeSet<RepoPath> = stored
            .iter()
            .map(|(path, _)| path)
            .filter(|path| !accepted.contains_key(path))
            .cloned()
            .collect();
        let files_removed = removals.len();
        let to_read: Vec<&TreeEntry> = diff
            .added
            .iter()
            .chain(&diff.changed)
            .filter_map(|path| accepted.get(path).copied())
            .collect();
        let files_unchanged = accepted.len() - to_read.len();

        let mut upserts: Vec<IndexedFile> = Vec::new();
        let mut chunks_written = 0;
        for batch in batches(&to_read) {
            let blobs = self.read_batch(id, &batch)?;
            for entry in batch {
                let content = blobs.get(&entry.blob).ok_or_else(|| {
                    GitError::NotFound(format!("blob {} of {}", entry.blob, entry.path).into())
                })?;
                match IndexPolicy::check_content(&entry.path, content) {
                    Ok(()) => {
                        let text = String::from_utf8_lossy(content);
                        let chunks = Chunker::chunk(id, &entry.path, &text);
                        chunks_written += chunks.len();
                        upserts.push(IndexedFile {
                            path: entry.path.clone(),
                            blob: entry.blob.clone(),
                            size: entry.size,
                            chunks,
                        });
                    }
                    Err(reason) => {
                        count(&mut skipped, reason);
                        if reason == SkipReason::SecretContent {
                            // Not recorded; a stored version of it must go.
                            if stored.get(&entry.path).is_some() {
                                removals.insert(entry.path.clone());
                            }
                        } else {
                            upserts.push(IndexedFile {
                                path: entry.path.clone(),
                                blob: entry.blob.clone(),
                                size: entry.size,
                                chunks: Vec::new(),
                            });
                        }
                    }
                }
            }
        }

        let touched: BTreeSet<&RepoPath> = upserts
            .iter()
            .map(|file| &file.path)
            .chain(&removals)
            .collect();
        let total_chunks = projected_chunks(self.store.chunk_count(id)?, chunks_written, || {
            Ok(self
                .store
                .chunks(id)?
                .iter()
                .filter(|chunk| touched.contains(&chunk.path))
                .count())
        })?;
        IndexPolicy::check_source_size(accepted.len(), total_chunks).map_err(too_large)?;

        let changes = IndexChanges {
            upserts,
            removals: removals.into_iter().collect(),
        };
        let mut next = source.clone();
        next.mark_ready(commit.clone(), self.clock.now());
        self.store.commit_index(&next, base.as_ref(), &changes)?;
        *source = next;

        Ok(IndexReport {
            commit: commit.to_string(),
            files_added: diff.added.len(),
            files_changed: diff.changed.len(),
            files_removed,
            files_unchanged,
            chunks_written,
            skipped,
            skipped_unsafe_paths: listing.skipped_unsafe,
        })
    }

    fn read_batch(
        &self,
        id: &SourceId,
        batch: &[&TreeEntry],
    ) -> Result<BTreeMap<BlobOid, Vec<u8>>, GitError> {
        let blobs: Vec<BlobOid> = batch.iter().map(|entry| entry.blob.clone()).collect();
        self.git.read_blobs(id, &blobs)
    }
}

fn count(skipped: &mut BTreeMap<&'static str, usize>, reason: SkipReason) {
    *skipped.entry(reason.as_str()).or_insert(0) += 1;
}

fn too_large(error: crate::domain::index_policy::SourceTooLarge) -> AppError {
    AppError::Storage(StorageError::Failed(error.to_string().into()))
}

/// Cuts `entries` into batches of at most `BATCH_MAX_FILES` files and
/// `BATCH_MAX_BYTES` bytes (a single file larger than that gets a batch of its own).
fn batches<'e>(entries: &[&'e TreeEntry]) -> Vec<Vec<&'e TreeEntry>> {
    let mut batches: Vec<Vec<&TreeEntry>> = Vec::new();
    let mut bytes = 0u64;
    for entry in entries {
        let fits = batches.last().is_some_and(|batch| {
            batch.len() < BATCH_MAX_FILES && bytes + entry.size <= BATCH_MAX_BYTES
        });
        if fits {
            bytes += entry.size;
            batches.last_mut().expect("a batch to extend").push(entry);
        } else {
            bytes = entry.size;
            batches.push(vec![entry]);
        }
    }
    batches
}

/// How many chunks the source holds after the change: the stored ones, minus those
/// of the files touched, plus the new ones. The stored per-file counts are costly to
/// read, so while the cheap upper bound (nothing replaced) is within the cap it is
/// returned and `touched_stored_chunks` is not called.
fn projected_chunks(
    stored: usize,
    written: usize,
    touched_stored_chunks: impl FnOnce() -> Result<usize, StorageError>,
) -> Result<usize, StorageError> {
    let upper_bound = stored + written;
    if upper_bound <= MAX_CHUNKS_PER_SOURCE {
        return Ok(upper_bound);
    }
    Ok(stored.saturating_sub(touched_stored_chunks()?) + written)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::application::add_source::AddRequest;
    use crate::application::indexer_test_support::{GitSpy, World, REMOTE};

    fn entry(path: &str, size: u64, n: u8) -> TreeEntry {
        TreeEntry {
            path: RepoPath::from_git(path).unwrap(),
            blob: BlobOid::parse(&format!("{n:02x}").repeat(20)).unwrap(),
            size,
        }
    }

    fn sizes(batches: &[Vec<&TreeEntry>]) -> Vec<usize> {
        batches.iter().map(Vec::len).collect()
    }

    #[test]
    fn batches_are_cut_by_file_count() {
        let entries: Vec<TreeEntry> = (0..450).map(|i| entry(&format!("f{i}"), 1, 1)).collect();
        let refs: Vec<&TreeEntry> = entries.iter().collect();
        assert_eq!(sizes(&batches(&refs)), [200, 200, 50]);
    }

    #[test]
    fn batches_are_cut_by_bytes() {
        let big = BATCH_MAX_BYTES / 3;
        let entries: Vec<TreeEntry> = (0..7).map(|i| entry(&format!("f{i}"), big, 1)).collect();
        let refs: Vec<&TreeEntry> = entries.iter().collect();
        let cut = batches(&refs);
        assert_eq!(sizes(&cut), [3, 3, 1]);
        for batch in &cut {
            assert!(batch.iter().map(|e| e.size).sum::<u64>() <= BATCH_MAX_BYTES);
        }
    }

    #[test]
    fn a_file_larger_than_a_batch_still_gets_its_own_batch() {
        let entries = [
            entry("a", 1, 1),
            entry("big", BATCH_MAX_BYTES + 1, 2),
            entry("b", 1, 3),
        ];
        let refs: Vec<&TreeEntry> = entries.iter().collect();
        assert_eq!(sizes(&batches(&refs)), [1, 1, 1]);
    }

    #[test]
    fn no_entries_make_no_batches() {
        assert!(batches(&[]).is_empty());
    }

    #[test]
    fn the_chunk_total_is_cheap_while_far_from_the_cap() {
        let total = projected_chunks(10, 5, || panic!("not needed")).unwrap();
        assert_eq!(total, 15);
        let at_cap =
            projected_chunks(MAX_CHUNKS_PER_SOURCE - 5, 5, || panic!("not needed")).unwrap();
        assert_eq!(at_cap, MAX_CHUNKS_PER_SOURCE);
    }

    #[test]
    fn near_the_cap_replaced_chunks_are_not_counted_twice() {
        let total = projected_chunks(MAX_CHUNKS_PER_SOURCE, 100, || Ok(100)).unwrap();
        assert_eq!(total, MAX_CHUNKS_PER_SOURCE);
        let over = projected_chunks(MAX_CHUNKS_PER_SOURCE, 100, || Ok(10)).unwrap();
        assert_eq!(over, MAX_CHUNKS_PER_SOURCE + 90);
    }

    #[test]
    fn blobs_are_read_in_batches_by_count() {
        let w = World::new();
        let contents: Vec<String> = (0..450).map(|i| format!("file number {i}\n")).collect();
        let names: Vec<String> = (0..450).map(|i| format!("f{i}.txt")).collect();
        let files: Vec<(&str, &str)> = names
            .iter()
            .zip(&contents)
            .map(|(n, c)| (n.as_str(), c.as_str()))
            .collect();
        w.commit(&files);
        let spy = GitSpy::new(&w.git);

        w.add_source_via(&spy)
            .execute(&AddRequest::new(REMOTE))
            .unwrap();

        let counts: Vec<usize> = spy.reads().iter().map(|(blobs, _)| *blobs).collect();
        assert_eq!(counts, [200, 200, 50]);
        assert_eq!(w.store.manifest(&w.id()).unwrap().len(), 450);
    }

    #[test]
    fn blobs_are_read_in_batches_by_bytes() {
        let w = World::new();
        let contents: Vec<String> = (0..12)
            .map(|i| format!("{i}\n{}", "some line of text\n".repeat(50_000)))
            .collect();
        let names: Vec<String> = (0..12).map(|i| format!("big{i}.txt")).collect();
        let files: Vec<(&str, &str)> = names
            .iter()
            .zip(&contents)
            .map(|(n, c)| (n.as_str(), c.as_str()))
            .collect();
        w.commit(&files);
        let spy = GitSpy::new(&w.git);

        w.add_source_via(&spy)
            .execute(&AddRequest::new(REMOTE))
            .unwrap();

        let reads = spy.reads();
        assert!(reads.len() >= 2, "{reads:?}");
        for (_, bytes) in &reads {
            assert!(*bytes <= BATCH_MAX_BYTES, "{reads:?}");
        }
        assert_eq!(reads.iter().map(|(blobs, _)| blobs).sum::<usize>(), 12);
        assert_eq!(w.store.manifest(&w.id()).unwrap().len(), 12);
    }
}
