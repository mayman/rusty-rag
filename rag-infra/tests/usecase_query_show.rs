//! The show use case against the real SQLite store and the real system `git`: a
//! repository made with the tests' own helpers is cloned, indexed through
//! `commit_index`, and then shown by path and by chunk id.

use std::path::{Path, PathBuf};
use std::process::Command;

use rag_core::application::error::ErrorCode;
use rag_core::application::ports::{
    ChunkStore, GitClient, IndexChanges, IndexedFile, SourceRepository,
};
use rag_core::application::show_file::{ShowFile, ShowFileRequest};
use rag_core::domain::chunker::Chunker;
use rag_core::domain::git_location::GitLocation;
use rag_core::domain::git_ref::GitRef;
use rag_core::domain::index_policy::IndexPolicy;
use rag_core::domain::indexed_at::IndexedAt;
use rag_core::domain::oid::CommitId;
use rag_core::domain::source::Source;
use rag_core::domain::source_id::SourceId;
use rag_infra::config::DataLayout;
use rag_infra::git::SystemGitClient;
use rag_infra::sqlite::SqliteStore;
use tempfile::TempDir;

/// A git command with a fixed identity, no signing, no prompts, and no dependence
/// on the developer's configuration.
fn git(dir: &Path) -> Command {
    let mut command = Command::new("git");
    command
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
        ])
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", dir.join("no-global-gitconfig-here"));
    command
}

fn git_ok(dir: &Path, args: &[&str]) -> String {
    let output = git(dir).args(args).output().expect("git runs");
    assert!(
        output.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).trim().to_owned()
}

struct World {
    _upstream: TempDir,
    _data: TempDir,
    store: SqliteStore,
    git: SystemGitClient,
    commit: CommitId,
}

/// Commits `files` to a fresh repository, clones it, and indexes every file the
/// policy accepts under the source name `proj`.
fn world(files: &[(&str, &str)]) -> World {
    let upstream = tempfile::tempdir().unwrap();
    let repo: PathBuf = upstream.path().join("proj");
    std::fs::create_dir_all(&repo).unwrap();
    git_ok(&repo, &["init", "-q", "-b", "main"]);
    for (name, content) in files {
        let file = repo.join(name);
        std::fs::create_dir_all(file.parent().unwrap()).unwrap();
        std::fs::write(file, content).unwrap();
    }
    git_ok(&repo, &["add", "-A"]);
    git_ok(&repo, &["commit", "-q", "-m", "initial"]);

    let data = tempfile::tempdir().unwrap();
    let layout = DataLayout::new(data.path().join("data"));
    layout.ensure_dirs().unwrap();
    let client = SystemGitClient::new(layout.repos_dir(), layout.tmp_dir()).with_isolated_config();
    let store = SqliteStore::open_for_write(data.path()).unwrap();

    let location = GitLocation::parse(repo.to_str().unwrap()).unwrap();
    let id = SourceId::from_location(&location).unwrap();
    let commit = client
        .clone_shallow(&id, &location, &GitRef::parse("main").unwrap())
        .expect("clone");
    let mut source = Source::register(id.clone(), "proj", location);
    store.save(&source).unwrap();
    source.mark_ready(commit.clone(), IndexedAt::from_epoch_seconds(1_700_000_000));

    let entries = client.ls_tree(&id, &commit).unwrap().entries;
    let mut upserts = Vec::new();
    for entry in entries {
        if IndexPolicy::check_entry(&entry).is_err() {
            continue;
        }
        let bytes = client
            .read_blobs(&id, std::slice::from_ref(&entry.blob))
            .unwrap()[&entry.blob]
            .clone();
        if IndexPolicy::check_content(&entry.path, &bytes).is_err() {
            continue;
        }
        let text = String::from_utf8(bytes).unwrap();
        upserts.push(IndexedFile {
            chunks: Chunker::chunk(&id, &entry.path, &text),
            blob: entry.blob,
            size: entry.size,
            path: entry.path,
        });
    }
    store
        .commit_index(
            &source,
            None,
            &IndexChanges {
                upserts,
                removals: Vec::new(),
            },
        )
        .expect("commit the prepared index");
    World {
        _upstream: upstream,
        _data: data,
        store,
        git: client,
        commit,
    }
}

