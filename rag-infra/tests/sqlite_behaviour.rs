//! SQLite-specific behavior beyond the shared contracts: WAL, the pinned bm25 column
//! weights, writer-lock contention, crash safety, persistence, and cross-connection
//! races. Each test has its own temp directory.

use std::time::Duration;

use rag_core::application::error::StorageError;
use rag_core::application::ports::{
    ChunkSearcher, ChunkStore, IndexChanges, IndexedFile, ReindexedChunk, SourceRepository,
};
use rag_core::domain::chunk::IndexFields;
use rag_core::domain::chunker::Chunker;
use rag_core::domain::git_location::GitLocation;
use rag_core::domain::index_versions::IndexVersions;
use rag_core::domain::indexed_at::IndexedAt;
use rag_core::domain::oid::{BlobOid, CommitId};
use rag_core::domain::query::{Query, QueryMode};
use rag_core::domain::repo_path::RepoPath;
use rag_core::domain::search_filters::SearchFilters;
use rag_core::domain::source::Source;
use rag_core::domain::source_id::SourceId;
use rag_infra::sqlite::{SqliteStore, DATABASE_FILE};
use rusqlite::Connection;
use tempfile::TempDir;

fn source_id(name: &str) -> SourceId {
    SourceId::from_location(&location(name)).expect("source id")
}

fn location(name: &str) -> GitLocation {
    GitLocation::parse(&format!("https://example.com/o/{name}")).expect("location")
}

fn commit_id(n: u8) -> CommitId {
    CommitId::parse(&format!("{n:02x}").repeat(20)).expect("commit")
}

fn file(source: &SourceId, path: &str, text: &str, n: u8) -> IndexedFile {
    let path = RepoPath::parse(path).expect("path");
    IndexedFile {
        chunks: Chunker::chunk(source, &path, text),
        path,
        blob: BlobOid::parse(&format!("{n:02x}").repeat(20)).expect("blob"),
        size: text.len() as u64,
    }
}

fn upserts(files: Vec<IndexedFile>) -> IndexChanges {
    IndexChanges {
        upserts: files,
        removals: Vec::new(),
    }
}

fn ready(source: &Source, n: u8) -> Source {
    let mut next = source.clone();
    next.mark_ready(
        commit_id(n),
        IndexedAt::from_epoch_seconds(1_000 + i64::from(n)),
    );
    next
}

fn open(dir: &TempDir) -> SqliteStore {
    SqliteStore::open_for_write(dir.path()).expect("open")
}

/// Registers `name` and commits `files` as commit 1.
fn index(store: &SqliteStore, name: &str, files: Vec<IndexedFile>) -> Source {
    let registered = Source::register(source_id(name), name, location(name));
    store.save(&registered).expect("save");
    let indexed = ready(&registered, 1);
    store
        .commit_index(&indexed, None, &upserts(files))
        .expect("commit");
    indexed
}

fn find(store: &SqliteStore, text: &str) -> Vec<String> {
    store
        .search(
            &Query::parse(text).expect("query"),
            QueryMode::And,
            &SearchFilters::default(),
            50,
        )
        .expect("search")
        .hits
        .iter()
        .map(|hit| hit.chunk.path.to_string())
        .collect()
}

fn raw(dir: &TempDir) -> Connection {
    Connection::open(dir.path().join(DATABASE_FILE)).expect("raw connection")
}

#[test]
fn the_database_runs_in_wal_mode() {
    let dir = tempfile::tempdir().unwrap();
    let _store = open(&dir);
    let mode: String = raw(&dir)
        .query_row("PRAGMA journal_mode", [], |row| row.get(0))
        .unwrap();
    assert_eq!(mode, "wal");
}

#[test]
fn open_creates_a_missing_data_directory() {
    let dir = tempfile::tempdir().unwrap();
    let nested = dir.path().join("a").join("b");
    assert!(SqliteStore::open_for_write(&nested).is_ok());
    assert!(nested.join(DATABASE_FILE).is_file());
}

#[test]
fn data_survives_reopening_the_store() {
    let dir = tempfile::tempdir().unwrap();
    let source = {
        let store = open(&dir);
        index(
            &store,
            "one",
            vec![file(&source_id("one"), "a.md", "# One\nalpha", 1)],
        )
    };
    let reopened = open(&dir);
    assert_eq!(reopened.get(source.id()).unwrap().as_ref(), Some(&source));
    assert_eq!(find(&reopened, "alpha"), ["a.md"]);
}

