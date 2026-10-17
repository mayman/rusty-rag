//! The indexing use cases (add, update, remove, reindex) wired to the real adapters:
//! `SqliteStore`, `SystemGitClient`, `FileOperationLock`, and `SystemClock`, over
//! local git repositories created in temp directories. Every test has its own data
//! directory laid out like `RUSTY_RAG_HOME` (`DataLayout`), so nothing touches the
//! user's real data.

use std::path::{Path, PathBuf};
use std::process::Command;

use rag_core::application::add_source::{AddRequest, AddResponse, AddSource};
use rag_core::application::error::{AppError, ErrorCode};
use rag_core::application::ports::{ChunkSearcher, ChunkStore, OperationLock, SourceRepository};
use rag_core::application::reindex::{Reindex, ReindexRequest, ReindexResponse};
use rag_core::application::remove_source::{RemoveRequest, RemoveResponse, RemoveSource};
use rag_core::application::update_source::{UpdateRequest, UpdateResponse, UpdateSource};
use rag_core::domain::git_location::GitLocation;
use rag_core::domain::query::{Query, QueryMode};
use rag_core::domain::search_filters::SearchFilters;
use rag_core::domain::source::SourceStatus;
use rag_core::domain::source_id::SourceId;
use rag_infra::clock::SystemClock;
use rag_infra::config::DataLayout;
use rag_infra::git::SystemGitClient;
use rag_infra::lock::FileOperationLock;
use rag_infra::sqlite::SqliteStore;
use tempfile::TempDir;

// --- a throwaway upstream repository -----------------------------------------

fn git_in(dir: &Path, args: &[&str]) -> String {
    let output = Command::new("git")
        .current_dir(dir)
        .args([
            "-c",
            "user.name=Test",
            "-c",
            "user.email=test@example.com",
            "-c",
            "commit.gpgsign=false",
            "-c",
            "core.autocrlf=false",
            "-c",
            "protocol.file.allow=always",
        ])
        .args(args)
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", dir.join("no-global-gitconfig-here"))
        .output()
        .expect("git runs");
    assert!(
        output.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).trim().to_owned()
}

struct Upstream {
    _temp: TempDir,
    path: PathBuf,
}

impl Upstream {
    fn new() -> Self {
        let temp = tempfile::tempdir().expect("temp dir");
        let path = temp.path().join("upstream");
        std::fs::create_dir_all(&path).unwrap();
        git_in(&path, &["init", "-q", "-b", "main"]);
        Self { _temp: temp, path }
    }

    fn write(&self, name: &str, content: &str) {
        let file = self.path.join(name);
        std::fs::create_dir_all(file.parent().unwrap()).unwrap();
        std::fs::write(file, content).unwrap();
    }

    fn remove(&self, name: &str) {
        std::fs::remove_file(self.path.join(name)).unwrap();
    }

    fn rename(&self, from: &str, to: &str) {
        let target = self.path.join(to);
        std::fs::create_dir_all(target.parent().unwrap()).unwrap();
        std::fs::rename(self.path.join(from), target).unwrap();
    }

    fn commit(&self, message: &str) -> String {
        git_in(&self.path, &["add", "-A"]);
        git_in(
            &self.path,
            &["commit", "-q", "--allow-empty", "-m", message],
        );
        self.head()
    }

    /// Rewrites the last commit, as a force-push of a rebased branch would.
    fn amend(&self) -> String {
        git_in(&self.path, &["add", "-A"]);
        git_in(&self.path, &["commit", "-q", "--amend", "-m", "rewritten"]);
        self.head()
    }

    fn head(&self) -> String {
        git_in(&self.path, &["rev-parse", "HEAD"])
    }

    fn location(&self) -> String {
        self.path.to_str().unwrap().to_owned()
    }

    fn id(&self) -> SourceId {
        SourceId::from_location(&GitLocation::parse(&self.location()).unwrap()).unwrap()
    }
}

// --- the real adapters over one data directory ------------------------------

struct Env {
    _temp: TempDir,
    layout: DataLayout,
    store: SqliteStore,
    git: SystemGitClient,
    locks: FileOperationLock,
    clock: SystemClock,
}

impl Env {
    fn new() -> Self {
        let temp = tempfile::tempdir().expect("temp dir");
        let layout = DataLayout::new(temp.path().join("data"));
        layout.ensure_dirs().unwrap();
        let store = SqliteStore::open_for_write(layout.root()).unwrap();
        let git = SystemGitClient::new(layout.repos_dir(), layout.tmp_dir()).with_isolated_config();
        let locks = FileOperationLock::new(layout.locks_dir());
        Self {
            _temp: temp,
            layout,
            store,
            git,
            locks,
            clock: SystemClock,
        }
    }

    fn add(&self, upstream: &Upstream) -> Result<AddResponse, AppError> {
        AddSource::new(
            &self.store,
            &self.store,
            &self.git,
            &self.locks,
            &self.clock,
        )
        .execute(&AddRequest::new(&upstream.location()))
    }

