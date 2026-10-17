//! Contract tests for the store and lock ports. Each function takes a factory for a
//! fresh, empty implementation and panics (with a message naming the scenario) when
//! the implementation misbehaves. `rag-core` runs them against the in-memory fakes;
//! `rag-infra` runs the same functions against the real adapters, so the two cannot
//! drift.
//!
//! Every scenario registers its source (`SourceRepository::save`) before the first
//! `commit_index`, as the use cases do.

use std::sync::Mutex;
use std::time::Duration;

use crate::application::error::StorageError;
use crate::application::ports::{
    ChunkSearcher, ChunkStore, IndexChanges, IndexedFile, OperationLock, ReindexedChunk,
    SearchOutcome, SourceRepository, TOTAL_MATCHES_CAP,
};
use crate::application::retrieval::retrieve;
use crate::domain::chunk::Chunk;
use crate::domain::chunk_id::ChunkId;
use crate::domain::chunker::Chunker;
use crate::domain::git_location::GitLocation;
use crate::domain::git_ref::GitRef;
use crate::domain::index_versions::IndexVersions;
use crate::domain::indexed_at::IndexedAt;
use crate::domain::language::Language;
use crate::domain::line_range::LineRange;
use crate::domain::manifest::Manifest;
use crate::domain::oid::{BlobOid, CommitId};
use crate::domain::path_glob::PathGlob;
use crate::domain::query::{Query, QueryMode};
use crate::domain::repo_path::RepoPath;
use crate::domain::scored_chunk::ScoredChunk;
use crate::domain::search_filters::SearchFilters;
use crate::domain::source::{IndexedRevision, Source, SourceStatus};
use crate::domain::source_id::SourceId;

// --- fixtures -----------------------------------------------------------------

fn source_id(name: &str) -> SourceId {
    SourceId::from_location(&location(name)).expect("valid test source")
}

fn location(name: &str) -> GitLocation {
    GitLocation::parse(&format!("https://example.com/o/{name}")).expect("valid test location")
}

fn path(text: &str) -> RepoPath {
    RepoPath::parse(text).expect("valid test path")
}

fn blob(n: u8) -> BlobOid {
    BlobOid::parse(&format!("{n:02x}").repeat(20)).expect("valid test blob")
}

fn commit_id(n: u8) -> CommitId {
    CommitId::parse(&format!("{n:02x}").repeat(20)).expect("valid test commit")
}

/// A file chunked by the real `Chunker`, as the indexing use case would build it.
fn file(source: &SourceId, file_path: &str, text: &str, blob_n: u8) -> IndexedFile {
    let path = path(file_path);
    IndexedFile {
        chunks: Chunker::chunk(source, &path, text),
        path,
        blob: blob(blob_n),
        size: text.len() as u64,
    }
}

fn upserts(files: Vec<IndexedFile>) -> IndexChanges {
    IndexChanges {
        upserts: files,
        removals: Vec::new(),
    }
}

fn removals(paths: &[&str]) -> IndexChanges {
    IndexChanges {
        upserts: Vec::new(),
        removals: paths.iter().map(|p| path(p)).collect(),
    }
}

fn chunk_total(files: &[&IndexedFile]) -> usize {
    files.iter().map(|f| f.chunks.len()).sum()
}

fn manifest_of(entries: &[(&str, u8)]) -> Manifest {
    entries.iter().map(|(p, n)| (path(p), blob(*n))).collect()
}

/// Registers a new, unindexed source named `name` and returns it.
fn register<S: SourceRepository>(store: &S, name: &str) -> Source {
    let source = Source::register(source_id(name), name, location(name));
    store.save(&source).expect("registering a source");
    source
}

/// `source` marked ready at commit number `n`.
fn at_commit(source: &Source, n: u8) -> Source {
    let mut next = source.clone();
    next.mark_ready(
        commit_id(n),
        IndexedAt::from_epoch_seconds(1_000 + i64::from(n)),
    );
    next
}

/// Registers `name` and indexes `files` as commit 1. Returns the committed source.
fn first_index<S: SourceRepository + ChunkStore>(
    store: &S,
    name: &str,
    files: Vec<IndexedFile>,
) -> Source {
    let indexed = at_commit(&register(store, name), 1);
    store
        .commit_index(&indexed, None, &upserts(files))
        .expect("first commit_index");
    indexed
}

/// Commits `changes` on top of `previous` as commit number `n`.
fn next_index<S: ChunkStore>(
    store: &S,
    previous: &Source,
    n: u8,
    changes: &IndexChanges,
) -> Source {
    let next = at_commit(previous, n);
    store
        .commit_index(&next, previous.indexed().map(|r| &r.commit), changes)
        .expect("next commit_index");
    next
}

fn stale_versions() -> IndexVersions {
    IndexVersions {
        tokenizer: IndexVersions::current().tokenizer + 1,
        ..IndexVersions::current()
    }
}

/// `source` with its indexed revision stamped with `versions`.
fn with_versions(source: &Source, versions: IndexVersions) -> Source {
    let revision = source.indexed().expect("an indexed source");
    Source::restore(
        source.id().clone(),
        source.name().to_owned(),
        source.location().clone(),
        source.git_ref().cloned(),
        source.status(),
        Some(IndexedRevision {
            versions,
            ..revision.clone()
        }),
    )
    .expect("a restorable source")
}

/// The current versions with the chunker version changed.
fn other_chunker() -> IndexVersions {
    IndexVersions {
        chunker: IndexVersions::current().chunker + 1,
        ..IndexVersions::current()
    }
}

/// The current versions with the policy version changed.
fn other_policy() -> IndexVersions {
    IndexVersions {
        policy: IndexVersions::current().policy + 1,
        ..IndexVersions::current()
    }
}

/// The re-tokenized fields of every stored chunk of `id`, as `rag reindex` would
/// compute them.
fn reindexed_fields<S: ChunkStore>(store: &S, id: &SourceId) -> Vec<ReindexedChunk> {
    store
        .chunks(id)
        .expect("reading chunks")
        .iter()
        .map(|chunk| ReindexedChunk {
            id: chunk.id.clone(),
            fields: chunk.index_fields(),
        })
        .collect()
}

/// A test seam of a store: puts the stored index of a source into the state an older
/// build of rag leaves behind. The ports cannot do it (`commit_index` and
/// `rewrite_index_fields` stamp the current tokenizer and policy versions only), but
/// the contracts must exercise the stale-index paths, so each store under test
/// implements this over its own storage (an UPDATE for SQLite).
pub trait StampStoredVersions {
    /// Replaces the index versions stamped on the indexed revision of `source`,
    /// leaving everything else untouched. Panics when the source has no revision.
    fn stamp_stored_versions(&self, source: &SourceId, versions: IndexVersions);
}

/// Makes the stored index of `id` look built by an older tokenizer.
fn make_stale<S: StampStoredVersions>(store: &S, id: &SourceId) {
    store.stamp_stored_versions(id, stale_versions());
}

/// Everything observable about one source, to compare before and after a failure.
#[derive(Debug, PartialEq)]
struct Snapshot {
    source: Option<Source>,
    manifest: Manifest,
    chunks: Vec<Chunk>,
}

fn snapshot<S: SourceRepository + ChunkStore>(store: &S, id: &SourceId) -> Snapshot {
    Snapshot {
        source: store.get(id).unwrap(),
        manifest: store.manifest(id).unwrap(),
        chunks: store.chunks(id).unwrap(),
    }
}

// --- SourceRepository -------------------------------------------------------

/// Contract for `SourceRepository`: round trips, upsert, name lookup, name
/// uniqueness (case-insensitive), ordering, and that `save` never writes or changes
/// the indexed revision.
pub fn contract_source_repository<R: SourceRepository>(make: impl Fn() -> R) {
    empty_catalog_knows_nothing(&make());
    saved_sources_round_trip(&make());
    save_replaces_and_list_is_ordered_by_id(&make());
    names_are_unique_case_insensitively(&make());
    save_never_writes_an_indexed_revision(&make());
    save_persists_the_last_error(&make());
}

fn save_persists_the_last_error<R: SourceRepository>(repo: &R) {
    let mut source = Source::register(source_id("one"), "one", location("one"));
    repo.save(&source).unwrap();
    assert_eq!(repo.get(source.id()).unwrap().unwrap().last_error(), None);

    source.record_failure(
        "network_or_auth",
        "git: operation timed out",
        IndexedAt::from_epoch_seconds(1_234),
    );
    repo.save(&source).unwrap();
    let stored = repo.get(source.id()).unwrap().unwrap();
    assert_eq!(stored.status(), SourceStatus::Failed);
    assert_eq!(
        stored.last_error(),
        source.last_error(),
        "code, message, and time round trip"
    );
    assert_eq!(
        repo.list().unwrap()[0].last_error(),
        source.last_error(),
        "list carries the last error too"
    );
    assert_eq!(
        repo.find_by_name("one").unwrap().unwrap().last_error(),
        source.last_error(),
        "and so does the lookup by name"
    );

    source.record_failure("error", "second", IndexedAt::from_epoch_seconds(2_000));
    repo.save(&source).unwrap();
    assert_eq!(
        repo.get(source.id()).unwrap().unwrap().last_error(),
        source.last_error(),
        "a later failure replaces the earlier one"
    );
}

fn empty_catalog_knows_nothing<R: SourceRepository>(repo: &R) {
    assert!(
        repo.list().unwrap().is_empty(),
        "a new repository lists nothing"
    );
    assert_eq!(
        repo.get(&source_id("a")).unwrap(),
        None,
        "get of an unknown id is None"
    );
    assert_eq!(
        repo.find_by_name("a").unwrap(),
        None,
        "unknown name is None"
    );
}

fn saved_sources_round_trip<R: SourceRepository>(repo: &R) {
    // A source with a ref and every lifecycle status that needs no index.
    let mut source = Source::register(source_id("a"), "a", location("a"));
    source.track(&GitRef::parse("release/1.0").unwrap());
    repo.save(&source).unwrap();
    assert_eq!(
        repo.get(source.id()).unwrap(),
        Some(source.clone()),
        "a new source round-trips with its ref"
    );
    for status_change in [
        Source::mark_indexing,
        Source::mark_failed,
        Source::mark_removing,
    ] {
        status_change(&mut source);
        repo.save(&source).unwrap();
        assert_eq!(
            repo.get(source.id()).unwrap(),
            Some(source.clone()),
            "save then get round-trips status {:?}",
            source.status()
        );
    }
    assert_eq!(
        repo.find_by_name("a").unwrap(),
        Some(source.clone()),
        "find_by_name finds it"
    );

    // A source that never finished indexing has no revision.
    let fresh = Source::register(source_id("fresh"), "fresh", location("fresh"));
    repo.save(&fresh).unwrap();
    assert_eq!(repo.get(fresh.id()).unwrap().unwrap().indexed(), None);
}

