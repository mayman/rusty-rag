//! `rag_infra::testing::age_index`: lets end-to-end tests of other crates leave an
//! index the way an older build would have, since no port can stamp old versions.

use rag_core::application::ports::{ChunkStore, IndexChanges, SourceRepository};
use rag_core::domain::chunker::Chunker;
use rag_core::domain::git_location::GitLocation;
use rag_core::domain::index_versions::IndexVersions;
use rag_core::domain::indexed_at::IndexedAt;
use rag_core::domain::oid::{BlobOid, CommitId};
use rag_core::domain::repo_path::RepoPath;
use rag_core::domain::source::Source;
use rag_core::domain::source_id::SourceId;
use rag_infra::sqlite::SqliteStore;
use rag_infra::testing::age_index;

fn indexed_store(dir: &std::path::Path) -> (SqliteStore, SourceId) {
    let location = GitLocation::parse("https://example.com/o/r").unwrap();
    let id = SourceId::from_location(&location).unwrap();
    let store = SqliteStore::open_for_write(dir).unwrap();
    let mut source = Source::register(id.clone(), "r", location);
    store.save(&source).unwrap();
    source.mark_ready(
        CommitId::parse(&"11".repeat(20)).unwrap(),
        IndexedAt::from_epoch_seconds(5),
    );
    let path = RepoPath::from_git("a.rs").unwrap();
    let text = "fn a() {}";
    let file = rag_core::application::ports::IndexedFile {
        chunks: Chunker::chunk(&id, &path, text),
        path,
        blob: BlobOid::parse(&"22".repeat(20)).unwrap(),
        size: text.len() as u64,
    };
    store
        .commit_index(
            &source,
            None,
            &IndexChanges {
                upserts: vec![file],
                removals: Vec::new(),
            },
        )
        .unwrap();
    (store, id)
}

#[test]
fn an_aged_index_reports_the_old_versions() {
    let dir = tempfile::tempdir().unwrap();
    let (store, id) = indexed_store(dir.path());
    assert!(store.stored_versions(&id).unwrap().unwrap().is_current());

    age_index(
        dir.path(),
        IndexVersions {
            tokenizer: 0,
            ..IndexVersions::current()
        },
    );

    let aged = store.stored_versions(&id).unwrap().unwrap();
    assert!(!aged.is_current());
    assert_eq!(aged.tokenizer, 0);
}
