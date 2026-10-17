//! fetch_update: new commits, force-push, rename, delete, vanished refs.

use rag_core::application::error::GitError;
use rag_core::application::ports::GitClient;
use rag_core::domain::git_ref::GitRef;
use rag_core::domain::oid::CommitId;

use super::support::{Sandbox, Upstream};

fn main_ref() -> GitRef {
    GitRef::parse("main").unwrap()
}

fn paths(sandbox: &Sandbox, upstream: &Upstream, commit: &CommitId) -> Vec<String> {
    sandbox
        .client
        .ls_tree(&upstream.source_id(), commit)
        .unwrap()
        .entries
        .iter()
        .map(|entry| entry.path.as_str().to_owned())
        .collect()
}

fn cloned(files: &[(&str, &[u8])]) -> (Upstream, Sandbox, CommitId) {
    let upstream = Upstream::with_files(files);
    let sandbox = Sandbox::new();
    let commit = sandbox
        .client
        .clone_shallow(&upstream.source_id(), &upstream.location(), &main_ref())
        .unwrap();
    (upstream, sandbox, commit)
}

#[test]
fn fetch_after_a_new_commit_returns_it_and_keeps_the_old_one_readable() {
    let (upstream, sandbox, first) = cloned(&[("a.txt", b"1")]);
    upstream.write("b.txt", b"2");
    let newer = upstream.commit("two");

    let fetched = sandbox
        .client
        .fetch_update(&upstream.source_id(), &main_ref())
        .unwrap();

    assert_eq!(fetched.as_str(), newer);
    assert_eq!(paths(&sandbox, &upstream, &fetched), ["a.txt", "b.txt"]);
    assert_eq!(
        paths(&sandbox, &upstream, &first),
        ["a.txt"],
        "earlier commits stay readable until the next fetch"
    );
}

#[test]
fn fetching_without_news_returns_the_same_commit() {
    let (upstream, sandbox, first) = cloned(&[("a.txt", b"1")]);
    let fetched = sandbox
        .client
        .fetch_update(&upstream.source_id(), &main_ref())
        .unwrap();
    assert_eq!(fetched, first);
}

#[test]
fn a_force_push_is_followed() {
    let (upstream, sandbox, first) = cloned(&[("a.txt", b"1")]);
    upstream.write("a.txt", b"rewritten");
    upstream.git(&["commit", "-q", "-a", "--amend", "-m", "rewritten history"]);
    let rewritten = upstream.head();
    assert_ne!(rewritten, first.as_str());

    let fetched = sandbox
        .client
        .fetch_update(&upstream.source_id(), &main_ref())
        .unwrap();

    assert_eq!(fetched.as_str(), rewritten);
}

#[test]
fn fetching_twice_after_a_force_push_back_to_older_history_works() {
    let (upstream, sandbox, _) = cloned(&[("a.txt", b"1")]);
    let id = upstream.source_id();
    upstream.write("a.txt", b"2");
    upstream.commit("two");
    sandbox.client.fetch_update(&id, &main_ref()).unwrap();
    upstream.git(&["reset", "-q", "--hard", "HEAD~1"]);
    let back = upstream.head();

    let fetched = sandbox.client.fetch_update(&id, &main_ref()).unwrap();

    assert_eq!(fetched.as_str(), back);
}

#[test]
fn renames_and_deletes_show_up_in_the_tree() {
    let (upstream, sandbox, _) = cloned(&[
        ("old.txt", b"same content"),
        ("gone.txt", b"bye"),
        ("keep.txt", b"keep"),
    ]);
    upstream.git(&["mv", "old.txt", "new.txt"]);
    upstream.remove("gone.txt");
    let newer = upstream.commit("rename and delete");

    let fetched = sandbox
        .client
        .fetch_update(&upstream.source_id(), &main_ref())
        .unwrap();

    assert_eq!(fetched.as_str(), newer);
    assert_eq!(
        paths(&sandbox, &upstream, &fetched),
        ["keep.txt", "new.txt"]
    );
}

#[test]
fn fetching_a_source_that_was_never_cloned_is_not_found() {
    let upstream = Upstream::with_files(&[("a.txt", b"1")]);
    let sandbox = Sandbox::new();
    let error = sandbox
        .client
        .fetch_update(&upstream.source_id(), &main_ref())
        .unwrap_err();
    assert!(matches!(error, GitError::NotFound(_)), "{error:?}");
}

#[test]
fn a_ref_deleted_upstream_is_not_found_and_the_old_commit_stays_readable() {
    let (upstream, sandbox, first) = cloned(&[("a.txt", b"1")]);
    upstream.git(&["checkout", "-q", "-b", "other"]);
    upstream.git(&["branch", "-q", "-D", "main"]);

    let error = sandbox
        .client
        .fetch_update(&upstream.source_id(), &main_ref())
        .unwrap_err();

    assert!(matches!(error, GitError::NotFound(_)), "{error:?}");
    assert_eq!(paths(&sandbox, &upstream, &first), ["a.txt"]);
}

#[test]
fn an_unreachable_origin_is_a_network_error_not_a_panic() {
    let (upstream, sandbox, _) = cloned(&[("a.txt", b"1")]);
    let moved = upstream.path.with_extension("moved");
    std::fs::rename(&upstream.path, &moved).unwrap();

    let error = sandbox
        .client
        .fetch_update(&upstream.source_id(), &main_ref())
        .unwrap_err();

    assert!(matches!(error, GitError::NotFound(_)), "{error:?}");
    std::fs::rename(&moved, &upstream.path).unwrap();
}