fn save_never_writes_an_indexed_revision<R: SourceRepository>(repo: &R) {
    let revision = IndexedRevision {
        commit: commit_id(5),
        at: IndexedAt::from_epoch_seconds(50),
        versions: IndexVersions::current(),
    };
    let carrying = |status| {
        Source::restore(
            source_id("a"),
            "a".to_owned(),
            location("a"),
            None,
            status,
            Some(revision.clone()),
        )
        .expect("a restorable source")
    };

    // The revision a caller carries is not stored: only `commit_index` writes one.
    repo.save(&carrying(SourceStatus::Indexing)).unwrap();
    let stored = repo.get(&source_id("a")).unwrap().unwrap();
    assert_eq!(stored.indexed(), None, "save did not write the revision");
    assert_eq!(stored.status(), SourceStatus::Indexing);
    assert!(!stored.is_searchable());

    // So a `Ready` source cannot be saved over a source that has no stored revision.
    assert!(
        matches!(
            repo.save(&carrying(SourceStatus::Ready)),
            Err(StorageError::Failed(_))
        ),
        "Ready without a stored revision is refused"
    );
    assert_eq!(
        repo.get(&source_id("a")).unwrap(),
        Some(stored),
        "the refused save changed nothing"
    );
}

fn save_replaces_and_list_is_ordered_by_id<R: SourceRepository>(repo: &R) {
    let new_source = |name: &str| Source::register(source_id(name), name, location(name));
    let mut a = new_source("a");
    repo.save(&a).unwrap();
    a.mark_failed();
    repo.save(&a).unwrap();
    assert_eq!(
        repo.list().unwrap(),
        vec![a.clone()],
        "save of an existing id replaces it"
    );

    let (b, c) = (new_source("b"), new_source("c"));
    repo.save(&c).unwrap();
    repo.save(&b).unwrap();
    let mut expected = vec![a, b, c];
    expected.sort_by(|x, y| x.id().cmp(y.id()));
    assert_eq!(repo.list().unwrap(), expected, "list is ordered by id");
}

fn names_are_unique_case_insensitively<R: SourceRepository>(repo: &R) {
    let named = |id: &str, name: &str| Source::register(source_id(id), name, location(id));
    let docs = named("docs-a", "Docs");
    repo.save(&docs).unwrap();

    let clash = named("docs-b", "dOCS");
    assert_eq!(
        repo.save(&clash),
        Err(StorageError::NameTaken("dOCS".into())),
        "a name differing only in case is taken"
    );
    assert_eq!(
        repo.get(clash.id()).unwrap(),
        None,
        "the clash was not saved"
    );
    assert_eq!(repo.list().unwrap(), vec![docs.clone()]);

    repo.save(&docs)
        .expect("saving a source again keeps its own name");
    assert_eq!(repo.find_by_name("DOCS").unwrap(), Some(docs.clone()));
    assert_eq!(repo.find_by_name("docs").unwrap(), Some(docs.clone()));

    let unicode = named("uni-a", "\u{c4}rger");
    repo.save(&unicode).unwrap();
    assert!(
        repo.save(&named("uni-b", "\u{e4}rger")).is_err(),
        "case folding covers non-ASCII letters"
    );

    let renamed = named("docs-a", "Manual");
    repo.save(&renamed).expect("a source may be renamed");
    repo.save(&clash).expect("the freed name can be taken");
    assert_eq!(repo.find_by_name("docs").unwrap(), Some(clash));
}

// --- ChunkStore -------------------------------------------------------------

/// Contract for `ChunkStore` (on a type that is also a `SourceRepository`):
/// `commit_index` atomicity, base checks, the version-change coverage rule,
/// validation, replace and removal semantics, manifest and counts, exact chunk round
/// trips and reads by id, `save` leaving the stored revision alone, reindex support
/// (including the tokenizer-only rule of `rewrite_index_fields`), and isolation
/// between sources. The concurrent race is `contract_commit_race`.
pub fn contract_chunk_store<S: ChunkStore + SourceRepository + StampStoredVersions>(
    make: impl Fn() -> S,
) {
    unknown_source_is_empty(&make());
    commit_requires_a_registered_source_with_a_revision(&make());
    first_commit_stores_source_files_and_chunks(&make());
    replacing_a_file_replaces_its_chunks(&make());
    removals_drop_files_and_their_chunks(&make());
    upserts_and_removals_apply_together(&make());
    committing_the_same_changes_again_changes_nothing(&make());
    files_without_chunks_stay_in_the_manifest(&make());
    a_stale_expected_base_is_a_conflict(&make());
    a_failed_commit_leaves_source_row_and_manifest_unchanged(&make());
    foreign_and_duplicate_path_changes_are_rejected(&make());
    duplicate_chunk_ids_are_rejected(&make());
    a_name_clash_fails_the_commit(&make());
    chunks_round_trip_exactly(&make());
    a_chunk_can_be_read_by_id(&make());
    save_leaves_the_stored_index_alone(&make());
    a_version_change_must_cover_every_stored_file(&make());
    only_the_current_tokenizer_and_policy_can_be_stamped(&make());
    rewrite_index_fields_replaces_fields_and_stamps_versions(&make());
    rewrite_index_fields_rejects_a_mismatched_chunk_set(&make());
    rewrite_index_fields_rejects_a_changed_chunker_or_policy_version(&make());
    sources_are_isolated_and_removable(&make());
    chunk_counts_for_paths_count_only_the_named_files(&make());
    a_commit_clears_the_last_error(&make());
}

fn chunk_counts_for_paths_count_only_the_named_files<S: ChunkStore + SourceRepository>(store: &S) {
    let s = source_id("one");
    let doc = file(&s, "a.md", "# One\nalpha\n# Two\nbeta\n# Three\ngamma", 1);
    let code = file(&s, "src/b.rs", "fn b() {}", 2);
    let empty = IndexedFile {
        chunks: Vec::new(),
        ..file(&s, "c.txt", "", 3)
    };
    assert!(doc.chunks.len() > 1, "fixture: several chunks in one file");
    first_index(store, "one", vec![doc.clone(), code.clone(), empty]);

    let count = |paths: &[&str]| {
        let paths: Vec<RepoPath> = paths.iter().map(|p| path(p)).collect();
        store.chunk_count_for_paths(&s, &paths).unwrap()
    };
    assert_eq!(count(&["a.md"]), doc.chunks.len());
    assert_eq!(count(&["a.md", "src/b.rs"]), chunk_total(&[&doc, &code]));
    assert_eq!(
        count(&["c.txt"]),
        0,
        "a stored file without chunks counts 0"
    );
    assert_eq!(count(&["never-stored.rs"]), 0, "unknown paths count 0");
    assert_eq!(count(&[]), 0, "no paths, no chunks");
    assert_eq!(
        count(&["a.md", "a.md"]),
        doc.chunks.len(),
        "a path named twice counts once"
    );
    assert_eq!(
        store
            .chunk_count_for_paths(&source_id("unknown"), &[path("a.md")])
            .unwrap(),
        0,
        "another source's files are not counted"
    );
}

fn a_commit_clears_the_last_error<S: ChunkStore + SourceRepository>(store: &S) {
    let s = source_id("one");
    let mut first = first_index(store, "one", vec![file(&s, "a.rs", "fn a() {}", 1)]);
    first.record_failure("error", "boom", IndexedAt::from_epoch_seconds(5));
    store.save(&first).unwrap();
    assert!(store.get(&s).unwrap().unwrap().last_error().is_some());

    let next = next_index(
        store,
        &first,
        2,
        &upserts(vec![file(&s, "a.rs", "fn a2() {}", 3)]),
    );
    let stored = store.get(&s).unwrap().unwrap();
    assert_eq!(stored, next);
    assert_eq!(stored.last_error(), None, "a good index clears the error");
}

/// How many times `contract_commit_race` repeats the race on a fresh store.
const RACE_ROUNDS: usize = 25;

/// Contract for two concurrent `commit_index` calls from the same base: exactly one
/// wins, the other gets `Conflict`, and the store holds the winner's changes only.
///
/// `rag-core` forbids threads in this layer, so the caller supplies
/// `run_concurrently`: it must start both closures at the same moment on two OS
/// threads and return once both have finished (see `tests/commit_race.rs` for the
/// `std::thread::scope` version). Each adapter runs this against its real store.
pub fn contract_commit_race<S: ChunkStore + SourceRepository>(
    make: impl Fn() -> S,
    run_concurrently: impl Fn(&(dyn Fn() + Sync), &(dyn Fn() + Sync)),
) {
    for _ in 0..RACE_ROUNDS {
        exactly_one_of_two_racing_commits_wins(&make(), &run_concurrently);
    }
}

fn exactly_one_of_two_racing_commits_wins<S: ChunkStore + SourceRepository>(
    store: &S,
    run_concurrently: &impl Fn(&(dyn Fn() + Sync), &(dyn Fn() + Sync)),
) {
    let s = source_id("one");
    let base = first_index(store, "one", vec![file(&s, "a.md", "# One\nalpha", 1)]);
    let contenders: Vec<(Source, IndexChanges)> = [2u8, 3]
        .into_iter()
        .map(|n| {
            let racing_file = file(&s, &format!("c{n}.rs"), &format!("fn c{n}() {{}}"), 10 + n);
            (at_commit(&base, n), upserts(vec![racing_file]))
        })
        .collect();
    let results: Vec<Mutex<Option<Result<(), StorageError>>>> =
        contenders.iter().map(|_| Mutex::new(None)).collect();
    let attempt = |index: usize| {
        let (next, changes) = &contenders[index];
        let result = store.commit_index(next, Some(&commit_id(1)), changes);
        *results[index].lock().expect("results lock") = Some(result);
    };
    run_concurrently(&|| attempt(0), &|| attempt(1));

    let results: Vec<Result<(), StorageError>> = results
        .into_iter()
        .map(|slot| {
            slot.into_inner()
                .expect("results lock")
                .expect("both contenders ran to completion")
        })
        .collect();
    let winners: Vec<usize> = (0..2).filter(|i| results[*i].is_ok()).collect();
    assert_eq!(
        winners.len(),
        1,
        "exactly one racing commit succeeds, got {results:?}"
    );
    let winner = winners[0];
    assert_eq!(
        results[1 - winner],
        Err(StorageError::Conflict),
        "the loser sees a stale base"
    );

    let (next, changes) = &contenders[winner];
    assert_eq!(
        store.get(&s).unwrap().as_ref(),
        Some(next),
        "the source row is the winner's"
    );
    let racing_path = changes.upserts[0].path.as_str();
    assert_eq!(
        store.manifest(&s).unwrap(),
        manifest_of(&[("a.md", 1), (racing_path, 10 + winner as u8 + 2)]),
        "the index holds the base and the winner's file, nothing of the loser's"
    );
}