#[test]
fn name_keys_are_unique_in_the_database_itself() {
    let dir = tempfile::tempdir().unwrap();
    let store = open(&dir);
    index(&store, "Alpha", Vec::new());
    let duplicate = raw(&dir).execute(
        "INSERT INTO sources (source_id, name, name_key, location, status) \
         VALUES ('other', 'ALPHA', 'alpha', 'x', 'cloning')",
        [],
    );
    assert!(
        duplicate.is_err(),
        "the UNIQUE index on name_key rejects it"
    );
    assert!(store.find_by_name("aLpHa").unwrap().is_some());
}

#[test]
fn bm25_weights_rank_a_path_hit_over_a_heading_hit_over_a_body_hit() {
    let dir = tempfile::tempdir().unwrap();
    let store = open(&dir);
    let s = source_id("one");
    let indexed = index(
        &store,
        "one",
        vec![
            file(&s, "by-body.txt", "x", 1),
            file(&s, "by-heading.txt", "x", 2),
            file(&s, "by-path.txt", "x", 3),
        ],
    );
    // Give the three chunks the same shape (one token per column, so equal lengths)
    // and put the term in a different column of each.
    let fields = |path: &str, heading: &str, body: &str| IndexFields {
        path: path.to_owned(),
        heading: heading.to_owned(),
        body: body.to_owned(),
    };
    let placement = [
        ("by-body.txt", fields("p", "h", "needle")),
        ("by-heading.txt", fields("p", "needle", "b")),
        ("by-path.txt", fields("needle", "h", "b")),
    ];
    let reindexed: Vec<ReindexedChunk> = store
        .chunks(indexed.id())
        .unwrap()
        .iter()
        .map(|chunk| ReindexedChunk {
            id: chunk.id.clone(),
            fields: placement
                .iter()
                .find(|(path, _)| *path == chunk.path.as_str())
                .map(|(_, f)| f.clone())
                .expect("known path"),
        })
        .collect();
    store
        .rewrite_index_fields(indexed.id(), &reindexed, IndexVersions::current())
        .unwrap();

    assert_eq!(
        find(&store, "needle"),
        ["by-path.txt", "by-heading.txt", "by-body.txt"]
    );
}

#[test]
fn scores_are_positive_and_higher_means_better() {
    let dir = tempfile::tempdir().unwrap();
    let store = open(&dir);
    let s = source_id("one");
    index(
        &store,
        "one",
        vec![
            file(&s, "a.txt", "needle needle needle", 1),
            file(&s, "b.txt", "needle among many many other words here", 2),
        ],
    );
    let hits = store
        .search(
            &Query::parse("needle").unwrap(),
            QueryMode::And,
            &SearchFilters::default(),
            10,
        )
        .unwrap()
        .hits;
    assert!(hits.iter().all(|hit| hit.score > 0.0));
    assert!(hits[0].score >= hits[1].score);
}

#[test]
fn a_writer_that_cannot_get_the_lock_in_time_is_busy() {
    let dir = tempfile::tempdir().unwrap();
    let store =
        SqliteStore::open_for_write_with_busy_timeout(dir.path(), Duration::from_millis(50))
            .unwrap();
    let source = Source::register(source_id("one"), "one", location("one"));

    let blocker = raw(&dir);
    blocker.execute_batch("BEGIN IMMEDIATE").unwrap();
    assert_eq!(store.save(&source), Err(StorageError::Busy));
    assert_eq!(
        store.commit_index(&ready(&source, 1), None, &IndexChanges::default()),
        Err(StorageError::Busy)
    );
    assert_eq!(store.remove_source(source.id()), Err(StorageError::Busy));

    blocker.execute_batch("ROLLBACK").unwrap();
    assert_eq!(store.save(&source), Ok(()));
}

