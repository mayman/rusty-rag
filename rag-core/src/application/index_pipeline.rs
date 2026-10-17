//! The indexing pipeline shared by add, update, and reindex: from the files of one
//! commit to ONE `ChunkStore::commit_index`.
//!
//! Steps: apply `IndexPolicy::check_entry` to the tree listing, refuse a source whose
//! accepted files add up to too many files or bytes (from the `ls_tree` sizes, before
//! any blob is read), diff the accepted files against the stored manifest, read the
//! blobs of the files to (re)index in batches (by count and by bytes), apply
//! `IndexPolicy::check_content`, chunk, enforce the chunk cap, and commit.
//!
//! Memory: the blobs of one batch are dropped once chunked, but the chunks of every
//! file that is (re)indexed stay in memory until the single commit, so a run holds the
//! text of all changed files at once. The caps bound that: `MAX_FILES_PER_SOURCE`,
//! `MAX_SOURCE_BYTES` (accepted file sizes, 200 MiB) and the chunk limit. The chunks
//! already stored are never loaded: replaced chunks are counted per path with
//! `ChunkStore::chunk_count_for_paths`.
//!
//! Skipped files:
//! - Skipped by name, path, or size (`check_entry`) never enter the manifest and are
//!   never read: re-checking them on every run costs no I/O.
//! - Skipped by content (binary, invalid UTF-8, LFS pointer, minified) are stored as
//!   manifest entries with no chunks, so an update does not read them again while
//!   their blob is unchanged.
//! - Files with secret content are not recorded: the manifest is what `rag show`
//!   resolves paths through, and a file judged secret must not become showable. They
//!   are read again on every run, which costs only their bytes. They are not counted
//!   as added or changed. The paths of secret-skipped files (by name or by content)
//!   are listed in the report, at most `MAX_SECRET_PATHS_LISTED`.

use std::collections::{BTreeMap, BTreeSet};

use serde::Serialize;

use crate::application::error::{AppError, GitError, StorageError};
use crate::application::ports::{
    ChunkStore, Clock, GitClient, IndexChanges, IndexedFile, TreeListing, MAX_READ_BYTES,
};
use crate::application::progress::{
    ensure_in_time, Phase, PhaseTiming, Progress, Timings, NO_PROGRESS,
};
use crate::domain::chunker::Chunker;
use crate::domain::index_policy::{IndexPolicy, SkipReason, SourceTooLarge, MAX_CHUNKS_PER_SOURCE};
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
/// The most secret-skipped paths a report lists.
pub const MAX_SECRET_PATHS_LISTED: usize = 50;

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
    /// Paths of files left out because their name or content looks like a secret,
    /// sorted, at most `MAX_SECRET_PATHS_LISTED` (the counts in `skipped` are
    /// complete). Paths are repository paths, never file content.
    pub skipped_secret_paths: Vec<String>,
    /// Paths git listed that `RepoPath` refused (not indexed).
    pub skipped_unsafe_paths: usize,
    /// How long each phase of the run took, in the order the phases began. A phase
    /// that ran several times (read batches) is one entry. Zero when the caller gave
    /// no `Progress` that measures time.
    pub phases: Vec<PhaseTiming>,
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
            skipped_secret_paths: Vec::new(),
            skipped_unsafe_paths: 0,
            phases: Vec::new(),
        }
    }
}

/// The shared indexing pipeline.
pub struct IndexPipeline<'a> {
    git: &'a dyn GitClient,
    store: &'a dyn ChunkStore,
    clock: &'a dyn Clock,
    progress: &'a dyn Progress,
    chunk_limit: usize,
}

impl<'a> IndexPipeline<'a> {
    pub fn new(git: &'a dyn GitClient, store: &'a dyn ChunkStore, clock: &'a dyn Clock) -> Self {
        Self {
            git,
            store,
            clock,
            progress: &NO_PROGRESS,
            chunk_limit: MAX_CHUNKS_PER_SOURCE,
        }
    }

    /// Reports the read and commit phases to `progress`.
    pub fn with_progress(mut self, progress: &'a dyn Progress) -> Self {
        self.progress = progress;
        self
    }