fn unknown_source_is_empty<S: ChunkStore>(store: &S) {
    let unknown = source_id("unknown");
    assert!(
        store.manifest(&unknown).unwrap().is_empty(),
        "unknown source: empty manifest"
    );
    assert_eq!(
        store.chunk_count(&unknown).unwrap(),
        0,
        "unknown source: zero chunks"
    );
    assert!(
        store.chunks(&unknown).unwrap().is_empty(),
        "unknown source: no chunks"
    );
    assert_eq!(
        store.stored_versions(&unknown).unwrap(),
        None,
        "unknown source: no stored versions"
    );
    store
        .remove_source(&unknown)
        .expect("removing an unknown source is a no-op");
}

fn commit_requires_a_registered_source_with_a_revision<S: ChunkStore + SourceRepository>(
    store: &S,
) {
    let a = file(&source_id("one"), "a.rs", "fn a() {}", 1);
    let unregistered = at_commit(
        &Source::register(source_id("one"), "one", location("one")),
        1,
    );
    assert!(
        store
            .commit_index(&unregistered, None, &upserts(vec![a.clone()]))
            .is_err(),
        "a source must be saved before its first commit_index"
    );
    assert_eq!(store.get(unregistered.id()).unwrap(), None);
    assert!(store.manifest(unregistered.id()).unwrap().is_empty());

    let registered = register(store, "one");
    assert!(
        store
            .commit_index(&registered, None, &upserts(vec![a]))
            .is_err(),
        "a source without an indexed revision cannot be committed"
    );
    assert!(store.manifest(registered.id()).unwrap().is_empty());
    assert_eq!(store.get(registered.id()).unwrap(), Some(registered));
}

fn first_commit_stores_source_files_and_chunks<S: ChunkStore + SourceRepository>(store: &S) {
    let s = source_id("one");
    let doc = file(&s, "a.md", "# One\nalpha\n# Two\nbeta", 1);
    let code = file(&s, "src/b.rs", "fn b() {}", 2);
    assert!(
        doc.chunks.len() > 1,
        "fixture: the markdown file has several chunks"
    );
    let committed = first_index(store, "one", vec![doc.clone(), code.clone()]);
    assert_eq!(
        store.manifest(&s).unwrap(),
        manifest_of(&[("a.md", 1), ("src/b.rs", 2)]),
        "manifest lists every upserted file with its blob"
    );
    assert_eq!(store.chunk_count(&s).unwrap(), chunk_total(&[&doc, &code]));
    assert_eq!(
        store.get(&s).unwrap(),
        Some(committed.clone()),
        "the source row carries the new revision and status"
    );
    assert_eq!(
        store.stored_versions(&s).unwrap(),
        Some(IndexVersions::current()),
        "the index versions are stored with the revision"
    );
}

fn replacing_a_file_replaces_its_chunks<S: ChunkStore + SourceRepository>(store: &S) {
    let s = source_id("one");
    let old = file(&s, "a.md", "# One\nalpha\n# Two\nbeta\n# Three\ngamma", 1);
    let other = file(&s, "b.rs", "fn b() {}", 2);
    let first = first_index(store, "one", vec![old, other.clone()]);

    let new = file(&s, "a.md", "# Only\ndelta", 3);
    next_index(store, &first, 2, &upserts(vec![new.clone()]));
    assert_eq!(
        store.chunk_count(&s).unwrap(),
        chunk_total(&[&new, &other]),
        "old chunks of a replaced file are gone, not appended to"
    );
    assert_eq!(
        store.manifest(&s).unwrap(),
        manifest_of(&[("a.md", 3), ("b.rs", 2)]),
        "the replaced file carries its new blob"
    );
}

fn removals_drop_files_and_their_chunks<S: ChunkStore + SourceRepository>(store: &S) {
    let s = source_id("one");
    let a = file(&s, "a.md", "# One\nalpha", 1);
    let b = file(&s, "b.rs", "fn b() {}", 2);
    let first = first_index(store, "one", vec![a, b.clone()]);

    next_index(store, &first, 2, &removals(&["a.md", "never-stored.rs"]));
    assert_eq!(store.manifest(&s).unwrap(), manifest_of(&[("b.rs", 2)]));
    assert_eq!(store.chunk_count(&s).unwrap(), chunk_total(&[&b]));
}

fn upserts_and_removals_apply_together<S: ChunkStore + SourceRepository>(store: &S) {
    let s = source_id("one");
    let a = file(&s, "a.md", "# One\nalpha", 1);
    let b = file(&s, "b.rs", "fn b() {}", 2);
    let first = first_index(store, "one", vec![a, b.clone()]);

    let c = file(&s, "c.rs", "fn c() {}", 3);
    next_index(
        store,
        &first,
        2,
        &IndexChanges {
            upserts: vec![c.clone()],
            removals: vec![path("a.md")],
        },
    );
    assert_eq!(
        store.manifest(&s).unwrap(),
        manifest_of(&[("b.rs", 2), ("c.rs", 3)])
    );
    assert_eq!(store.chunk_count(&s).unwrap(), chunk_total(&[&b, &c]));
}

fn committing_the_same_changes_again_changes_nothing<S: ChunkStore + SourceRepository>(store: &S) {
    let s = source_id("one");
    let a = file(&s, "a.md", "# One\nalpha\n# Two\nbeta", 1);
    let changes = upserts(vec![a.clone()]);
    let first = first_index(store, "one", vec![a.clone()]);
    next_index(store, &first, 2, &changes);
    assert_eq!(
        store.chunk_count(&s).unwrap(),
        chunk_total(&[&a]),
        "no duplicate chunks"
    );
    assert_eq!(store.manifest(&s).unwrap().len(), 1);
}

fn files_without_chunks_stay_in_the_manifest<S: ChunkStore + SourceRepository>(store: &S) {
    let s = source_id("one");
    let empty = file(&s, "empty.txt", "", 7);
    assert!(
        empty.chunks.is_empty(),
        "fixture: an empty file has no chunks"
    );
    first_index(store, "one", vec![empty]);
    assert_eq!(
        store.manifest(&s).unwrap(),
        manifest_of(&[("empty.txt", 7)]),
        "a file without chunks is still recorded, so it is not re-indexed forever"
    );
    assert_eq!(store.chunk_count(&s).unwrap(), 0);
}

fn a_stale_expected_base_is_a_conflict<S: ChunkStore + SourceRepository>(store: &S) {
    let s = source_id("one");
    let a = file(&s, "a.md", "# One\nalpha", 1);
    let b = file(&s, "b.rs", "fn b() {}", 2);

    // A first index must name no base.
    let registered = register(store, "one");
    let wrong_first = at_commit(&registered, 1);
    assert_eq!(
        store.commit_index(&wrong_first, Some(&commit_id(9)), &upserts(vec![a.clone()])),
        Err(StorageError::Conflict),
        "nothing is indexed yet, so any base is stale"
    );
    assert!(store.manifest(&s).unwrap().is_empty());

    store
        .commit_index(&wrong_first, None, &upserts(vec![a]))
        .unwrap();
    let before = snapshot(store, &s);

    let next = at_commit(&wrong_first, 2);
    let changes = upserts(vec![b]);
    assert_eq!(
        store.commit_index(&next, None, &changes),
        Err(StorageError::Conflict),
        "no base given, but the index is at commit 1"
    );
    assert_eq!(
        store.commit_index(&next, Some(&commit_id(9)), &changes),
        Err(StorageError::Conflict),
        "a base other than the stored commit"
    );
    assert_eq!(snapshot(store, &s), before, "conflicts change nothing");

    store
        .commit_index(&next, Some(&commit_id(1)), &changes)
        .expect("the right base is accepted");
    assert_eq!(store.get(&s).unwrap(), Some(next));
}

fn a_failed_commit_leaves_source_row_and_manifest_unchanged<S: ChunkStore + SourceRepository>(
    store: &S,
) {
    let s = source_id("one");
    let a = file(&s, "a.md", "# One\nalpha", 1);
    let b = file(&s, "b.rs", "fn b() {}", 2);
    let first = first_index(store, "one", vec![a, b]);
    let before = snapshot(store, &s);

    // A valid upsert and a removal travel with a file whose chunks belong to a
    // different source: the whole commit must fail, not just the bad file.
    let fresh = file(&s, "fresh.rs", "fn fresh() {}", 3);
    let foreign = file(&source_id("elsewhere"), "bad.rs", "fn bad() {}", 4);
    let changes = IndexChanges {
        upserts: vec![fresh, foreign],
        removals: vec![path("a.md")],
    };
    let attempt = at_commit(&first, 2);
    assert!(
        store
            .commit_index(&attempt, Some(&commit_id(1)), &changes)
            .is_err(),
        "a change set with a foreign chunk fails"
    );
    assert_eq!(
        snapshot(store, &s),
        before,
        "the source row (commit, time, status), the manifest, and the chunks are as before"
    );
    assert_eq!(store.get(&s).unwrap(), Some(first));
}

fn foreign_and_duplicate_path_changes_are_rejected<S: ChunkStore + SourceRepository>(store: &S) {
    let s = source_id("one");
    let a = file(&s, "a.md", "# One\nalpha", 1);
    let first = first_index(store, "one", vec![a.clone()]);
    let before = snapshot(store, &s);
    let attempt = at_commit(&first, 2);
    let base = Some(&commit_id(1));

    let foreign_source = file(&source_id("elsewhere"), "x.rs", "fn x() {}", 5);
    let mut foreign_path = file(&s, "y.rs", "fn y() {}", 6);
    foreign_path.chunks[0].path = path("not-y.rs");
    let rejected = [
        ("a chunk of another source", upserts(vec![foreign_source])),
        ("a chunk of another path", upserts(vec![foreign_path])),
        (
            "the same path upserted twice",
            upserts(vec![
                file(&s, "dup.rs", "fn one() {}", 7),
                file(&s, "dup.rs", "fn two() {}", 8),
            ]),
        ),
        (
            "a path both upserted and removed",
            IndexChanges {
                upserts: vec![file(&s, "a.md", "# New\nbeta", 9)],
                removals: vec![path("a.md")],
            },
        ),
    ];
    for (what, changes) in rejected {
        assert!(
            store.commit_index(&attempt, base, &changes).is_err(),
            "rejected: {what}"
        );
        assert_eq!(snapshot(store, &s), before, "unchanged after: {what}");
    }
}