#[test]
fn readers_and_searches_are_not_blocked_by_a_writer() {
    let dir = tempfile::tempdir().unwrap();
    let store =
        SqliteStore::open_for_write_with_busy_timeout(dir.path(), Duration::from_millis(50))
            .unwrap();
    let s = source_id("one");
    index(&store, "one", vec![file(&s, "a.md", "# One\nalpha", 1)]);

    let blocker = raw(&dir);
    blocker.execute_batch("BEGIN IMMEDIATE").unwrap();
    blocker
        .execute("UPDATE sources SET status = 'failed'", [])
        .unwrap();
    assert_eq!(find(&store, "alpha"), ["a.md"], "search ignores the writer");
    assert_eq!(store.list().unwrap().len(), 1);
    assert_eq!(
        store.get(&s).unwrap().unwrap().status().as_str(),
        "ready",
        "uncommitted writes are invisible"
    );
    blocker.execute_batch("ROLLBACK").unwrap();
}

#[test]
fn two_stores_on_one_database_race_for_the_same_base() {
    let dir = tempfile::tempdir().unwrap();
    let first = open(&dir);
    let second = open(&dir);
    let s = source_id("one");
    let base = index(&first, "one", vec![file(&s, "a.md", "# One\nalpha", 1)]);

    let attempt = |store: &SqliteStore, n: u8| {
        let next = ready(&base, n);
        let changes = upserts(vec![file(&s, &format!("c{n}.rs"), "fn c() {}", n + 10)]);
        store.commit_index(&next, Some(&commit_id(1)), &changes)
    };
    let (a, b) = std::thread::scope(|scope| {
        let a = scope.spawn(|| attempt(&first, 2));
        let b = scope.spawn(|| attempt(&second, 3));
        (a.join().unwrap(), b.join().unwrap())
    });
    let outcomes = [a, b];
    assert_eq!(
        outcomes.iter().filter(|r| r.is_ok()).count(),
        1,
        "{outcomes:?}"
    );
    assert!(
        outcomes.contains(&Err(StorageError::Conflict)),
        "{outcomes:?}"
    );
}

/// Installs a trigger that aborts any insert of a chunk whose text is `poison`, so a
/// commit can be made to fail halfway through its writes.
fn poison_chunk_inserts(dir: &TempDir) {
    raw(dir)
        .execute_batch(
            "CREATE TRIGGER poison BEFORE INSERT ON chunks WHEN NEW.text = 'poison' \
             BEGIN SELECT RAISE(ABORT, 'poisoned chunk'); END;",
        )
        .unwrap();
}

#[test]
fn a_commit_that_fails_halfway_leaves_everything_unchanged() {
    let dir = tempfile::tempdir().unwrap();
    let store = open(&dir);
    let s = source_id("one");
    let base = index(
        &store,
        "one",
        vec![
            file(&s, "keep.md", "# Keep\nalpha", 1),
            file(&s, "drop.md", "# Drop\nbeta", 2),
        ],
    );
    poison_chunk_inserts(&dir);

    let before = (
        store.get(&s).unwrap(),
        store.manifest(&s).unwrap(),
        store.chunks(&s).unwrap(),
    );
    // Removes drop.md, replaces keep.md, and then fails on the poisoned file.
    let changes = IndexChanges {
        upserts: vec![
            file(&s, "keep.md", "# Keep\ngamma", 3),
            file(&s, "new.txt", "poison", 4),
        ],
        removals: vec![RepoPath::parse("drop.md").unwrap()],
    };
    let result = store.commit_index(&ready(&base, 2), Some(&commit_id(1)), &changes);
    assert!(matches!(result, Err(StorageError::Failed(_))), "{result:?}");

    let after = (
        store.get(&s).unwrap(),
        store.manifest(&s).unwrap(),
        store.chunks(&s).unwrap(),
    );
    assert_eq!(
        before, after,
        "source row, manifest, and chunks are unchanged"
    );
    assert_eq!(
        find(&store, "alpha"),
        ["keep.md"],
        "old text still searchable"
    );
    assert_eq!(
        find(&store, "beta"),
        ["drop.md"],
        "removed file still searchable"
    );
    assert!(
        find(&store, "gamma").is_empty(),
        "no trace of the failed commit"
    );
}