    fn update(&self, name: Option<&str>) -> Result<UpdateResponse, AppError> {
        let request = name.map_or_else(UpdateRequest::all, UpdateRequest::named);
        UpdateSource::new(
            &self.store,
            &self.store,
            &self.git,
            &self.locks,
            &self.clock,
        )
        .execute(&request)
    }

    fn remove(&self, name: &str) -> Result<RemoveResponse, AppError> {
        RemoveSource::new(&self.store, &self.store, &self.git, &self.locks)
            .execute(&RemoveRequest::new(name))
    }

    fn reindex(&self, name: Option<&str>) -> Result<ReindexResponse, AppError> {
        let request = name.map_or_else(ReindexRequest::all, ReindexRequest::named);
        Reindex::new(
            &self.store,
            &self.store,
            &self.git,
            &self.locks,
            &self.clock,
        )
        .execute(&request)
    }

    fn manifest_paths(&self, id: &SourceId) -> Vec<String> {
        self.store
            .manifest(id)
            .unwrap()
            .iter()
            .map(|(path, _)| path.as_str().to_owned())
            .collect()
    }

    fn chunk_texts(&self, id: &SourceId) -> String {
        self.store
            .chunks(id)
            .unwrap()
            .iter()
            .map(|chunk| chunk.text.clone())
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn search_paths(&self, text: &str) -> Vec<String> {
        let query = Query::parse(text).unwrap();
        let outcome = self
            .store
            .search(&query, QueryMode::And, &SearchFilters::default(), 20)
            .unwrap();
        outcome
            .hits
            .iter()
            .map(|hit| hit.chunk.path.as_str().to_owned())
            .collect()
    }

    fn checkout_dir(&self, id: &SourceId) -> PathBuf {
        self.layout.repos_dir().join(id.checkout_dir_name())
    }
}

fn base_repo() -> Upstream {
    let upstream = Upstream::new();
    upstream.write("src/lib.rs", "pub fn parse_config() {}\n");
    upstream.write("README.md", "# Title\n\nSome docs about widgets.\n");
    upstream.write("notes.txt", "keep me\n");
    upstream.commit("initial");
    upstream
}

// --- tests -------------------------------------------------------------------

#[test]
fn add_indexes_the_committed_head_and_skips_what_the_policy_rejects() {
    let env = Env::new();
    let upstream = base_repo();
    upstream.write(".env", "TOKEN=hunter2\n");
    upstream.write("logo.png", "not really a png\n");
    upstream.write("package-lock.json", "{}\n");
    upstream.commit("noise");
    upstream.write("uncommitted.rs", "fn never_committed() {}\n");

    let response = env.add(&upstream).unwrap();

    assert_eq!(response.status, "ready");
    assert_eq!(response.commit, upstream.head());
    assert_eq!(
        env.manifest_paths(&upstream.id()),
        ["README.md", "notes.txt", "src/lib.rs"],
        "committed HEAD only, nothing skipped by the policy"
    );
    let skipped = &response.index.skipped;
    assert_eq!(skipped.get("secret_name"), Some(&1), "{skipped:?}");
    assert_eq!(skipped.get("binary"), Some(&1), "{skipped:?}");
    assert_eq!(skipped.get("lockfile"), Some(&1), "{skipped:?}");
    assert!(!env.chunk_texts(&upstream.id()).contains("hunter2"));
    assert_eq!(env.search_paths("parse_config"), ["src/lib.rs"]);
    let source = env.store.get(&upstream.id()).unwrap().unwrap();
    assert_eq!(source.status(), SourceStatus::Ready);
}

#[test]
fn update_after_a_new_commit_indexes_only_the_changes() {
    let env = Env::new();
    let upstream = base_repo();
    env.add(&upstream).unwrap();
    upstream.write("src/lib.rs", "pub fn parse_config_v2() {}\n");
    upstream.write("src/new.rs", "pub fn brand_new() {}\n");
    let second = upstream.commit("change");

    let response = env.update(Some(upstream.id().slug())).unwrap();

    let entry = &response.sources[0];
    assert_eq!(entry.result, "updated");
    let report = entry.index.as_ref().unwrap();
    assert_eq!(report.commit, second);
    assert_eq!(
        (
            report.files_added,
            report.files_changed,
            report.files_removed,
            report.files_unchanged
        ),
        (1, 1, 0, 2)
    );
    assert_eq!(env.search_paths("parse_config_v2"), ["src/lib.rs"]);
    assert_eq!(env.search_paths("brand_new"), ["src/new.rs"]);
    let source = env.store.get(&upstream.id()).unwrap().unwrap();
    assert_eq!(source.indexed().unwrap().commit.as_str(), second);
}

#[test]
fn update_follows_renames_and_deletions() {
    let env = Env::new();
    let upstream = base_repo();
    env.add(&upstream).unwrap();
    upstream.rename("notes.txt", "docs/notes.txt");
    upstream.remove("README.md");
    upstream.commit("rename and delete");

    let response = env.update(None).unwrap();

    let report = response.sources[0].index.as_ref().unwrap();
    assert_eq!((report.files_added, report.files_removed), (1, 2));
    assert_eq!(
        env.manifest_paths(&upstream.id()),
        ["docs/notes.txt", "src/lib.rs"]
    );
    assert!(
        env.search_paths("widgets").is_empty(),
        "the deleted file left the index"
    );
}

#[test]
fn update_follows_a_force_push() {
    let env = Env::new();
    let upstream = base_repo();
    let first = env.add(&upstream).unwrap().commit;
    upstream.write("src/lib.rs", "pub fn rebased_version() {}\n");
    let rewritten = upstream.amend();
    assert_ne!(first, rewritten, "the history was rewritten");

    let response = env.update(None).unwrap();

    assert_eq!(response.sources[0].result, "updated");
    assert_eq!(env.search_paths("rebased_version"), ["src/lib.rs"]);
    assert!(env.search_paths("parse_config").is_empty());
    let source = env.store.get(&upstream.id()).unwrap().unwrap();
    assert_eq!(source.indexed().unwrap().commit.as_str(), rewritten);
    assert_eq!(source.status(), SourceStatus::Ready);
}

#[test]
fn updating_or_adding_again_without_new_commits_changes_nothing() {
    let env = Env::new();
    let upstream = base_repo();
    let added = env.add(&upstream).unwrap();
    let before = env.store.get(&upstream.id()).unwrap().unwrap();

    let update = env.update(None).unwrap();
    let again = env.add(&upstream).unwrap();

    assert_eq!(update.sources[0].result, "unchanged");
    assert!(again.already_added);
    assert_eq!(again.commit, added.commit);
    assert_eq!(
        env.store.get(&upstream.id()).unwrap().unwrap().indexed(),
        before.indexed()
    );
    assert_eq!(env.store.list().unwrap().len(), 1);
}

#[test]
fn remove_deletes_the_index_the_catalog_row_and_the_clone() {
    let env = Env::new();
    let upstream = base_repo();
    env.add(&upstream).unwrap();
    assert!(env.checkout_dir(&upstream.id()).exists());

    let response = env.remove(upstream.id().slug()).unwrap();

    assert!(response.checkout_removed);
    assert!(env.store.get(&upstream.id()).unwrap().is_none());
    assert!(env.store.manifest(&upstream.id()).unwrap().is_empty());
    assert!(env.search_paths("parse_config").is_empty());
    assert!(!env.checkout_dir(&upstream.id()).exists());
    let error = env.remove(upstream.id().slug()).unwrap_err();
    assert_eq!(error.code(), ErrorCode::NotFound);
    env.add(&upstream)
        .expect("a removed source can be added again");
}

#[test]
fn a_held_operation_lock_makes_the_use_cases_fail_with_exit_six() {
    let env = Env::new();
    let upstream = base_repo();
    env.add(&upstream).unwrap();
    let held = env.locks.acquire(&upstream.id()).unwrap();

    for error in [
        env.add(&upstream).unwrap_err(),
        env.update(Some(upstream.id().slug())).unwrap_err(),
        env.remove(upstream.id().slug()).unwrap_err(),
        env.reindex(Some(upstream.id().slug())).unwrap_err(),
    ] {
        assert_eq!(error.code().exit_code(), 6, "{error}");
    }
    drop(held);
    assert!(env.update(None).is_ok());
}

#[test]
fn adding_a_location_that_cannot_be_cloned_fails_and_a_retry_works_once_it_exists() {
    let env = Env::new();
    let upstream = Upstream::new();
    // No commits yet: nothing to clone.
    assert!(env.add(&upstream).is_err());
    let source = env.store.get(&upstream.id()).unwrap().unwrap();
    assert_eq!(source.status(), SourceStatus::Failed);
    assert!(!source.is_searchable());

    upstream.write("a.rs", "pub fn after_the_fix() {}\n");
    upstream.commit("first");
    let response = env.add(&upstream).unwrap();

    assert!(!response.already_added);
    assert_eq!(env.search_paths("after_the_fix"), ["a.rs"]);
}

#[test]
fn reindex_rebuilds_an_index_made_by_an_older_chunker_from_git() {
    let env = Env::new();
    let upstream = base_repo();
    env.add(&upstream).unwrap();
    let before = env.store.chunks(&upstream.id()).unwrap();
    // Age the index the way an older build would have left it.
    let database = rusqlite::Connection::open(env.layout.index_db()).unwrap();
    let changed = database
        .execute(
            "UPDATE sources SET chunker_version = 0 WHERE source_id = ?1",
            [upstream.id().as_str()],
        )
        .unwrap();
    assert_eq!(changed, 1);
    drop(database);
    let query = Query::parse("parse_config").unwrap();
    assert!(
        env.store
            .search(&query, QueryMode::And, &SearchFilters::default(), 5)
            .is_err(),
        "a stale index is not searched"
    );

    let response = env.reindex(None).unwrap();

    assert_eq!(response.sources[0].result, "full");
    assert_eq!(env.store.chunks(&upstream.id()).unwrap(), before);
    assert_eq!(env.search_paths("parse_config"), ["src/lib.rs"]);
}