fn duplicate_chunk_ids_are_rejected<S: ChunkStore + SourceRepository>(store: &S) {
    let s = source_id("one");
    let a = file(&s, "a.md", "# One\nalpha", 1);
    let first = first_index(store, "one", vec![a.clone()]);
    let before = snapshot(store, &s);
    let attempt = at_commit(&first, 2);
    let base = Some(&commit_id(1));

    // The same id twice inside one file, across two files of one commit, and an id
    // that another stored file keeps.
    let mut within_a_file = file(&s, "b.md", "# Two\nbeta\n# Three\ngamma", 2);
    let first_id = within_a_file.chunks[0].id.clone();
    within_a_file.chunks[1].id = first_id;
    let mut copy = file(&s, "c.md", "# Four\ndelta", 3);
    copy.chunks[0].id = a.chunks[0].id.clone();
    let mut across_files = file(&s, "d.md", "# Five\nepsilon", 4);
    across_files.chunks[0].id = copy.chunks[0].id.clone();
    for (what, changes) in [
        ("twice in one file", upserts(vec![within_a_file])),
        (
            "in two files of one change set",
            upserts(vec![copy.clone(), across_files]),
        ),
        ("equal to a chunk of a stored file", upserts(vec![copy])),
    ] {
        let result = store.commit_index(&attempt, base, &changes);
        assert!(
            matches!(result, Err(StorageError::Failed(_))),
            "a chunk id {what} is Failed, got {result:?}"
        );
        assert_eq!(snapshot(store, &s), before, "unchanged after: {what}");
    }

    // Replacing a file may reuse the ids of the chunks it replaces.
    let same_text = file(&s, "a.md", "# One\nalpha", 5);
    assert_eq!(same_text.chunks[0].id, a.chunks[0].id, "fixture: same id");
    next_index(store, &first, 2, &upserts(vec![same_text]));
}

fn a_name_clash_fails_the_commit<S: ChunkStore + SourceRepository>(store: &S) {
    let one = first_index(
        store,
        "one",
        vec![file(&source_id("one"), "a.rs", "fn a() {}", 1)],
    );
    register(store, "two");
    let before = snapshot(store, one.id());

    let mut renamed = Source::restore(
        one.id().clone(),
        "TWO".to_owned(),
        one.location().clone(),
        None,
        SourceStatus::Ready,
        one.indexed().cloned(),
    )
    .unwrap();
    renamed.mark_ready(commit_id(2), IndexedAt::from_epoch_seconds(2_000));
    assert!(
        matches!(
            store.commit_index(&renamed, Some(&commit_id(1)), &IndexChanges::default()),
            Err(StorageError::NameTaken(_))
        ),
        "a commit may not take another source's name"
    );
    assert_eq!(snapshot(store, one.id()), before);
}

/// Chunks whose stored form is easy to damage: headings holding the ` > ` joiner,
/// empty heading elements, non-ASCII text, every line ending, and a very long line.
fn awkward_chunks(source: &SourceId, file_path: &RepoPath) -> Vec<Chunk> {
    let chunk = |n: usize, headings: &[&str], text: String| Chunk {
        id: ChunkId::parse(
            blake3::hash(format!("awkward-{n}").as_bytes())
                .to_hex()
                .as_str(),
        )
        .expect("a 64-character hex id"),
        source: source.clone(),
        path: file_path.clone(),
        lines: LineRange::new(n * 10 + 1, n * 10 + 3).expect("a valid range"),
        heading_path: headings.iter().map(|h| (*h).to_owned()).collect(),
        text,
        language: Language::from_path(file_path),
    };
    vec![
        chunk(0, &["A > B", "C"], "awkwardone with a joiner".to_owned()),
        chunk(1, &["", "x", ""], "awkwardtwo empty elements".to_owned()),
        chunk(
            2,
            &[
                "Secci\u{f3}n \u{fc}n\u{ef}",
                "\u{65e5}\u{672c}\u{8a9e} \u{2014} \u{442}\u{435}\u{441}\u{442}",
            ],
            "awkwardthree na\u{ef}ve caf\u{e9}".to_owned(),
        ),
        chunk(
            3,
            &[],
            "awkwardfour line one\r\nline two\rline three\ttabbed\n".to_owned(),
        ),
        chunk(
            4,
            &["long"],
            format!("awkwardfive {}", "longlinetoken ".repeat(700)),
        ),
        chunk(5, &[" > ", ">"], "awkwardsix".to_owned()),
    ]
}

fn chunks_round_trip_exactly<S: ChunkStore + SourceRepository>(store: &S) {
    let s = source_id("one");
    let file_path = path("docs/awkward.md");
    let awkward = awkward_chunks(&s, &file_path);
    let stored = IndexedFile {
        path: file_path,
        blob: blob(1),
        size: 1,
        chunks: awkward.clone(),
    };
    first_index(store, "one", vec![stored]);
    assert_eq!(
        store.chunks(&s).unwrap(),
        awkward,
        "chunks come back exactly as committed: heading lists, text bytes, line ranges, ids"
    );
}

fn a_chunk_can_be_read_by_id<S: ChunkStore + SourceRepository + StampStoredVersions>(store: &S) {
    let s = source_id("one");
    let file_path = path("docs/awkward.md");
    let awkward = awkward_chunks(&s, &file_path);
    let other = file(&s, "b.rs", "fn b() {}", 2);
    let stored = IndexedFile {
        path: file_path,
        blob: blob(1),
        size: 1,
        chunks: awkward.clone(),
    };
    let first = first_index(store, "one", vec![stored, other.clone()]);

    for chunk in awkward.iter().chain(&other.chunks) {
        assert_eq!(
            store.chunk(&chunk.id).unwrap().as_ref(),
            Some(chunk),
            "a chunk comes back by id exactly as committed (headings, text bytes, lines)"
        );
    }
    assert_eq!(
        store
            .chunk(&ChunkId::parse(&"f".repeat(64)).unwrap())
            .unwrap(),
        None,
        "an unknown id is None, not an error"
    );

    let replaced = file(&s, "b.rs", "fn replaced() {}", 3);
    let second = next_index(store, &first, 2, &upserts(vec![replaced.clone()]));
    assert_eq!(
        store.chunk(&other.chunks[0].id).unwrap(),
        None,
        "the chunk of a replaced file is gone"
    );
    assert_eq!(
        store.chunk(&replaced.chunks[0].id).unwrap().as_ref(),
        Some(&replaced.chunks[0])
    );

    next_index(store, &second, 3, &removals(&["b.rs"]));
    assert_eq!(
        store.chunk(&replaced.chunks[0].id).unwrap(),
        None,
        "the chunk of a removed file is gone"
    );

    make_stale(store, &s);
    assert_eq!(
        store.chunk(&awkward[0].id).unwrap().as_ref(),
        Some(&awkward[0]),
        "chunks can be read on a stale index"
    );

    store.remove_source(&s).unwrap();
    assert_eq!(
        store.chunk(&awkward[0].id).unwrap(),
        None,
        "a removed source leaves no chunks to read"
    );
}

fn save_leaves_the_stored_index_alone<S: ChunkStore + SourceRepository + StampStoredVersions>(
    store: &S,
) {
    let s = source_id("one");
    let first = first_index(
        store,
        "one",
        vec![file(&s, "a.md", "# One\nalpha\n# Two\nbeta", 1)],
    );
    let before = snapshot(store, &s);

    // A caller with an old copy (no revision) saves a rename and a status change.
    let stale_copy = Source::restore(
        s.clone(),
        "Renamed".to_owned(),
        first.location().clone(),
        first.git_ref().cloned(),
        SourceStatus::Indexing,
        None,
    )
    .unwrap();
    store.save(&stale_copy).unwrap();
    let stored = store.get(&s).unwrap().unwrap();
    assert_eq!(stored.name(), "Renamed", "the other fields were saved");
    assert_eq!(stored.status(), SourceStatus::Indexing);
    assert_eq!(
        stored.indexed(),
        first.indexed(),
        "save did not drop the stored revision"
    );

    // A copy carrying a different revision does not replace it either.
    let other_revision = at_commit(&first, 9);
    store.save(&other_revision).unwrap();
    assert_eq!(
        store.get(&s).unwrap().unwrap().indexed(),
        first.indexed(),
        "save did not write the revision it was given"
    );
    assert_eq!(store.manifest(&s).unwrap(), before.manifest);
    assert_eq!(store.chunks(&s).unwrap(), before.chunks);

    // Stored index versions come back as stored, stale or not.
    make_stale(store, &s);
    assert_eq!(
        store
            .get(&s)
            .unwrap()
            .unwrap()
            .indexed()
            .map(|r| r.versions),
        Some(stale_versions())
    );
}

fn a_version_change_must_cover_every_stored_file<
    S: ChunkStore + SourceRepository + StampStoredVersions,