#[test]
fn a_failed_commit_does_not_block_the_next_one() {
    let dir = tempfile::tempdir().unwrap();
    let store = open(&dir);
    let s = source_id("one");
    let base = index(&store, "one", vec![file(&s, "a.md", "# A\nalpha", 1)]);
    poison_chunk_inserts(&dir);
    let poisoned = upserts(vec![file(&s, "p.txt", "poison", 2)]);
    assert!(store
        .commit_index(&ready(&base, 2), Some(&commit_id(1)), &poisoned)
        .is_err());

    let fine = upserts(vec![file(&s, "b.md", "# B\ndelta", 3)]);
    store
        .commit_index(&ready(&base, 2), Some(&commit_id(1)), &fine)
        .expect("the same base is still current");
    assert_eq!(find(&store, "delta"), ["b.md"]);
}

#[test]
fn a_failed_tokenizer_rewrite_leaves_the_old_fields_and_versions() {
    let dir = tempfile::tempdir().unwrap();
    let store = open(&dir);
    let s = source_id("one");
    let indexed = index(&store, "one", vec![file(&s, "a.md", "# A\nalpha", 1)]);
    let mut fields: Vec<ReindexedChunk> = store
        .chunks(&s)
        .unwrap()
        .iter()
        .map(|c| ReindexedChunk {
            id: c.id.clone(),
            fields: c.index_fields(),
        })
        .collect();
    fields[0].fields.body = "zebra".to_owned();
    let mismatched = IndexVersions {
        chunker: IndexVersions::current().chunker + 1,
        ..IndexVersions::current()
    };
    assert!(store.rewrite_index_fields(&s, &fields, mismatched).is_err());
    assert_eq!(find(&store, "alpha"), ["a.md"]);
    assert!(find(&store, "zebra").is_empty());
    assert_eq!(store.get(&s).unwrap().as_ref(), Some(&indexed));
}

#[test]
fn removing_a_source_leaves_no_orphan_rows() {
    let dir = tempfile::tempdir().unwrap();
    let store = open(&dir);
    let s = source_id("one");
    index(&store, "one", vec![file(&s, "a.md", "# A\nalpha", 1)]);
    store.remove_source(&s).unwrap();
    let connection = raw(&dir);
    for table in ["sources", "files", "chunks", "chunks_fts"] {
        let rows: i64 = connection
            .query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(rows, 0, "{table} is empty");
    }
}

#[test]
fn versions_are_stamped_per_source_in_the_database() {
    let dir = tempfile::tempdir().unwrap();
    let store = open(&dir);
    index(&store, "one", Vec::new());
    let stamped: (i64, i64, i64) = raw(&dir)
        .query_row(
            "SELECT tokenizer_version, chunker_version, policy_version FROM sources",
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .unwrap();
    let current = IndexVersions::current();
    assert_eq!(
        stamped,
        (
            i64::from(current.tokenizer),
            i64::from(current.chunker),
            i64::from(current.policy)
        )
    );
}

fn directory_listing(dir: &TempDir) -> Vec<String> {
    let mut names: Vec<String> = std::fs::read_dir(dir.path())
        .unwrap()
        .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    names
}

/// Makes a directory unwritable for the current user until dropped (an ACL deny on
/// Windows, mode 0555 elsewhere).
struct UnwritableDir {
    path: std::path::PathBuf,
}

impl UnwritableDir {
    #[cfg(windows)]
    fn new(path: &std::path::Path) -> Self {
        let user = std::env::var("USERNAME").expect("USERNAME");
        let status = std::process::Command::new("icacls")
            .arg(path)
            .arg("/deny")
            .arg(format!("{user}:(OI)(CI)(WD,AD)"))
            .output()
            .expect("icacls")
            .status;
        assert!(status.success(), "icacls /deny failed");
        Self { path: path.into() }
    }

    #[cfg(unix)]
    fn new(path: &std::path::Path) -> Self {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o555)).expect("chmod");
        Self { path: path.into() }
    }
}

impl Drop for UnwritableDir {
    #[cfg(windows)]
    fn drop(&mut self) {
        let user = std::env::var("USERNAME").expect("USERNAME");
        let _ = std::process::Command::new("icacls")
            .arg(&self.path)
            .arg("/remove:d")
            .arg(user)
            .output();
    }

    #[cfg(unix)]
    fn drop(&mut self) {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&self.path, std::fs::Permissions::from_mode(0o755));
    }
}