impl World {
    fn show(&self, path: &str) -> ShowFileRequest {
        ShowFileRequest {
            repo: "proj".to_owned(),
            path: path.to_owned(),
            lines: None,
            around: None,
        }
    }

    fn use_case(&self) -> ShowFile<'_> {
        ShowFile::new(&self.store, &self.store, &self.git)
    }
}

const LIB: &str = "pub fn alpha() {}\npub fn beta() {}\npub fn gamma() {}\n";

#[test]
fn a_real_file_is_shown_at_the_indexed_commit() {
    let world = world(&[("src/lib.rs", LIB)]);
    let shown = world.use_case().execute(&world.show("src/lib.rs")).unwrap();
    assert_eq!(shown.text, LIB.trim_end());
    assert_eq!(shown.commit, world.commit.short());
    assert_eq!(shown.lines, [1, 3]);
    assert_eq!(shown.total_lines, Some(3));
    assert_eq!(shown.language, "rust");
    assert!(!shown.truncated);
}

#[test]
fn a_line_range_is_cut_from_the_real_blob() {
    let world = world(&[("src/lib.rs", LIB)]);
    let mut request = world.show("src/lib.rs");
    request.lines = Some((2, 2));
    let shown = world.use_case().execute(&request).unwrap();
    assert_eq!(shown.text, "pub fn beta() {}");
    request.lines = None;
    request.around = Some(3);
    assert_eq!(world.use_case().execute(&request).unwrap().lines, [1, 3]);
}

#[test]
fn a_file_the_policy_skipped_is_not_shown_though_git_has_it() {
    let world = world(&[("src/lib.rs", LIB), ("Cargo.lock", "# lock\nversion = 3\n")]);
    let error = world
        .use_case()
        .execute(&world.show("Cargo.lock"))
        .unwrap_err();
    assert_eq!(error.code(), ErrorCode::NotFound);
    assert!(error.to_string().contains("not indexed"), "{error}");
}

#[test]
fn a_traversal_path_is_refused_before_touching_git() {
    let world = world(&[("src/lib.rs", LIB)]);
    for path in ["../secret", "/etc/passwd", "src/../../x"] {
        let error = world.use_case().execute(&world.show(path)).unwrap_err();
        assert_eq!(error.code(), ErrorCode::Usage, "{path}");
    }
}

#[test]
fn a_chunk_id_from_the_store_is_shown_with_its_lines() {
    let world = world(&[("docs/guide.md", "# Guide\n\nSome words here.\n")]);
    let source = world.store.list().unwrap().remove(0);
    let chunk = world.store.chunks(source.id()).unwrap().remove(0);
    let shown = world.use_case().execute_chunk(chunk.id.as_str()).unwrap();
    assert_eq!(shown.path, "docs/guide.md");
    assert_eq!(shown.text, "# Guide\n\nSome words here.");
    assert_eq!(shown.chunk.as_deref(), Some(chunk.id.as_str()));
    assert_eq!(shown.commit, world.commit.short());
}

#[test]
fn an_unknown_chunk_id_points_back_to_search() {
    let world = world(&[("src/lib.rs", LIB)]);
    let error = world
        .use_case()
        .execute_chunk(&"cd".repeat(32))
        .unwrap_err();
    assert_eq!(error.code(), ErrorCode::NotFound);
    assert!(error.to_string().contains("rag search"));
}

#[test]
fn a_removed_clone_makes_show_report_not_found_instead_of_failing_obscurely() {
    let world = world(&[("src/lib.rs", LIB)]);
    let source = world.store.list().unwrap().remove(0);
    world.git.remove_checkout(source.id()).unwrap();
    let error = world
        .use_case()
        .execute(&world.show("src/lib.rs"))
        .unwrap_err();
    assert_eq!(error.code(), ErrorCode::NotFound, "{error}");
}