>(
    store: &S,
) {
    let s = source_id("one");
    let a = file(&s, "a.md", "# One\nalpha", 1);
    let b = file(&s, "b.rs", "fn b() {}", 2);

    // A first index may carry another chunker version: there is nothing stored to
    // contradict.
    let registered = register(store, "one");
    let old = with_versions(&at_commit(&registered, 1), other_chunker());
    store
        .commit_index(&old, None, &upserts(vec![a.clone(), b.clone()]))
        .expect("a first commit may carry another chunker version");
    assert_eq!(store.stored_versions(&s).unwrap(), Some(other_chunker()));

    // Moving to the current versions must rewrite every file, or the index would mix
    // chunks made by two chunkers.
    let repaired = at_commit(&old, 2);
    let base = Some(&commit_id(1));
    let before = snapshot(store, &s);
    let a_again = file(&s, "a.md", "# One\nalpha again", 3);
    for (what, changes) in [
        ("no changes", IndexChanges::default()),
        ("one of two files", upserts(vec![a_again.clone()])),
        ("one removal of two files", removals(&["b.rs"])),
    ] {
        assert!(
            store.commit_index(&repaired, base, &changes).is_err(),
            "rejected: a version change with {what}"
        );
        assert_eq!(snapshot(store, &s), before, "unchanged after: {what}");
    }
    store
        .commit_index(
            &repaired,
            base,
            &IndexChanges {
                upserts: vec![a_again],
                removals: vec![path("b.rs")],
            },
        )
        .expect("upserts and removals together cover every stored file");
    assert_eq!(
        store.stored_versions(&s).unwrap(),
        Some(IndexVersions::current())
    );
    assert_eq!(store.manifest(&s).unwrap(), manifest_of(&[("a.md", 3)]));

    // An index that an older build stamped with another tokenizer, policy, or chunker
    // version is replaced as a whole too: the commit moves it to the current versions.
    let mut previous = repaired;
    let f = |n: u8| file(&s, "c.rs", &format!("fn c{n}() {{}}"), 20 + n);
    for (n, aged) in [
        (3u8, stale_versions()),
        (4, other_policy()),
        (5, other_chunker()),
    ] {
        store.stamp_stored_versions(&s, aged);
        let next = at_commit(&previous, n);
        let before = snapshot(store, &s);
        let base = previous.indexed().map(|r| &r.commit);
        assert!(
            store
                .commit_index(&next, base, &upserts(vec![f(n)]))
                .is_err(),
            "leaving versions {aged:?} with a partial change set is rejected"
        );
        assert_eq!(snapshot(store, &s), before);
        let everything = IndexChanges {
            upserts: vec![file(&s, "a.md", &format!("# Redone {n}"), 30 + n), f(n)],
            removals: Vec::new(),
        };
        store
            .commit_index(&next, base, &everything)
            .expect("a full change set may change versions");
        assert_eq!(
            store.stored_versions(&s).unwrap(),
            Some(IndexVersions::current())
        );
        previous = next;
    }

    // The chunker version is stored as given (the coverage rule above guards it).
    let rechunked = with_versions(&at_commit(&previous, 6), other_chunker());
    let everything = IndexChanges {
        upserts: vec![file(&s, "a.md", "# Rechunked", 40), f(6)],
        removals: Vec::new(),
    };
    store
        .commit_index(
            &rechunked,
            previous.indexed().map(|r| &r.commit),
            &everything,
        )
        .expect("a full change set may carry another chunker version");
    assert_eq!(store.stored_versions(&s).unwrap(), Some(other_chunker()));

    // Without a version change a partial change set is as fine as ever.
    let steady = with_versions(&at_commit(&rechunked, 7), other_chunker());
    store
        .commit_index(
            &steady,
            rechunked.indexed().map(|r| &r.commit),
            &removals(&["c.rs"]),
        )
        .expect("same versions, partial changes");
}

fn only_the_current_tokenizer_and_policy_can_be_stamped<
    S: ChunkStore + SourceRepository + StampStoredVersions,
>(
    store: &S,
) {
    let s = source_id("one");
    let a = file(&s, "a.md", "# One\nalpha", 1);
    let registered = register(store, "one");

    // The index is built by the domain of this build, so a commit that claims other
    // tokenizer or policy versions is refused, first index or not.
    for (what, versions) in [("tokenizer", stale_versions()), ("policy", other_policy())] {
        let claimed = with_versions(&at_commit(&registered, 1), versions);
        let result = store.commit_index(&claimed, None, &upserts(vec![a.clone()]));
        assert!(
            matches!(result, Err(StorageError::Failed(_))),
            "a first commit claiming another {what} version is Failed, got {result:?}"
        );
        assert_eq!(store.get(&s).unwrap(), Some(registered.clone()));
        assert!(store.manifest(&s).unwrap().is_empty());
    }
    let first = next_index(store, &registered, 1, &upserts(vec![a.clone()]));
    let before = snapshot(store, &s);
    for (what, versions) in [("tokenizer", stale_versions()), ("policy", other_policy())] {
        let claimed = with_versions(&at_commit(&first, 2), versions);
        let everything = upserts(vec![file(&s, "a.md", "# Two\nbeta", 2)]);
        assert!(
            store
                .commit_index(&claimed, Some(&commit_id(1)), &everything)
                .is_err(),
            "a later commit claiming another {what} version is refused"
        );
        assert_eq!(snapshot(store, &s), before, "unchanged after: {what}");
    }

    // A rewrite recomputes the fields with this build's tokenizer, so it cannot
    // stamp another tokenizer or policy version either.
    let fields = reindexed_fields(store, &s);
    for (what, versions) in [("tokenizer", stale_versions()), ("policy", other_policy())] {
        assert!(
            store.rewrite_index_fields(&s, &fields, versions).is_err(),
            "a rewrite stamping another {what} version is refused"
        );
        assert_eq!(snapshot(store, &s), before, "unchanged after: {what}");
    }
}

fn rewrite_index_fields_replaces_fields_and_stamps_versions<
    S: ChunkStore + SourceRepository + StampStoredVersions,
>(
    store: &S,
) {
    let s = source_id("one");
    let first = first_index(
        store,
        "one",
        vec![file(&s, "a.md", "# One\nalpha\n# Two\nbeta", 1)],
    );
    let chunks = store.chunks(&s).unwrap();
    let fields: Vec<ReindexedChunk> = chunks
        .iter()
        .map(|c| ReindexedChunk {
            id: c.id.clone(),
            fields: c.index_fields(),
        })
        .collect();

    // An older build left a stale tokenizer version behind: read it, fix it.
    make_stale(store, &s);
    assert_eq!(store.stored_versions(&s).unwrap(), Some(stale_versions()));
    store
        .rewrite_index_fields(&s, &fields, IndexVersions::current())
        .expect("rewrite works on a stale index");
    assert_eq!(
        store.stored_versions(&s).unwrap(),
        Some(IndexVersions::current())
    );
    let after = store.get(&s).unwrap().unwrap();
    let revision = after.indexed().unwrap();
    let before = first.indexed().unwrap();
    assert_eq!(
        (&revision.commit, revision.at),
        (&before.commit, before.at),
        "only the versions of the revision change"
    );
    assert_eq!(
        store.chunks(&s).unwrap(),
        chunks,
        "chunk rows are untouched"
    );
}

fn rewrite_index_fields_rejects_a_mismatched_chunk_set<S: ChunkStore + SourceRepository>(
    store: &S,
) {
    let s = source_id("one");
    first_index(
        store,
        "one",
        vec![file(&s, "a.md", "# One\nalpha\n# Two\nbeta", 1)],
    );
    let all: Vec<ReindexedChunk> = store
        .chunks(&s)
        .unwrap()
        .iter()
        .map(|c| ReindexedChunk {
            id: c.id.clone(),
            fields: c.index_fields(),
        })
        .collect();
    assert!(all.len() > 1, "fixture: several chunks");
    let before = snapshot(store, &s);

    let mut duplicated = all.clone();
    duplicated.push(all[0].clone());
    let mut unknown = all.clone();
    unknown[0].id = ChunkId::parse(&"f".repeat(64)).unwrap();
    for (what, fields) in [
        ("a missing chunk", all[..all.len() - 1].to_vec()),
        ("a duplicated chunk", duplicated),
        ("an unknown chunk", unknown),
        ("no chunks at all", Vec::new()),
    ] {
        assert!(
            store
                .rewrite_index_fields(&s, &fields, IndexVersions::current())
                .is_err(),
            "rejected: {what}"
        );
        assert_eq!(snapshot(store, &s), before, "unchanged after: {what}");
    }
    assert!(
        store
            .rewrite_index_fields(&source_id("unknown"), &[], IndexVersions::current())
            .is_err(),
        "an unknown source cannot be rewritten"
    );
    let unindexed = register(store, "unindexed");
    assert!(
        store
            .rewrite_index_fields(unindexed.id(), &[], IndexVersions::current())
            .is_err(),
        "a source without an indexed revision cannot be rewritten"
    );
}

fn rewrite_index_fields_rejects_a_changed_chunker_or_policy_version<
    S: ChunkStore + SourceRepository,
>(
    store: &S,
) {
    let s = source_id("one");
    first_index(
        store,
        "one",
        vec![file(&s, "a.md", "# One\nalpha\n# Two\nbeta", 1)],
    );
    let fields = reindexed_fields(store, &s);
    let before = snapshot(store, &s);

    // Only the tokenizer may change, and only to the current one: the stored chunks
    // were made by the chunker and policy of the stored versions, and a rewrite
    // cannot remake them.
    for (what, versions) in [
        ("a tokenizer that is not the current one", stale_versions()),
        ("a changed chunker", other_chunker()),
        ("a changed policy", other_policy()),
        (
            "a changed chunker and tokenizer",
            IndexVersions {
                tokenizer: stale_versions().tokenizer,
                ..other_chunker()
            },
        ),
    ] {
        assert!(
            store.rewrite_index_fields(&s, &fields, versions).is_err(),
            "rejected: {what}"
        );
        assert_eq!(snapshot(store, &s), before, "unchanged after: {what}");
        assert_eq!(
            store.stored_versions(&s).unwrap(),
            Some(IndexVersions::current())
        );
    }
}

fn sources_are_isolated_and_removable<S: ChunkStore + SourceRepository>(store: &S) {
    let (one, two) = (source_id("one"), source_id("two"));
    let a = file(&one, "a.md", "# One\nalpha", 1);
    let b = file(&two, "a.md", "# One\nalpha\n# Two\nbeta", 2);
    first_index(store, "one", vec![a.clone()]);
    let second = first_index(store, "two", vec![b.clone()]);
    assert_eq!(
        store.chunk_count(&one).unwrap(),
        chunk_total(&[&a]),
        "counts are per source"
    );
    assert_eq!(store.chunk_count(&two).unwrap(), chunk_total(&[&b]));

    store.remove_source(&one).unwrap();
    assert!(
        store.manifest(&one).unwrap().is_empty(),
        "removed source has no files"
    );
    assert_eq!(store.chunk_count(&one).unwrap(), 0);
    assert_eq!(
        store.get(&one).unwrap(),
        None,
        "removing a source removes its catalog row too"
    );
    assert_eq!(
        store.chunk_count(&two).unwrap(),
        chunk_total(&[&b]),
        "other sources are untouched"
    );
    assert_eq!(store.get(&two).unwrap(), Some(second));
    store.remove_source(&one).expect("removing twice is fine");
}

// --- ChunkSearcher ----------------------------------------------------------