#[test]
fn open_for_read_reports_a_missing_index_and_creates_nothing() {
    let dir = tempfile::tempdir().unwrap();
    assert!(matches!(
        SqliteStore::open_for_read(dir.path()),
        Err(StorageError::IndexMissing)
    ));
    let absent = dir.path().join("not").join("there");
    assert!(matches!(
        SqliteStore::open_for_read(&absent),
        Err(StorageError::IndexMissing)
    ));
    assert!(directory_listing(&dir).is_empty(), "nothing was created");

    std::fs::write(dir.path().join(DATABASE_FILE), b"").unwrap();
    assert!(
        matches!(
            SqliteStore::open_for_read(dir.path()),
            Err(StorageError::IndexMissing)
        ),
        "a file without the schema is no index"
    );
    assert_eq!(
        std::fs::metadata(dir.path().join(DATABASE_FILE))
            .unwrap()
            .len(),
        0,
        "the read path did not initialise it"
    );
}

#[test]
fn open_for_read_searches_and_changes_nothing_on_disk() {
    let dir = tempfile::tempdir().unwrap();
    {
        let store = open(&dir);
        let s = source_id("one");
        index(&store, "one", vec![file(&s, "a.md", "# One\nalpha", 1)]);
    }
    let before = directory_listing(&dir);
    let modified = std::fs::metadata(dir.path().join(DATABASE_FILE))
        .unwrap()
        .modified()
        .unwrap();

    let reader = SqliteStore::open_for_read(dir.path()).expect("open for read");
    assert_eq!(find(&reader, "alpha"), ["a.md"]);
    assert_eq!(reader.list().unwrap().len(), 1);
    drop(reader);

    assert_eq!(directory_listing(&dir), before, "no files were created");
    assert_eq!(
        std::fs::metadata(dir.path().join(DATABASE_FILE))
            .unwrap()
            .modified()
            .unwrap(),
        modified,
        "the database file was not written"
    );
}

#[test]
fn open_for_read_succeeds_while_another_connection_holds_begin_immediate() {
    let dir = tempfile::tempdir().unwrap();
    {
        let store = open(&dir);
        let s = source_id("one");
        index(&store, "one", vec![file(&s, "a.md", "# One\nalpha", 1)]);
    }
    let blocker = raw(&dir);
    blocker.execute_batch("BEGIN IMMEDIATE").unwrap();

    let started = std::time::Instant::now();
    let reader = SqliteStore::open_for_read(dir.path()).expect("not blocked by the writer");
    assert_eq!(find(&reader, "alpha"), ["a.md"]);
    assert!(
        started.elapsed() < Duration::from_secs(2),
        "the read path never waits for the writer lock"
    );
    blocker.execute_batch("ROLLBACK").unwrap();
}

#[test]
fn open_for_read_works_in_a_directory_that_cannot_be_written() {
    let dir = tempfile::tempdir().unwrap();
    {
        let store = open(&dir);
        let s = source_id("one");
        index(&store, "one", vec![file(&s, "a.md", "# One\nalpha", 1)]);
    }
    let before = directory_listing(&dir);
    let _unwritable = UnwritableDir::new(dir.path());

    let reader = SqliteStore::open_for_read(dir.path()).expect("a read-only directory is fine");
    assert_eq!(find(&reader, "alpha"), ["a.md"]);
    let source = Source::register(source_id("two"), "two", location("two"));
    assert!(matches!(reader.save(&source), Err(StorageError::Failed(_))));
    drop(reader);
    assert_eq!(directory_listing(&dir), before);
}

#[test]
fn a_read_only_store_refuses_every_write() {
    let dir = tempfile::tempdir().unwrap();
    let s = source_id("one");
    let indexed = {
        let store = open(&dir);
        index(&store, "one", vec![file(&s, "a.md", "# One\nalpha", 1)])
    };
    let reader = SqliteStore::open_for_read(dir.path()).unwrap();
    let next = ready(&indexed, 2);
    assert!(matches!(
        reader.commit_index(&next, Some(&commit_id(1)), &IndexChanges::default()),
        Err(StorageError::Failed(_))
    ));
    assert!(matches!(
        reader.remove_source(&s),
        Err(StorageError::Failed(_))
    ));
    assert!(matches!(
        reader.rewrite_index_fields(&s, &[], IndexVersions::current()),
        Err(StorageError::Failed(_))
    ));
    assert_eq!(find(&reader, "alpha"), ["a.md"], "reads still work");
}