    /// Lowers the most chunks a source may hold (default `MAX_CHUNKS_PER_SOURCE`).
    /// For tests of the cap; the production limit is the policy constant.
    pub fn with_chunk_limit(mut self, limit: usize) -> Self {
        self.chunk_limit = limit;
        self
    }

    /// Indexes `listing` (the files of `commit`) into `source` and commits once,
    /// with `expected_base` set to the commit the source is indexed at (none for a
    /// first index). The read and commit phases are added to `timings`. On success
    /// `source` is `Ready` at `commit`; on any error nothing was written and `source`
    /// is unchanged.
    pub fn run(
        &self,
        source: &mut Source,
        commit: &CommitId,
        listing: &TreeListing,
        scope: IndexScope,
        timings: &mut Timings,
    ) -> Result<IndexReport, AppError> {
        let id = source.id();
        let base = source.indexed().map(|revision| revision.commit.clone());
        let stored = self.store.manifest(id)?;

        let mut skipped: BTreeMap<&'static str, usize> = BTreeMap::new();
        let mut secret_paths: BTreeSet<RepoPath> = BTreeSet::new();
        let mut accepted: BTreeMap<&RepoPath, &TreeEntry> = BTreeMap::new();
        for entry in &listing.entries {
            match IndexPolicy::check_entry(entry) {
                Ok(()) => {
                    accepted.insert(&entry.path, entry);
                }
                Err(reason) => {
                    count(&mut skipped, reason);
                    if reason == SkipReason::SecretName {
                        secret_paths.insert(entry.path.clone());
                    }
                }
            }
        }
        IndexPolicy::check_source_size(accepted.len(), 0).map_err(too_large)?;
        // The sizes come from `ls-tree`, so this refuses a huge source before a
        // single blob is read.
        let accepted_bytes: u64 = accepted.values().map(|entry| entry.size).sum();
        IndexPolicy::check_source_bytes(accepted_bytes).map_err(too_large)?;

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
        let to_read: Vec<&TreeEntry> = diff
            .added
            .iter()
            .chain(&diff.changed)
            .filter_map(|path| accepted.get(path).copied())
            .collect();
        let files_unchanged = accepted.len() - to_read.len();

        let mut upserts: Vec<IndexedFile> = Vec::new();
        let mut chunks_written = 0;
        let mut dropped_as_secret: BTreeSet<RepoPath> = BTreeSet::new();
        let batches = batches(&to_read);
        let batch_count = batches.len();
        for (number, batch) in batches.into_iter().enumerate() {
            ensure_in_time(self.progress)?;
            let detail = format!("reading batch {}/{batch_count}", number + 1);
            let blobs = timings.run(self.progress, Phase::Read, &detail, || {
                self.read_batch(id, &batch)
            })?;
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
                            secret_paths.insert(entry.path.clone());
                            dropped_as_secret.insert(entry.path.clone());
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

        let touched: Vec<RepoPath> = upserts
            .iter()
            .map(|file| file.path.clone())
            .chain(removals.iter().cloned())
            .collect();
        let total_chunks = projected_chunks(
            self.store.chunk_count(id)?,
            chunks_written,
            self.chunk_limit,
            || self.store.chunk_count_for_paths(id, &touched),
        )?;
        if total_chunks > self.chunk_limit {
            return Err(too_large(SourceTooLarge::Chunks {
                found: total_chunks,
                limit: self.chunk_limit,
            }));
        }

        let files_added = diff
            .added
            .iter()
            .filter(|path| !dropped_as_secret.contains(path))
            .count();
        let files_changed = diff
            .changed
            .iter()
            .filter(|path| !dropped_as_secret.contains(path))
            .count();
        let files_removed = removals.len();
        let changes = IndexChanges {
            upserts,
            removals: removals.into_iter().collect(),
        };
        ensure_in_time(self.progress)?;
        let mut next = source.clone();
        next.mark_ready(commit.clone(), self.clock.now());
        timings.run(self.progress, Phase::Commit, "writing the index", || {
            self.store.commit_index(&next, base.as_ref(), &changes)
        })?;
        *source = next;

        Ok(IndexReport {
            commit: commit.to_string(),
            files_added,
            files_changed,
            files_removed,
            files_unchanged,
            chunks_written,
            skipped,
            skipped_secret_paths: secret_paths
                .into_iter()
                .take(MAX_SECRET_PATHS_LISTED)
                .map(|path| path.as_str().to_owned())
                .collect(),
            skipped_unsafe_paths: listing.skipped_unsafe,
            phases: Vec::new(),
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

fn too_large(error: SourceTooLarge) -> AppError {
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
/// of the files touched, plus the new ones. The per-path count costs a query, so
/// while the cheap upper bound (nothing replaced) is within `limit` it is returned
/// and `touched_stored_chunks` is not called.
fn projected_chunks(
    stored: usize,
    written: usize,
    limit: usize,
    touched_stored_chunks: impl FnOnce() -> Result<usize, StorageError>,
) -> Result<usize, StorageError> {
    let upper_bound = stored + written;
    if upper_bound <= limit {
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
        let total =
            projected_chunks(10, 5, MAX_CHUNKS_PER_SOURCE, || panic!("not needed")).unwrap();
        assert_eq!(total, 15);
        let at_cap = projected_chunks(MAX_CHUNKS_PER_SOURCE - 5, 5, MAX_CHUNKS_PER_SOURCE, || {
            panic!("not needed")
        })
        .unwrap();
        assert_eq!(at_cap, MAX_CHUNKS_PER_SOURCE);
    }

    #[test]
    fn near_the_cap_replaced_chunks_are_not_counted_twice() {
        let total = projected_chunks(MAX_CHUNKS_PER_SOURCE, 100, MAX_CHUNKS_PER_SOURCE, || {
            Ok(100)
        })
        .unwrap();
        assert_eq!(total, MAX_CHUNKS_PER_SOURCE);
        let over =
            projected_chunks(MAX_CHUNKS_PER_SOURCE, 100, MAX_CHUNKS_PER_SOURCE, || Ok(10)).unwrap();
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

    use crate::application::error::ErrorCode;
    use crate::application::indexer_test_support::RecordingProgress;
    use crate::application::ports::{
        ChunkStore, GitClient, IndexChanges, ReindexedChunk, SourceRepository,
    };
    use crate::application::progress::Timings;
    use crate::application::testkit::InMemoryStore;
    use crate::domain::chunk::Chunk;
    use crate::domain::chunk_id::ChunkId;
    use crate::domain::index_versions::IndexVersions;

    fn token_file() -> String {
        format!("const T: &str = \"ghp_{}\";\n", "aB3".repeat(12))
    }

    #[test]
    fn files_with_secret_content_are_not_counted_as_added() {
        let w = World::new();
        w.commit(&[
            ("src/lib.rs", "pub fn parse_config() {}\n"),
            ("config/token.rs", &token_file()),
            ("config/token2.rs", &token_file()),
        ]);

        let report = w.add().unwrap().index;

        assert_eq!(report.files_added, 1, "only src/lib.rs was recorded");
        assert_eq!(report.skipped.get("secret_content"), Some(&2));
        assert_eq!(w.store.manifest(&w.id()).unwrap().len(), 1);
    }

    #[test]
    fn a_stored_file_that_turns_secret_counts_as_removed_not_as_changed() {
        let w = World::new();
        w.commit(&[("a.rs", "fn a() {}\n"), ("b.rs", "fn b() {}\n")]);
        w.add().unwrap();
        w.commit(&[("a.rs", &token_file()), ("b.rs", "fn b() {}\n")]);

        let report = w
            .update_source()
            .execute(&crate::application::update_source::UpdateRequest::all())
            .unwrap()
            .sources
            .remove(0)
            .index
            .unwrap();

        assert_eq!(
            (
                report.files_added,
                report.files_changed,
                report.files_removed
            ),
            (0, 0, 1)
        );
        assert_eq!(report.files_unchanged, 1);
    }

    #[test]
    fn the_report_lists_the_secret_skipped_paths_sorted() {
        let w = World::new();
        w.commit(&[
            ("src/lib.rs", "pub fn parse_config() {}\n"),
            (".env", "A=1\n"),
            ("deploy/id_rsa", "key\n"),
            ("z/token.rs", &token_file()),
            ("a/token.rs", &token_file()),
        ]);

        let report = w.add().unwrap().index;

        assert_eq!(
            report.skipped_secret_paths,
            [".env", "a/token.rs", "deploy/id_rsa", "z/token.rs"]
        );
        let json = serde_json::to_string(&report).unwrap();
        assert!(
            json.contains(
                r#""skipped_secret_paths":[".env","a/token.rs","deploy/id_rsa","z/token.rs"]"#
            ),
            "{json}"
        );
    }

    #[test]
    fn secret_skipped_paths_stop_at_fifty() {
        let w = World::new();
        let names: Vec<String> = (0..60).map(|i| format!(".env.s{i:02}")).collect();
        let mut files: Vec<(&str, &str)> = names.iter().map(|n| (n.as_str(), "A=1\n")).collect();
        files.push(("src/lib.rs", "pub fn parse_config() {}\n"));
        w.commit(&files);

        let report = w.add().unwrap().index;

        assert_eq!(
            report.skipped.get("secret_name"),
            Some(&60),
            "all are counted"
        );
        assert_eq!(
            report.skipped_secret_paths.len(),
            50,
            "but at most 50 are listed"
        );
        assert_eq!(report.skipped_secret_paths[0], ".env.s00");
        assert_eq!(report.skipped_secret_paths[49], ".env.s49");
    }

    #[test]
    fn a_report_without_secrets_lists_no_secret_paths() {
        let w = World::new();
        w.commit(&[("a.rs", "fn a() {}\n")]);
        assert!(w.add().unwrap().index.skipped_secret_paths.is_empty());
    }

    fn big_listing(files: usize, size: u64) -> TreeListing {
        TreeListing {
            entries: (0..files)
                .map(|i| entry(&format!("f{i:04}.txt"), size, (i % 250) as u8))
                .collect(),
            skipped_unsafe: 0,
        }
    }

    #[test]
    fn a_source_over_the_accepted_bytes_cap_is_refused_before_any_blob_is_read() {
        use crate::domain::index_policy::{MAX_FILE_BYTES, MAX_SOURCE_BYTES};

        let w = World::new();
        let head = w.commit(&[("a.rs", "fn a() {}\n")]);
        let spy = GitSpy::new(&w.git);
        let files = (MAX_SOURCE_BYTES / MAX_FILE_BYTES) as usize + 2;
        let listing = big_listing(files, MAX_FILE_BYTES);
        let id = w.id();
        let mut source = crate::domain::source::Source::register(id.clone(), "r", location(REMOTE));
        w.store.save(&source).unwrap();

        let error = IndexPipeline::new(&spy, &w.store, &w.clock)
            .run(
                &mut source,
                &head,
                &listing,
                IndexScope::Changes,
                &mut Timings::new(),
            )
            .unwrap_err();

        assert_eq!(error.code(), ErrorCode::Generic);
        assert!(error.to_string().contains("too large"), "{error}");
        assert!(error.to_string().contains("bytes"), "{error}");
        assert!(
            !spy.called("read_blobs"),
            "no blob is read for a refused source"
        );
        assert!(w.store.manifest(&id).unwrap().is_empty());
    }

    fn location(remote: &str) -> crate::domain::git_location::GitLocation {
        crate::domain::git_location::GitLocation::parse(remote).unwrap()
    }

    /// A store that refuses to hand out every chunk of a source and counts how often
    /// the pipeline asks for the chunks of some paths.
    struct NoFullLoad<'a> {
        inner: &'a InMemoryStore,
        per_path_queries: std::sync::atomic::AtomicUsize,
    }

    impl SourceRepository for NoFullLoad<'_> {
        fn get(
            &self,
            id: &crate::domain::source_id::SourceId,
        ) -> Result<Option<crate::domain::source::Source>, crate::application::error::StorageError>
        {
            self.inner.get(id)
        }
        fn find_by_name(
            &self,
            name: &str,
        ) -> Result<Option<crate::domain::source::Source>, crate::application::error::StorageError>
        {
            self.inner.find_by_name(name)
        }
        fn list(
            &self,
        ) -> Result<Vec<crate::domain::source::Source>, crate::application::error::StorageError>
        {
            self.inner.list()
        }
        fn save(
            &self,
            source: &crate::domain::source::Source,
        ) -> Result<(), crate::application::error::StorageError> {
            self.inner.save(source)
        }
    }

    impl ChunkStore for NoFullLoad<'_> {
        fn commit_index(
            &self,
            source: &crate::domain::source::Source,
            expected_base: Option<&CommitId>,
            changes: &IndexChanges,
        ) -> Result<(), crate::application::error::StorageError> {
            self.inner.commit_index(source, expected_base, changes)
        }
        fn remove_source(
            &self,
            id: &crate::domain::source_id::SourceId,
        ) -> Result<(), crate::application::error::StorageError> {
            self.inner.remove_source(id)
        }
        fn manifest(
            &self,
            source: &crate::domain::source_id::SourceId,
        ) -> Result<Manifest, crate::application::error::StorageError> {
            self.inner.manifest(source)
        }
        fn chunk_count(
            &self,
            source: &crate::domain::source_id::SourceId,
        ) -> Result<usize, crate::application::error::StorageError> {
            self.inner.chunk_count(source)
        }
        fn chunk_count_for_paths(
            &self,
            source: &crate::domain::source_id::SourceId,
            paths: &[RepoPath],
        ) -> Result<usize, crate::application::error::StorageError> {
            self.per_path_queries
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            self.inner.chunk_count_for_paths(source, paths)
        }
        fn chunk(
            &self,
            id: &ChunkId,
        ) -> Result<Option<Chunk>, crate::application::error::StorageError> {
            self.inner.chunk(id)
        }
        fn chunks(
            &self,
            _source: &crate::domain::source_id::SourceId,
        ) -> Result<Vec<Chunk>, crate::application::error::StorageError> {
            panic!("the pipeline must not load every chunk of a source");
        }
        fn stored_versions(
            &self,
            source: &crate::domain::source_id::SourceId,
        ) -> Result<Option<IndexVersions>, crate::application::error::StorageError> {
            self.inner.stored_versions(source)
        }
        fn rewrite_index_fields(
            &self,
            source: &crate::domain::source_id::SourceId,
            fields: &[ReindexedChunk],
            versions: IndexVersions,
        ) -> Result<(), crate::application::error::StorageError> {
            self.inner.rewrite_index_fields(source, fields, versions)
        }
    }

    #[test]
    fn near_the_chunk_cap_the_replaced_chunks_are_counted_per_path_without_loading_all() {
        let w = World::new();
        w.commit(&[
            ("a.md", "# A\none\n# B\ntwo\n"),
            ("b.md", "# C\nthree\n# D\nfour\n"),
        ]);
        w.add().unwrap();
        let stored_chunks = w.store.chunk_count(&w.id()).unwrap();
        let second = w.commit(&[
            ("a.md", "# A\none changed\n# B\ntwo\n"),
            ("b.md", "# C\nthree\n# D\nfour\n"),
        ]);
        w.git
            .fetch_update(
                &w.id(),
                &crate::domain::git_ref::GitRef::parse("main").unwrap(),
            )
            .unwrap();
        let listing = w.git.ls_tree(&w.id(), &second).unwrap();
        let store = NoFullLoad {
            inner: &w.store,
            per_path_queries: std::sync::atomic::AtomicUsize::new(0),
        };
        let mut source = w.store.get(&w.id()).unwrap().unwrap();

        // A cap the cheap upper bound (stored + written) passes only by replaced chunks.
        let report = IndexPipeline::new(&w.git, &store, &w.clock)
            .with_chunk_limit(stored_chunks)
            .run(
                &mut source,
                &second,
                &listing,
                IndexScope::Changes,
                &mut Timings::new(),
            )
            .expect("replacing a file does not grow the source past the cap");

        assert_eq!(report.files_changed, 1);
        assert_eq!(
            store
                .per_path_queries
                .load(std::sync::atomic::Ordering::SeqCst),
            1,
            "asked once, for the touched paths"
        );
    }

    #[test]
    fn growing_past_the_chunk_cap_is_refused() {
        let w = World::new();
        w.commit(&[("a.md", "# A\none\n# B\ntwo\n")]);
        w.add().unwrap();
        let stored_chunks = w.store.chunk_count(&w.id()).unwrap();
        let second = w.commit(&[
            ("a.md", "# A\none\n# B\ntwo\n"),
            ("b.md", "# C\nthree\n# D\nfour\n"),
        ]);
        w.git
            .fetch_update(
                &w.id(),
                &crate::domain::git_ref::GitRef::parse("main").unwrap(),
            )
            .unwrap();
        let listing = w.git.ls_tree(&w.id(), &second).unwrap();
        let mut source = w.store.get(&w.id()).unwrap().unwrap();

        let error = IndexPipeline::new(&w.git, &w.store, &w.clock)
            .with_chunk_limit(stored_chunks)
            .run(
                &mut source,
                &second,
                &listing,
                IndexScope::Changes,
                &mut Timings::new(),
            )
            .unwrap_err();

        assert!(error.to_string().contains("chunks"), "{error}");
        assert_eq!(w.store.chunk_count(&w.id()).unwrap(), stored_chunks);
    }

    #[test]
    fn an_add_announces_its_phases_and_reports_their_times() {
        let w = World::new();
        w.commit(&[("a.rs", "fn a() {}\n"), ("b.rs", "fn b() {}\n")]);
        let progress = RecordingProgress::default();

        let response = w
            .add_source_with(&progress)
            .execute(&AddRequest::new(REMOTE))
            .unwrap();

        assert_eq!(
            progress.sequence(),
            [
                "begin:resolve",
                "end:resolve",
                "begin:fetch",
                "end:fetch",
                "begin:list",
                "detail:list",
                "end:list",
                "begin:read",
                "end:read",
                "begin:commit",
                "end:commit",
            ]
        );
        let events = progress.events();
        assert!(
            events.contains(&"detail:list:2 files".to_owned()),
            "{events:?}"
        );
        assert!(
            events.contains(&"begin:read:reading batch 1/1".to_owned()),
            "{events:?}"
        );
        let phases: Vec<(&str, u64)> = response
            .index
            .phases
            .iter()
            .map(|p| (p.phase, p.elapsed_ms))
            .collect();
        assert_eq!(
            phases,
            [
                ("resolve", 5),
                ("fetch", 5),
                ("list", 5),
                ("read", 5),
                ("commit", 5)
            ]
        );
    }

    #[test]
    fn read_batches_are_numbered_and_their_time_is_summed() {
        let w = World::new();
        let contents: Vec<String> = (0..450).map(|i| format!("file number {i}\n")).collect();
        let names: Vec<String> = (0..450).map(|i| format!("f{i}.txt")).collect();
        let files: Vec<(&str, &str)> = names
            .iter()
            .zip(&contents)
            .map(|(n, c)| (n.as_str(), c.as_str()))
            .collect();
        w.commit(&files);
        let progress = RecordingProgress::default();

        let response = w
            .add_source_with(&progress)
            .execute(&AddRequest::new(REMOTE))
            .unwrap();

        let events = progress.events();
        for batch in ["1/3", "2/3", "3/3"] {
            assert!(
                events.contains(&format!("begin:read:reading batch {batch}")),
                "{batch}: {events:?}"
            );
        }
        let read = response
            .index
            .phases
            .iter()
            .find(|p| p.phase == "read")
            .unwrap();
        assert_eq!(read.elapsed_ms, 15, "three batches of 5 ms");
    }

    #[test]
    fn without_a_progress_the_phases_are_listed_with_no_time() {
        let w = World::new();
        w.commit(&[("a.rs", "fn a() {}\n")]);
        let phases = w.add().unwrap().index.phases;
        assert_eq!(phases.len(), 5);
        assert!(phases.iter().all(|p| p.elapsed_ms == 0));
    }
}