/// Contract for `ChunkSearcher` (on a type that is also a store and a catalog, with
/// the `StampStoredVersions` seam): AND
/// and OR matching, the AND-then-OR policy and paging via `retrieve`, identifier
/// splitting (`parse_config` and `parseConfig` hit the same chunk) with parts that
/// may sit in different columns, no diacritic folding, determinism and the path
/// tie-break (but no promise that more matched words always win), filters applied
/// before the limit, `total_matches`, revision stamping, searchability (an indexed
/// revision and not `Removing`), the stale-version gate (tokenizer, chunker, and
/// policy versions), reindexed fields, visibility of replaced and removed
/// files, the per-file cap, and hostile query text.
pub fn contract_chunk_search<
    S: ChunkStore + SourceRepository + ChunkSearcher + StampStoredVersions,
>(
    make: impl Fn() -> S,
) {
    and_requires_every_word(&loaded(&make()));
    or_matches_any_word(&loaded(&make()));
    retrieve_falls_back_from_and_to_or(&loaded(&make()));
    identifier_forms_hit_the_same_chunk(&loaded(&make()));
    word_parts_may_match_across_columns(&loaded(&make()));
    diacritics_are_not_folded(&loaded(&make()));
    ordering_is_deterministic_with_path_tiebreak(&loaded(&make()));
    hits_carry_the_stored_chunk(&loaded(&make()));
    limit_total_matches_and_misses(&loaded(&make()));
    the_total_is_counted_up_to_a_cap(&make());
    filters_apply_before_the_limit(&make());
    hits_are_stamped_with_their_source_revision(&loaded(&make()));
    searchability_follows_the_indexed_revision(&loaded(&make()));
    stale_versions_block_only_the_sources_a_query_touches(&loaded(&make()));
    a_stale_chunker_or_policy_version_also_requires_a_reindex(&make);
    rewritten_fields_are_what_search_matches(&loaded(&make()));
    replaced_and_removed_files_are_no_longer_found(&loaded(&make()));
    removing_a_source_empties_its_search_results(&loaded(&make()));
    documentation_outranks_tests_at_equal_relevance(&make());
    at_most_two_hits_per_file(&make());
    one_file_with_many_matching_chunks_pages_cleanly(&make());
    hostile_queries_never_error(&loaded(&make()));
}

/// The corpus shared by the scenarios.
struct Corpus<S> {
    store: S,
    one: Source,
    two: Source,
    config: IndexedFile,
    awkward: Vec<Chunk>,
}

fn loaded<S: ChunkStore + SourceRepository>(store: &S) -> Corpus<&S> {
    let (one, two) = (source_id("one"), source_id("two"));
    let config = file(
        &one,
        "src/config.rs",
        "fn parse_config() {\n    load();\n}",
        1,
    );
    let awkward_path = path("docs/awkward.md");
    let awkward = awkward_chunks(&one, &awkward_path);
    let files = vec![
        config.clone(),
        file(&one, "src/server.rs", "fn start_server() { alpha beta }", 2),
        file(&one, "src/other.rs", "alpha only here", 3),
        file(
            &one,
            "docs/guide.md",
            "# Guide\nHow to use parseConfig in practice",
            4,
        ),
        file(&one, "tie/b.txt", "tiebreak", 5),
        file(&one, "tie/a.txt", "tiebreak", 6),
        file(&one, "src/widget/render.rs", "fn tool() {}", 7),
        file(&one, "docs/accents.txt", "na\u{ef}ve caf\u{e9}", 8),
        file(&one, "notes/hostile.txt", "AND OR NOT NEAR x y a b", 9),
        IndexedFile {
            path: awkward_path,
            blob: blob(10),
            size: 1,
            chunks: awkward.clone(),
        },
    ];
    let one = first_index(store, "one", files);
    let two = first_index(
        store,
        "two",
        vec![file(&two, "src/config.py", "def parse_config(): pass", 11)],
    );
    Corpus {
        store,
        one,
        two,
        config,
        awkward,
    }
}

fn query(text: &str) -> Query {
    Query::parse(text).expect("searchable test query")
}

fn paths_of(hits: &[ScoredChunk]) -> Vec<String> {
    hits.iter().map(|h| h.chunk.path.to_string()).collect()
}

fn sorted_paths_of(hits: &[ScoredChunk]) -> Vec<String> {
    let mut paths = paths_of(hits);
    paths.sort();
    paths
}

fn run<S: ChunkSearcher>(s: &S, text: &str, mode: QueryMode) -> SearchOutcome {
    s.search(&query(text), mode, &SearchFilters::default(), 50)
        .expect("search succeeds")
}

fn search<S: ChunkSearcher>(s: &S, text: &str, mode: QueryMode) -> Vec<ScoredChunk> {
    run(s, text, mode).hits
}

fn and_requires_every_word<S: ChunkSearcher>(c: &Corpus<&S>) {
    let hits = search(c.store, "alpha beta", QueryMode::And);
    assert_eq!(
        paths_of(&hits),
        ["src/server.rs"],
        "AND returns only chunks with every word"
    );
}

fn or_matches_any_word<S: ChunkSearcher>(c: &Corpus<&S>) {
    let hits = search(c.store, "alpha beta", QueryMode::OrFallback);
    assert_eq!(
        sorted_paths_of(&hits),
        ["src/other.rs", "src/server.rs"],
        "OR returns chunks with any word (the order between them is not promised)"
    );
}

fn retrieve_falls_back_from_and_to_or<S: ChunkSearcher>(c: &Corpus<&S>) {
    let filters = SearchFilters::default();
    let both = retrieve(c.store, &query("alpha beta"), &filters, 0, 5).unwrap();
    assert_eq!(both.mode, QueryMode::And, "AND hits: no fallback");
    assert_eq!(paths_of(&both.hits), ["src/server.rs"]);

    let fallback = retrieve(c.store, &query("alpha zebra"), &filters, 0, 5).unwrap();
    assert_eq!(
        fallback.mode,
        QueryMode::OrFallback,
        "no AND hits: falls back to OR"
    );
    assert_eq!(
        sorted_paths_of(&fallback.hits),
        ["src/other.rs", "src/server.rs"],
        "OR finds the two chunks with alpha"
    );

    let nothing = retrieve(c.store, &query("zebra yak"), &filters, 0, 5).unwrap();
    assert_eq!(nothing.mode, QueryMode::OrFallback);
    assert!(nothing.hits.is_empty());
    assert_eq!(nothing.total_matches, 0);
}

fn identifier_forms_hit_the_same_chunk<S: ChunkSearcher>(c: &Corpus<&S>) {
    let snake = search(c.store, "parse_config", QueryMode::And);
    let camel = search(c.store, "parseConfig", QueryMode::And);
    assert_eq!(
        snake, camel,
        "parse_config and parseConfig return identical results"
    );
    for text in [
        "parse_config",
        "parseConfig",
        "PARSE_CONFIG",
        "parse config",
        "Config",
    ] {
        let hits = search(c.store, text, QueryMode::And);
        assert!(
            hits.iter().any(|h| h.chunk.id == c.config.chunks[0].id),
            "query {text:?} finds the chunk containing parse_config"
        );
    }
    assert!(
        paths_of(&camel).contains(&"docs/guide.md".to_owned()),
        "a snake_case chunk and a camelCase chunk are both found by either form"
    );
}

fn word_parts_may_match_across_columns<S: ChunkSearcher>(c: &Corpus<&S>) {
    // `render` is only in the path of src/widget/render.rs, `tool` only in its body.
    let hits = search(c.store, "renderTool", QueryMode::And);
    assert_eq!(
        paths_of(&hits),
        ["src/widget/render.rs"],
        "the parts of one word may be found in different columns"
    );
    assert!(
        search(c.store, "renderZebra", QueryMode::And).is_empty(),
        "every part of the word must be present somewhere"
    );
}

fn diacritics_are_not_folded<S: ChunkSearcher>(c: &Corpus<&S>) {
    let finds_accents = |text: &str| {
        paths_of(&search(c.store, text, QueryMode::And)).contains(&"docs/accents.txt".to_owned())
    };
    assert!(
        finds_accents("na\u{ef}ve"),
        "the accented word finds itself"
    );
    assert!(finds_accents("caf\u{e9}"), "the accented word finds itself");
    assert!(!finds_accents("naive"), "naive does not match na\u{ef}ve");
    assert!(!finds_accents("cafe"), "cafe does not match caf\u{e9}");
}
fn ordering_is_deterministic_with_path_tiebreak<S: ChunkSearcher>(c: &Corpus<&S>) {
    let first = search(c.store, "tiebreak", QueryMode::And);
    let second = search(c.store, "tiebreak", QueryMode::And);
    assert_eq!(first, second, "the same search returns the same results");
    assert_eq!(
        paths_of(&first),
        ["tie/a.txt", "tie/b.txt"],
        "equal scores are ordered by path"
    );
    let many = search(
        c.store,
        "alpha parse config tiebreak",
        QueryMode::OrFallback,
    );
    assert!(
        many.windows(2)
            .all(|w| ScoredChunk::ranking_order(&w[0], &w[1]).is_le()),
        "results are in ScoredChunk::ranking_order"
    );
    assert!(
        many.windows(2).all(|w| w[0].score >= w[1].score),
        "scores are higher-is-better"
    );
}

fn hits_carry_the_stored_chunk<S: ChunkSearcher>(c: &Corpus<&S>) {
    let hits = search(c.store, "load", QueryMode::And);
    assert_eq!(hits.len(), 1);
    assert_eq!(
        hits[0].chunk, c.config.chunks[0],
        "the hit is the chunk that was stored"
    );
    for stored in &c.awkward {
        let token = stored
            .text
            .split_whitespace()
            .next()
            .expect("awkward chunks start with a word");
        let found = search(c.store, token, QueryMode::And);
        assert_eq!(
            found.len(),
            1,
            "{token:?} appears in exactly one awkward chunk"
        );
        assert_eq!(
            &found[0].chunk, stored,
            "a hit returns heading lists, text bytes, and line ranges exactly as stored"
        );
    }
}

