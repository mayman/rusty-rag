//! The shared store contracts of `rag-core::application::testkit`, run against the
//! real SQLite adapter. Every store lives in its own temp directory, which is
//! removed when the store is dropped.

use rag_core::application::error::StorageError;
use rag_core::application::ports::{
    ChunkSearcher, ChunkStore, IndexChanges, ReindexedChunk, SearchOutcome, SourceRepository,
};
use rag_core::application::testkit::{
    contract_chunk_search, contract_chunk_store, contract_commit_race, contract_source_repository,
    StampStoredVersions,
};
use rag_core::domain::chunk::Chunk;
use rag_core::domain::chunk_id::ChunkId;
use rag_core::domain::index_versions::IndexVersions;
use rag_core::domain::manifest::Manifest;
use rag_core::domain::oid::CommitId;
use rag_core::domain::query::{Query, QueryMode};
use rag_core::domain::search_filters::SearchFilters;
use rag_core::domain::source::Source;
use rag_core::domain::source_id::SourceId;
use rag_infra::sqlite::{SqliteStore, DATABASE_FILE};
use tempfile::TempDir;

/// A store plus the temp directory that holds its database.
struct TempStore {
    store: SqliteStore,
    dir: TempDir,
}

fn temp_store() -> TempStore {
    let dir = tempfile::tempdir().expect("temp dir");
    let store = SqliteStore::open_for_write(dir.path()).expect("open store");
    TempStore { store, dir }
}

impl SourceRepository for TempStore {
    fn get(&self, id: &SourceId) -> Result<Option<Source>, StorageError> {
        self.store.get(id)
    }
    fn find_by_name(&self, name: &str) -> Result<Option<Source>, StorageError> {
        self.store.find_by_name(name)
    }
    fn list(&self) -> Result<Vec<Source>, StorageError> {
        self.store.list()
    }
    fn save(&self, source: &Source) -> Result<(), StorageError> {
        self.store.save(source)
    }
}

impl ChunkStore for TempStore {
    fn commit_index(
        &self,
        source: &Source,
        expected_base: Option<&CommitId>,
        changes: &IndexChanges,
    ) -> Result<(), StorageError> {
        self.store.commit_index(source, expected_base, changes)
    }
    fn remove_source(&self, id: &SourceId) -> Result<(), StorageError> {
        self.store.remove_source(id)
    }
    fn manifest(&self, source: &SourceId) -> Result<Manifest, StorageError> {
        self.store.manifest(source)
    }
    fn chunk_count(&self, source: &SourceId) -> Result<usize, StorageError> {
        self.store.chunk_count(source)
    }
    fn chunk(&self, id: &ChunkId) -> Result<Option<Chunk>, StorageError> {
        self.store.chunk(id)
    }
    fn chunks(&self, source: &SourceId) -> Result<Vec<Chunk>, StorageError> {
        self.store.chunks(source)
    }
    fn stored_versions(&self, source: &SourceId) -> Result<Option<IndexVersions>, StorageError> {
        self.store.stored_versions(source)
    }
    fn rewrite_index_fields(
        &self,
        source: &SourceId,
        fields: &[ReindexedChunk],
        versions: IndexVersions,
    ) -> Result<(), StorageError> {
        self.store.rewrite_index_fields(source, fields, versions)
    }
}

impl ChunkSearcher for TempStore {
    fn search(
        &self,
        query: &Query,
        mode: QueryMode,
        filters: &SearchFilters,
        limit: usize,
    ) -> Result<SearchOutcome, StorageError> {
        self.store.search(query, mode, filters, limit)
    }
}

/// Ages a stored index the way an older build would have left it: an UPDATE through
/// a second connection, since no port can stamp other tokenizer or policy versions.
impl StampStoredVersions for TempStore {
    fn stamp_stored_versions(&self, source: &SourceId, versions: IndexVersions) {
        let connection = rusqlite::Connection::open(self.dir.path().join(DATABASE_FILE))
            .expect("second connection");
        connection
            .busy_timeout(std::time::Duration::from_secs(10))
            .expect("busy timeout");
        let changed = connection
            .execute(
                "UPDATE sources SET tokenizer_version = ?2, chunker_version = ?3, \
                 policy_version = ?4 WHERE source_id = ?1 AND indexed_commit IS NOT NULL",
                (
                    source.as_str(),
                    versions.tokenizer,
                    versions.chunker,
                    versions.policy,
                ),
            )
            .expect("stamp versions");
        assert_eq!(changed, 1, "an indexed source to stamp");
    }
}

#[test]
fn sqlite_store_satisfies_the_source_repository_contract() {
    contract_source_repository(temp_store);
}

#[test]
fn sqlite_store_satisfies_the_chunk_store_contract() {
    contract_chunk_store(temp_store);
}

#[test]
fn sqlite_store_satisfies_the_chunk_search_contract() {
    contract_chunk_search(temp_store);
}

#[test]
fn sqlite_store_lets_exactly_one_of_two_racing_commits_win() {
    contract_commit_race(temp_store, |a, b| {
        let barrier = std::sync::Barrier::new(2);
        let barrier = &barrier;
        std::thread::scope(|scope| {
            let first = scope.spawn(move || {
                barrier.wait();
                a();
            });
            let second = scope.spawn(move || {
                barrier.wait();
                b();
            });
            first.join().expect("first contender");
            second.join().expect("second contender");
        });
    });
}

#[test]
fn sqlite_store_is_send_and_sync() {
    fn assert_send_sync<T: Send + Sync>() {}
    assert_send_sync::<SqliteStore>();
}