/// A database whose `meta` says `schema_version` is `found` and which holds nothing
/// else: what a future (or past) build would have left.
fn database_with_schema_version(dir: &TempDir, found: &str) {
    raw(dir)
        .execute_batch(&format!(
            "CREATE TABLE meta (key TEXT PRIMARY KEY, value TEXT NOT NULL) WITHOUT ROWID; \
             INSERT INTO meta VALUES ('schema_version', '{found}'), ('fts_tokenizer', 'x');"
        ))
        .unwrap();
}

fn schema_objects(dir: &TempDir) -> Vec<String> {
    let connection = raw(dir);
    let mut statement = connection
        .prepare("SELECT name FROM sqlite_master ORDER BY name")
        .unwrap();
    let rows = statement.query_map([], |row| row.get(0)).unwrap();
    rows.map(|row| row.unwrap()).collect()
}

#[test]
fn another_schema_version_is_refused_before_any_ddl_runs() {
    let dir = tempfile::tempdir().unwrap();
    database_with_schema_version(&dir, "999");
    let before = schema_objects(&dir);

    let write = SqliteStore::open_for_write(dir.path());
    assert!(
        matches!(write, Err(StorageError::Failed(_))),
        "open_for_write: {write:?}"
    );
    let read = SqliteStore::open_for_read(dir.path());
    assert!(
        matches!(read, Err(StorageError::Failed(_))),
        "open_for_read: {read:?}"
    );
    assert_eq!(
        schema_objects(&dir),
        before,
        "no table was created in a database of another version"
    );
}

#[test]
fn another_fts_tokenizer_configuration_is_refused_on_open() {
    let dir = tempfile::tempdir().unwrap();
    drop(open(&dir));
    raw(&dir)
        .execute(
            "UPDATE meta SET value = 'porter' WHERE key = 'fts_tokenizer'",
            [],
        )
        .unwrap();
    assert!(matches!(
        SqliteStore::open_for_write(dir.path()),
        Err(StorageError::Failed(_))
    ));
    assert!(matches!(
        SqliteStore::open_for_read(dir.path()),
        Err(StorageError::Failed(_))
    ));
}

#[test]
fn a_bulk_commit_checkpoints_and_truncates_the_wal() {
    let dir = tempfile::tempdir().unwrap();
    let store = open(&dir);
    let s = source_id("one");
    let files: Vec<IndexedFile> = (0..300)
        .map(|i| {
            file(
                &s,
                &format!("bulk/f{i:03}.txt"),
                &format!(
                    "needle {}",
                    "filler words to make the pages busy ".repeat(40)
                ),
                (i % 200) as u8,
            )
        })
        .collect();
    index(&store, "one", files);

    let wal = dir.path().join(format!("{DATABASE_FILE}-wal"));
    let size = std::fs::metadata(&wal).map_or(0, |m| m.len());
    assert_eq!(size, 0, "the WAL was checkpointed and truncated");
    assert_eq!(find(&store, "needle").len(), 50, "the data is intact");
}

#[test]
fn a_checkpoint_never_waits_for_a_reader() {
    let dir = tempfile::tempdir().unwrap();
    let store =
        SqliteStore::open_for_write_with_busy_timeout(dir.path(), Duration::from_secs(30)).unwrap();
    let s = source_id("one");
    let base = index(&store, "one", vec![file(&s, "a.md", "# One\nalpha", 1)]);

    // A reader that keeps its snapshot open blocks a TRUNCATE checkpoint.
    let reader = raw(&dir);
    reader.execute_batch("BEGIN").unwrap();
    reader
        .query_row("SELECT COUNT(*) FROM chunks", [], |row| {
            row.get::<_, i64>(0)
        })
        .unwrap();

    let started = std::time::Instant::now();
    store
        .commit_index(
            &ready(&base, 2),
            Some(&commit_id(1)),
            &upserts(vec![file(&s, "b.md", "# Two\nbeta", 2)]),
        )
        .expect("the commit succeeds although the checkpoint cannot finish");
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "the writer did not wait for the reader ({:?})",
        started.elapsed()
    );
    reader.execute_batch("ROLLBACK").unwrap();
    assert_eq!(find(&store, "beta"), ["b.md"]);
}