fn limit_total_matches_and_misses<S: ChunkSearcher>(c: &Corpus<&S>) {
    let all = SearchFilters::default();
    let one = c
        .store
        .search(&query("alpha"), QueryMode::And, &all, 1)
        .unwrap();
    assert_eq!(one.hits.len(), 1, "limit is respected");
    assert_eq!(
        one.total_matches, 2,
        "total_matches counts every matching chunk, not just the returned ones"
    );

    let miss = run(c.store, "zebra", QueryMode::And);
    assert!(miss.hits.is_empty(), "no match: empty result");
    assert_eq!(miss.total_matches, 0);
    assert!(miss.revisions.is_empty());

    let only_two = SearchFilters {
        sources: vec![c.two.id().clone()],
        ..SearchFilters::default()
    };
    let found = c
        .store
        .search(&query("parse_config"), QueryMode::And, &only_two, 50)
        .unwrap();
    assert_eq!(paths_of(&found.hits), ["src/config.py"], "source filter");
    assert_eq!(found.total_matches, 1, "filters reduce the total");

    let python = SearchFilters {
        languages: vec![Language::parse("python").unwrap()],
        ..SearchFilters::default()
    };
    let found = c
        .store
        .search(&query("parse_config"), QueryMode::And, &python, 50)
        .unwrap();
    assert_eq!(paths_of(&found.hits), ["src/config.py"], "language filter");

    let docs = SearchFilters {
        path: Some(PathGlob::parse("docs/**").unwrap()),
        ..SearchFilters::default()
    };
    let found = c
        .store
        .search(&query("parseConfig"), QueryMode::And, &docs, 50)
        .unwrap();
    assert_eq!(paths_of(&found.hits), ["docs/guide.md"], "path glob filter");
}

fn the_total_is_counted_up_to_a_cap<S: ChunkStore + SourceRepository + ChunkSearcher>(store: &S) {
    let s = source_id("one");
    let over = TOTAL_MATCHES_CAP + 5;
    let files: Vec<IndexedFile> = (0..over)
        .map(|i| file(&s, &format!("many/f{i:04}.txt"), "needle", (i % 200) as u8))
        .collect();
    first_index(store, "one", files);
    let filters = SearchFilters::default();

    let capped = store
        .search(&query("needle"), QueryMode::And, &filters, 3)
        .unwrap();
    assert_eq!(capped.hits.len(), 3, "the limit is respected");
    assert_eq!(
        capped.total_matches, TOTAL_MATCHES_CAP,
        "more matches than the cap are reported as the cap"
    );
    assert!(!capped.total_is_exact, "the capped total is a lower bound");

    let few = SearchFilters {
        path: Some(PathGlob::parse("many/f000?.txt").unwrap()),
        ..SearchFilters::default()
    };
    let exact = store
        .search(&query("needle"), QueryMode::And, &few, 3)
        .unwrap();
    assert_eq!(exact.total_matches, 10);
    assert!(exact.total_is_exact, "below the cap the count is exact");

    let at_the_cap = SearchFilters {
        path: Some(PathGlob::parse("many/f0*.txt").unwrap()),
        ..SearchFilters::default()
    };
    let hits_at_cap = store
        .search(&query("needle"), QueryMode::And, &at_the_cap, 3)
        .unwrap();
    assert_eq!(
        (hits_at_cap.total_matches, hits_at_cap.total_is_exact),
        (TOTAL_MATCHES_CAP, true),
        "exactly the cap is still an exact count"
    );
}

fn filters_apply_before_the_limit<S: ChunkStore + SourceRepository + ChunkSearcher>(store: &S) {
    // Five strong matches in `big` outrank the one weak match in `small`, so with a
    // limit of 1 the filtered-out `big` would fill the page if the limit came first.
    let (big, small) = (source_id("big"), source_id("small"));
    let strong: Vec<IndexedFile> = (0..5)
        .map(|i| {
            file(
                &big,
                &format!("needle/n{i}.md"),
                "# needle\nneedle needle",
                10 + i as u8,
            )
        })
        .collect();
    first_index(store, "big", strong);
    first_index(
        store,
        "small",
        vec![file(
            &small,
            "docs/z.rs",
            "fn z() { /* lorem needle ipsum */ }",
            40,
        )],
    );
    let expected = ["docs/z.rs"];
    let narrowed = [
        (
            "source",
            SearchFilters {
                sources: vec![small.clone()],
                ..SearchFilters::default()
            },
        ),
        (
            "language",
            SearchFilters {
                languages: vec![Language::parse("rust").unwrap()],
                ..SearchFilters::default()
            },
        ),
        (
            "path glob",
            SearchFilters {
                path: Some(PathGlob::parse("docs/**").unwrap()),
                ..SearchFilters::default()
            },
        ),
    ];
    for (what, filters) in narrowed {
        let outcome = store
            .search(&query("needle"), QueryMode::And, &filters, 1)
            .unwrap();
        assert_eq!(
            paths_of(&outcome.hits),
            expected,
            "the {what} filter applies before the limit"
        );
        assert_eq!(
            outcome.total_matches, 1,
            "{what} filter: total after filters"
        );
    }
}

fn hits_are_stamped_with_their_source_revision<S: ChunkSearcher>(c: &Corpus<&S>) {
    let outcome = run(c.store, "parse_config", QueryMode::And);
    assert_eq!(
        sorted_paths_of(&outcome.hits).len(),
        3,
        "fixture: both sources hit"
    );
    assert_eq!(
        outcome.revisions.len(),
        2,
        "one revision per source that appears in the hits"
    );
    assert_eq!(
        outcome.revisions.get(c.one.id()),
        c.one.indexed(),
        "revision of source one"
    );
    assert_eq!(
        outcome.revisions.get(c.two.id()),
        c.two.indexed(),
        "revision of source two"
    );

    let only_one = c
        .store
        .search(
            &query("parse_config"),
            QueryMode::And,
            &SearchFilters {
                sources: vec![c.one.id().clone()],
                ..SearchFilters::default()
            },
            50,
        )
        .unwrap();
    assert_eq!(
        only_one.revisions.keys().collect::<Vec<_>>(),
        [c.one.id()],
        "sources without hits get no revision"
    );
}

fn searchability_follows_the_indexed_revision<S: ChunkStore + SourceRepository + ChunkSearcher>(
    c: &Corpus<&S>,
) {
    let never_indexed = register(c.store, "never");
    assert!(!never_indexed.is_searchable());

    // An update in progress and a failed update keep serving the old index with the
    // old commit.
    for lifecycle in [
        Source::mark_indexing as fn(&mut Source),
        Source::mark_failed,
    ] {
        let mut serving = c.one.clone();
        lifecycle(&mut serving);
        c.store.save(&serving).unwrap();
        let outcome = run(c.store, "load", QueryMode::And);
        assert_eq!(
            paths_of(&outcome.hits),
            ["src/config.rs"],
            "status {:?} still searches the stored index",
            serving.status()
        );
        assert_eq!(
            outcome.revisions.get(c.one.id()).map(|r| &r.commit),
            c.one.indexed().map(|r| &r.commit),
            "results carry the old commit"
        );
    }

    // A source being removed is not searchable, though its index still exists.
    let mut removing = c.one.clone();
    removing.mark_removing();
    c.store.save(&removing).unwrap();
    let outcome = run(c.store, "load", QueryMode::And);
    assert!(
        outcome.hits.is_empty() && outcome.revisions.is_empty() && outcome.total_matches == 0,
        "a Removing source never appears in results"
    );
    assert_eq!(
        paths_of(&run(c.store, "parse_config", QueryMode::And).hits),
        ["src/config.py"],
        "other sources are unaffected"
    );
    assert!(
        c.store.chunk_count(c.one.id()).unwrap() > 0,
        "the index itself is kept until the removal completes"
    );
}

fn stale_versions_block_only_the_sources_a_query_touches<
    S: ChunkStore + SourceRepository + ChunkSearcher + StampStoredVersions,
>(
    c: &Corpus<&S>,
) {
    make_stale(c.store, c.two.id());

    let touching_all = c
        .store
        .search(
            &query("parse_config"),
            QueryMode::And,
            &SearchFilters::default(),
            50,
        )
        .unwrap_err();
    assert_eq!(touching_all, StorageError::ReindexRequired);

    let only_two = SearchFilters {
        sources: vec![c.two.id().clone()],
        ..SearchFilters::default()
    };
    assert_eq!(
        c.store
            .search(&query("parse_config"), QueryMode::And, &only_two, 50)
            .unwrap_err(),
        StorageError::ReindexRequired,
        "a query naming the stale source needs a reindex"
    );

    let only_one = SearchFilters {
        sources: vec![c.one.id().clone()],
        ..SearchFilters::default()
    };
    let hits = c
        .store
        .search(&query("parse_config"), QueryMode::And, &only_one, 50)
        .expect("a query that does not touch the stale source still works")
        .hits;
    assert!(!hits.is_empty());

    // The stale state can still be read and repaired.
    assert_eq!(
        c.store.stored_versions(c.two.id()).unwrap(),
        Some(stale_versions())
    );
    let fields = reindexed_fields(c.store, c.two.id());
    c.store
        .rewrite_index_fields(c.two.id(), &fields, IndexVersions::current())
        .unwrap();
    assert!(
        c.store
            .search(
                &query("parse_config"),
                QueryMode::And,
                &SearchFilters::default(),
                50
            )
            .is_ok(),
        "after the rewrite the whole index is searchable again"
    );
}

fn a_stale_chunker_or_policy_version_also_requires_a_reindex<
    S: ChunkStore + SourceRepository + ChunkSearcher + StampStoredVersions,
>(
    make: &impl Fn() -> S,
) {
    for (what, versions) in [
        ("chunker", other_chunker()),
        ("policy", other_policy()),
        ("tokenizer", stale_versions()),
    ] {
        let store = make();
        let s = source_id("one");
        first_index(&store, "one", vec![file(&s, "a.md", "# One\nalpha", 1)]);
        store.stamp_stored_versions(&s, versions);
        assert_eq!(
            store
                .search(
                    &query("alpha"),
                    QueryMode::And,
                    &SearchFilters::default(),
                    10
                )
                .unwrap_err(),
            StorageError::ReindexRequired,
            "an index built with another {what} version is stale"
        );
    }
}

fn rewritten_fields_are_what_search_matches<S: ChunkStore + ChunkSearcher>(c: &Corpus<&S>) {
    let other = &c.store.chunks(c.one.id()).unwrap();
    let target = other
        .iter()
        .find(|chunk| chunk.path.as_str() == "src/other.rs")
        .expect("the corpus has src/other.rs")
        .clone();
    let fields: Vec<ReindexedChunk> = other
        .iter()
        .map(|chunk| {
            let mut fields = chunk.index_fields();
            if chunk.id == target.id {
                fields.body = "zebra".to_owned();
            }
            ReindexedChunk {
                id: chunk.id.clone(),
                fields,
            }
        })
        .collect();
    c.store
        .rewrite_index_fields(c.one.id(), &fields, IndexVersions::current())
        .unwrap();
    assert_eq!(
        paths_of(&search(c.store, "zebra", QueryMode::And)),
        ["src/other.rs"],
        "search matches the rewritten fields"
    );
    assert!(
        !paths_of(&search(c.store, "alpha", QueryMode::And)).contains(&"src/other.rs".to_owned()),
        "the old tokens are gone"
    );
    assert_eq!(
        c.store.chunks(c.one.id()).unwrap().len(),
        other.len(),
        "the chunk rows stay"
    );
}

fn replaced_and_removed_files_are_no_longer_found<S: ChunkStore + ChunkSearcher>(c: &Corpus<&S>) {
    let replacement = file(c.one.id(), "src/other.rs", "zebra now", 90);
    let second = next_index(c.store, &c.one, 2, &upserts(vec![replacement]));
    assert!(
        !paths_of(&search(c.store, "alpha", QueryMode::And)).contains(&"src/other.rs".to_owned()),
        "text of a replaced file is no longer searchable"
    );
    assert_eq!(
        paths_of(&search(c.store, "zebra", QueryMode::And)),
        ["src/other.rs"]
    );

    next_index(c.store, &second, 3, &removals(&["docs/guide.md"]));
    assert!(
        search(c.store, "guide", QueryMode::And).is_empty(),
        "a removed file is no longer searchable"
    );
}

fn removing_a_source_empties_its_search_results<S: ChunkStore + ChunkSearcher>(c: &Corpus<&S>) {
    assert!(!search(c.store, "parse_config", QueryMode::And).is_empty());
    c.store.remove_source(c.one.id()).unwrap();
    let after = run(c.store, "parse_config", QueryMode::And);
    assert_eq!(
        paths_of(&after.hits),
        ["src/config.py"],
        "only the other source is left"
    );
    assert_eq!(after.total_matches, 1);
    c.store.remove_source(c.two.id()).unwrap();
    let empty = run(c.store, "parse_config", QueryMode::And);
    assert!(empty.hits.is_empty() && empty.revisions.is_empty());
    assert_eq!(empty.total_matches, 0);
}

/// The path prior (`domain::path_prior`) is part of the score: at equal raw
/// relevance documentation beats a test even though the path tie-break would put the
/// test first, the adjustment applies before the limit (one slot goes to the doc),
/// and a test that is the only match is still found.
fn documentation_outranks_tests_at_equal_relevance<
    S: ChunkStore + SourceRepository + ChunkSearcher,
>(
    store: &S,
) {
    let s = source_id("one");
    first_index(
        store,
        "one",
        vec![
            file(&s, "a/tests/prior.rs", "priorneedle", 1),
            file(&s, "z/docs/prior.md", "priorneedle", 2),
            file(&s, "only/tests/solo.rs", "soloneedle", 3),
        ],
    );

    let both = search(store, "priorneedle", QueryMode::And);
    assert_eq!(
        paths_of(&both),
        ["z/docs/prior.md", "a/tests/prior.rs"],
        "the documentation outranks the test"
    );
    assert!(both[0].score > both[1].score, "the score carries the prior");

    let one = store
        .search(
            &query("priorneedle"),
            QueryMode::And,
            &SearchFilters::default(),
            1,
        )
        .unwrap();
    assert_eq!(
        paths_of(&one.hits),
        ["z/docs/prior.md"],
        "the prior applies before the limit"
    );

    assert_eq!(
        paths_of(&search(store, "soloneedle", QueryMode::And)),
        ["only/tests/solo.rs"],
        "a test that is the only match still appears"
    );
}

fn at_most_two_hits_per_file<S: ChunkStore + SourceRepository + ChunkSearcher>(store: &S) {
    let s = source_id("one");
    let notes =
        "# A\nneedle one\n# B\nneedle two\n# C\nneedle three\n# D\nneedle four\n# E\nneedle five";
    let notes = file(&s, "notes.md", notes, 1);
    assert_eq!(notes.chunks.len(), 5, "fixture: five sections, five chunks");
    let other = file(&s, "other.md", "# Z\nneedle elsewhere", 2);
    first_index(store, "one", vec![notes, other]);

    let result = retrieve(store, &query("needle"), &SearchFilters::default(), 0, 10).unwrap();
    let from_notes = result
        .hits
        .iter()
        .filter(|h| h.chunk.path.as_str() == "notes.md")
        .count();
    assert_eq!(from_notes, 2, "at most two hits come from one file");
    assert_eq!(
        result.hits.len(),
        3,
        "the other file's hit is still returned"
    );
    assert_eq!(
        result.total_matches, 6,
        "total counts chunks before the cap"
    );
    assert!(!result.has_more);
}

fn one_file_with_many_matching_chunks_pages_cleanly<
    S: ChunkStore + SourceRepository + ChunkSearcher,
>(
    store: &S,
) {
    let s = source_id("one");
    let sections: String = (0..30)
        .map(|i| format!("# S{i}\nneedle section {i}\n"))
        .collect();
    let mut files = vec![file(&s, "big.md", &sections, 1)];
    for (n, name) in ["x.md", "y.md", "z.md"].into_iter().enumerate() {
        files.push(file(&s, name, "# One\nneedle", 2 + n as u8));
    }
    first_index(store, "one", files);

    let filters = SearchFilters::default();
    let q = query("needle");
    let first = retrieve(store, &q, &filters, 0, 2).unwrap();
    assert_eq!(first.total_matches, 33, "every matching chunk is counted");
    assert_eq!(first.hits.len(), 2);
    assert!(first.has_more);

    let second = retrieve(store, &q, &filters, 2, 2).unwrap();
    assert_eq!(second.hits.len(), 2);
    assert!(second.has_more);

    let third = retrieve(store, &q, &filters, 4, 2).unwrap();
    assert_eq!(
        third.hits.len(),
        1,
        "2 hits from big.md plus 3 single files"
    );
    assert!(!third.has_more);

    let mut ids: Vec<_> = [&first, &second, &third]
        .iter()
        .flat_map(|page| page.hits.iter().map(|h| h.chunk.id.clone()))
        .collect();
    ids.sort();
    ids.dedup();
    assert_eq!(ids.len(), 5, "pages do not overlap");
    let everything = retrieve(store, &q, &filters, 0, 50).unwrap();
    let from_big = everything
        .hits
        .iter()
        .filter(|h| h.chunk.path.as_str() == "big.md")
        .count();
    assert_eq!(from_big, 2, "30 matching chunks of one file show up twice");
    assert_eq!(everything.hits.len(), 5);
}

fn hostile_queries_never_error<S: ChunkSearcher>(c: &Corpus<&S>) {
    let hostile = [
        "AND OR NOT NEAR \"x\" -y a:b * (",
        "\"unbalanced",
        "(",
        ")",
        "a OR",
        "NEAR(a b)",
        "x*",
        "col:val",
        "path:src",
        "foo AND",
        "NOT",
        "^start",
        "a +b",
        "{x}",
        "'quote",
        "back\\slash",
        "AND",
        "OR",
        "-",
        "*",
    ];
    for text in hostile {
        let Ok(query) = Query::parse(text) else {
            continue;
        };
        for mode in [QueryMode::And, QueryMode::OrFallback] {
            let outcome = c
                .store
                .search(&query, mode, &SearchFilters::default(), 50)
                .unwrap_or_else(|e| panic!("query {text:?} in {mode:?} failed: {e}"));
            assert!(
                outcome
                    .hits
                    .windows(2)
                    .all(|w| ScoredChunk::ranking_order(&w[0], &w[1]).is_le()),
                "query {text:?}: results stay ordered"
            );
        }
    }
    let found = search(c.store, "AND OR NOT NEAR \"x\" -y a:b * (", QueryMode::And);
    assert!(
        paths_of(&found).contains(&"notes/hostile.txt".to_owned()),
        "reserved words and punctuation are ordinary words"
    );
}

// --- OperationLock ----------------------------------------------------------

/// Contract for `OperationLock`: one holder per source, independent sources,
/// release on drop, and `Busy` on contention.
pub fn contract_operation_lock<L: OperationLock>(make: impl Fn() -> L) {
    let lock = make();
    let (a, b) = (source_id("a"), source_id("b"));

    let held = lock.acquire(&a).expect("a free lock can be taken");
    assert_eq!(
        lock.acquire(&a).unwrap_err(),
        StorageError::Busy,
        "a held lock cannot be taken again"
    );
    let other = lock
        .acquire(&b)
        .expect("different sources lock independently");
    drop(held);
    let again = lock
        .acquire(&a)
        .expect("the lock is released when the guard is dropped");
    assert_eq!(lock.acquire(&a).unwrap_err(), StorageError::Busy);
    drop((again, other));
    lock.acquire(&b).expect("released locks can be retaken");

    waiting_for_a_free_lock_succeeds_at_once(&lock);
    waiting_on_a_held_lock_gives_up_with_a_retryable_error(&lock);
    a_released_lock_is_taken_even_with_a_tiny_wait(&lock);
}

fn waiting_for_a_free_lock_succeeds_at_once<L: OperationLock>(lock: &L) {
    let free = source_id("wait-free");
    let guard = lock
        .acquire_waiting(&free, "add", Duration::from_secs(5))
        .expect("a free lock is taken without waiting");
    assert!(
        matches!(lock.acquire(&free), Err(StorageError::Busy)),
        "the waiting acquire really holds the lock"
    );
    drop(guard);
    lock.acquire(&free).expect("and releases it on drop");
}

fn waiting_on_a_held_lock_gives_up_with_a_retryable_error<L: OperationLock>(lock: &L) {
    let busy = source_id("wait-busy");
    let held = lock
        .acquire_waiting(&busy, "add", Duration::ZERO)
        .expect("the holder");
    for wait in [Duration::ZERO, Duration::from_millis(30)] {
        match lock.acquire_waiting(&busy, "update", wait) {
            Err(StorageError::Busy) => {}
            Err(StorageError::LockedBy(operation)) => assert!(
                operation.as_str().contains("add"),
                "the error names the operation in progress: {operation}"
            ),
            other => panic!("a held lock must not be taken ({wait:?}): {other:?}"),
        }
    }
    drop(held);
    lock.acquire_waiting(&busy, "update", Duration::from_millis(30))
        .expect("free again");
}

fn a_released_lock_is_taken_even_with_a_tiny_wait<L: OperationLock>(lock: &L) {
    // Threads are not available in this layer, so the holder is released before the
    // wait starts; the contract pins that a wait never fails on a free lock.
    let id = source_id("wait-released");
    drop(lock.acquire(&id).expect("holder"));
    lock.acquire_waiting(&id, "update", Duration::from_millis(1))
        .expect("a released lock is taken even with a tiny wait");
}
