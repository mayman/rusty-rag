//! default_branch and clone_shallow.

use std::time::Duration;

use rag_core::application::error::GitError;
use rag_core::application::ports::GitClient;
use rag_core::domain::git_location::GitLocation;
use rag_core::domain::git_ref::GitRef;
use rag_core::domain::source_id::SourceId;
use rag_infra::git::SystemGitClient;

use super::support::{git_ok, Sandbox, Upstream};

fn main_ref() -> GitRef {
    GitRef::parse("main").unwrap()
}

#[test]
fn default_branch_is_the_branch_head_points_to() {
    let upstream = Upstream::on_branch("trunk");
    upstream.write("a.txt", b"a");
    upstream.commit("one");
    let sandbox = Sandbox::new();
    let branch = sandbox.client.default_branch(&upstream.location()).unwrap();
    assert_eq!(branch.as_str(), "trunk");
}

#[test]
fn default_branch_of_a_missing_repository_is_not_found() {
    let sandbox = Sandbox::new();
    let missing =
        GitLocation::parse(sandbox.layout.root().join("nowhere").to_str().unwrap()).unwrap();
    let error = sandbox.client.default_branch(&missing).unwrap_err();
    assert!(matches!(error, GitError::NotFound(_)), "{error:?}");
}

#[test]
fn clone_returns_the_commit_and_stores_only_objects_without_a_working_tree() {
    let upstream =
        Upstream::with_files(&[("src/lib.rs", b"fn main() {}\n"), ("README.md", b"# hi\n")]);
    let sandbox = Sandbox::new();
    let id = upstream.source_id();

    let commit = sandbox
        .client
        .clone_shallow(&id, &upstream.location(), &main_ref())
        .unwrap();

    assert_eq!(commit.as_str(), upstream.head());
    let dir = sandbox.checkout_dir(&id);
    assert!(dir.join("objects").is_dir());
    assert!(!dir.join("src").exists(), "no working tree");
    assert!(!dir.join("README.md").exists(), "no working tree");
    assert_eq!(git_ok(&dir, &["rev-parse", "--is-bare-repository"]), "true");
}

#[test]
fn clone_is_shallow_and_keeps_one_commit() {
    let upstream = Upstream::with_files(&[("a.txt", b"1")]);
    upstream.write("a.txt", b"2");
    upstream.commit("two");
    upstream.write("a.txt", b"3");
    upstream.commit("three");
    let sandbox = Sandbox::new();
    let id = upstream.source_id();

    sandbox
        .client
        .clone_shallow(&id, &upstream.location(), &main_ref())
        .unwrap();

    let dir = sandbox.checkout_dir(&id);
    assert_eq!(git_ok(&dir, &["rev-list", "--count", "--all"]), "1");
    assert!(dir.join("shallow").is_file());
}

#[test]
fn clone_works_over_a_file_url() {
    let upstream = Upstream::with_files(&[("a.txt", b"1")]);
    let sandbox = Sandbox::new();
    let location = upstream.file_url();
    let id = SourceId::from_location(&location).unwrap();
    let commit = sandbox
        .client
        .clone_shallow(&id, &location, &main_ref())
        .unwrap();
    assert_eq!(commit.as_str(), upstream.head());
}

#[test]
fn cloning_twice_is_idempotent_and_picks_up_new_upstream_commits() {
    let upstream = Upstream::with_files(&[("a.txt", b"1")]);
    let sandbox = Sandbox::new();
    let id = upstream.source_id();
    let first = sandbox
        .client
        .clone_shallow(&id, &upstream.location(), &main_ref())
        .unwrap();
    let again = sandbox
        .client
        .clone_shallow(&id, &upstream.location(), &main_ref())
        .unwrap();
    assert_eq!(first, again);

    upstream.write("b.txt", b"2");
    let newer = upstream.commit("two");
    let third = sandbox
        .client
        .clone_shallow(&id, &upstream.location(), &main_ref())
        .unwrap();
    assert_eq!(third.as_str(), newer);
}

#[test]
fn a_half_cloned_directory_is_replaced() {
    let upstream = Upstream::with_files(&[("a.txt", b"1")]);
    let sandbox = Sandbox::new();
    let id = upstream.source_id();
    let dir = sandbox.checkout_dir(&id);
    std::fs::create_dir_all(dir.join("objects")).unwrap();
    std::fs::write(dir.join("HEAD"), "garbage").unwrap();

    let commit = sandbox
        .client
        .clone_shallow(&id, &upstream.location(), &main_ref())
        .unwrap();

    assert_eq!(commit.as_str(), upstream.head());
    assert_eq!(git_ok(&dir, &["rev-parse", "--is-bare-repository"]), "true");
}

#[test]
fn a_leftover_staging_directory_from_a_crash_does_not_block_a_retry() {
    let upstream = Upstream::with_files(&[("a.txt", b"1")]);
    let sandbox = Sandbox::new();
    let id = upstream.source_id();
    let stale = sandbox
        .layout
        .tmp_dir()
        .join(format!("clone-{}", id.checkout_dir_name()));
    std::fs::create_dir_all(stale.join("objects")).unwrap();
    std::fs::write(stale.join("junk"), "x").unwrap();

    sandbox
        .client
        .clone_shallow(&id, &upstream.location(), &main_ref())
        .unwrap();

    assert!(!stale.exists(), "staging directory is consumed or cleared");
}

#[test]
fn an_existing_clone_of_another_origin_is_not_reused() {
    let first = Upstream::with_files(&[("a.txt", b"1")]);
    let second = Upstream::with_files(&[("b.txt", b"2")]);
    let sandbox = Sandbox::new();
    // Force both onto one checkout directory by cloning `first` under `second`'s id.
    let id = second.source_id();
    sandbox
        .client
        .clone_shallow(&id, &first.location(), &main_ref())
        .unwrap();

    let commit = sandbox
        .client
        .clone_shallow(&id, &second.location(), &main_ref())
        .unwrap();

    assert_eq!(
        commit.as_str(),
        second.head(),
        "the origin is verified on reuse"
    );
    let origin = git_ok(&sandbox.checkout_dir(&id), &["config", "remote.origin.url"]);
    assert_eq!(origin, second.location().as_str());
    assert!(
        sandbox.scratch_leftovers().is_empty(),
        "the replaced copy is gone, not left as trash"
    );
}

#[test]
fn a_probe_that_fails_never_replaces_the_existing_clone() {
    let upstream = Upstream::with_files(&[("a.txt", b"1")]);
    let sandbox = Sandbox::new();
    let id = upstream.source_id();
    let commit = sandbox
        .client
        .clone_shallow(&id, &upstream.location(), &main_ref())
        .unwrap();

    // Every local git call times out, including the one that inspects the checkout.
    let impatient = sandbox
        .another_client()
        .with_timeouts(Duration::from_secs(300), Duration::from_millis(1));
    let error = impatient
        .clone_shallow(&id, &upstream.location(), &main_ref())
        .unwrap_err();

    assert_eq!(error, GitError::Timeout, "the probe error propagates");
    assert!(sandbox.checkout_dir(&id).join("objects").is_dir());
    let listing = sandbox.client.ls_tree(&id, &commit).unwrap();
    assert_eq!(listing.entries.len(), 1, "the old checkout is intact");
    assert!(sandbox.scratch_leftovers().is_empty());
}

#[test]
fn a_failed_replacement_keeps_the_old_clone() {
    let first = Upstream::with_files(&[("a.txt", b"1")]);
    let second = Upstream::with_files(&[("b.txt", b"2")]);
    let sandbox = Sandbox::new();
    let id = second.source_id();
    let old_commit = sandbox
        .client
        .clone_shallow(&id, &first.location(), &main_ref())
        .unwrap();

    // The origin differs, so the old copy must be replaced; the replacement fails.
    let error = sandbox
        .client
        .clone_shallow(
            &id,
            &second.location(),
            &GitRef::parse("no-such-branch").unwrap(),
        )
        .unwrap_err();

    assert!(matches!(error, GitError::NotFound(_)), "{error:?}");
    let dir = sandbox.checkout_dir(&id);
    assert_eq!(
        git_ok(&dir, &["config", "remote.origin.url"]),
        first.location().as_str(),
        "the old checkout is still the first origin"
    );
    assert_eq!(
        sandbox
            .client
            .ls_tree(&id, &old_commit)
            .unwrap()
            .entries
            .len(),
        1
    );
    assert!(sandbox.scratch_leftovers().is_empty(), "no staging left");
}

#[test]
fn a_new_clone_has_no_template_files_and_repository_hooks_never_run() {
    let upstream = Upstream::with_files(&[("a.txt", b"1")]);
    let sandbox = Sandbox::new();
    let id = upstream.source_id();
    sandbox
        .client
        .clone_shallow(&id, &upstream.location(), &main_ref())
        .unwrap();
    let dir = sandbox.checkout_dir(&id);
    let templated: Vec<_> = std::fs::read_dir(dir.join("hooks"))
        .map(|entries| entries.map(|e| e.unwrap().file_name()).collect())
        .unwrap_or_default();
    assert!(templated.is_empty(), "no template hooks: {templated:?}");
    assert!(!dir.join("info").join("exclude").exists());

    // A hook planted in the checkout is never run, whatever git operation fires it.
    let marker = sandbox.layout.root().join("hook-ran");
    let hook = dir.join("hooks").join("reference-transaction");
    std::fs::create_dir_all(hook.parent().unwrap()).unwrap();
    std::fs::write(
        &hook,
        format!(
            "#!/bin/sh\necho ran >> \"{}\"\n",
            marker.to_string_lossy().replace('\\', "/")
        ),
    )
    .unwrap();
    upstream.write("b.txt", b"2");
    let newer = upstream.commit("two");
    let fetched = sandbox.client.fetch_update(&id, &main_ref()).unwrap();
    assert_eq!(fetched.as_str(), newer);
    assert!(!marker.exists(), "the planted hook must not run");
}

#[test]
fn a_failed_clone_leaves_no_checkout_and_no_staging_directory() {
    let upstream = Upstream::with_files(&[("a.txt", b"1")]);
    let sandbox = Sandbox::new();
    let id = upstream.source_id();

    let error = sandbox
        .client
        .clone_shallow(
            &id,
            &upstream.location(),
            &GitRef::parse("no-such-branch").unwrap(),
        )
        .unwrap_err();

    assert!(matches!(error, GitError::NotFound(_)), "{error:?}");
    assert!(!sandbox.checkout_dir(&id).exists());
    assert!(sandbox.scratch_leftovers().is_empty());
}

#[test]
fn cloning_a_missing_repository_is_not_found() {
    let sandbox = Sandbox::new();
    let missing =
        GitLocation::parse(sandbox.layout.root().join("nowhere").to_str().unwrap()).unwrap();
    let id = SourceId::from_location(&missing).unwrap();
    let error = sandbox
        .client
        .clone_shallow(&id, &missing, &main_ref())
        .unwrap_err();
    assert!(matches!(error, GitError::NotFound(_)), "{error:?}");
}

#[test]
fn an_empty_upstream_has_no_ref_to_clone() {
    let upstream = Upstream::new();
    let sandbox = Sandbox::new();
    let error = sandbox
        .client
        .clone_shallow(&upstream.source_id(), &upstream.location(), &main_ref())
        .unwrap_err();
    assert!(matches!(error, GitError::NotFound(_)), "{error:?}");
}

#[test]
fn an_annotated_tag_resolves_to_the_commit_it_points_at() {
    let upstream = Upstream::with_files(&[("a.txt", b"1")]);
    let commit = upstream.head();
    upstream.git(&["tag", "-a", "v1.0", "-m", "release"]);
    let tag_object = upstream.git(&["rev-parse", "v1.0"]);
    assert_ne!(tag_object, commit, "the tag is a separate object");
    upstream.write("a.txt", b"later");
    upstream.commit("after the tag");
    let sandbox = Sandbox::new();
    let id = upstream.source_id();

    let cloned = sandbox
        .client
        .clone_shallow(&id, &upstream.location(), &GitRef::parse("v1.0").unwrap())
        .unwrap();

    assert_eq!(cloned.as_str(), commit);
    let listing = sandbox.client.ls_tree(&id, &cloned).unwrap();
    assert_eq!(listing.entries.len(), 1);
}

#[test]
fn a_sha256_repository_is_rejected_with_a_clear_message() {
    let upstream = Upstream::sha256();
    upstream.write("a.txt", b"1");
    upstream.commit("sha256 commit");
    let sandbox = Sandbox::new();
    let id = upstream.source_id();

    let error = sandbox
        .client
        .clone_shallow(&id, &upstream.location(), &main_ref())
        .unwrap_err();

    let GitError::Failed(message) = &error else {
        panic!("expected Failed, got {error:?}");
    };
    assert!(message.as_str().contains("SHA-256"), "{message}");
    assert!(!sandbox.checkout_dir(&id).exists());
}

#[test]
fn a_transport_outside_the_allow_list_never_runs_a_command() {
    // `restore` is lenient on purpose; the adapter must still refuse `ext::`.
    let marker_dir = tempfile::tempdir().unwrap();
    let marker = marker_dir.path().join("pwned");
    let hostile = GitLocation::restore(&format!(
        "ext::git -c core.hooksPath=/dev/null init {}",
        marker.display()
    ))
    .unwrap();
    let sandbox = Sandbox::new();
    let id = SourceId::from_location(&GitLocation::parse("https://example.com/o/hostile").unwrap())
        .unwrap();

    let error = sandbox
        .client
        .clone_shallow(&id, &hostile, &main_ref())
        .unwrap_err();

    assert!(!matches!(error, GitError::Timeout), "{error:?}");
    assert!(!marker.exists(), "the ext:: helper must not run");
    assert!(!sandbox.checkout_dir(&id).exists());
}

#[test]
fn network_errors_never_echo_credentials() {
    let hostile = GitLocation::restore("https://user:s3cr3t@127.0.0.1:1/o/r?token=abc123").unwrap();
    let sandbox = Sandbox::new();
    let id =
        SourceId::from_location(&GitLocation::parse("https://example.com/o/r").unwrap()).unwrap();

    let error = sandbox
        .client
        .clone_shallow(&id, &hostile, &main_ref())
        .unwrap_err();

    let shown = format!("{error} / {error:?}");
    for secret in ["s3cr3t", "abc123"] {
        assert!(!shown.contains(secret), "{secret} leaked: {shown}");
    }
    assert!(matches!(error, GitError::NetworkOrAuth(_)), "{error:?}");
}

#[test]
fn credentials_are_not_stored_in_the_clone_configuration() {
    let upstream = Upstream::with_files(&[("a.txt", b"1")]);
    let sandbox = Sandbox::new();
    let id = upstream.source_id();
    sandbox
        .client
        .clone_shallow(&id, &upstream.location(), &main_ref())
        .unwrap();
    let config = std::fs::read_to_string(sandbox.checkout_dir(&id).join("config")).unwrap();
    assert!(!config.contains('@'), "{config}");
}

#[test]
fn a_clone_keeps_fetched_objects_packed() {
    let upstream = Upstream::with_files(&[("a.txt", b"1")]);
    let sandbox = Sandbox::new();
    let id = upstream.source_id();
    sandbox
        .client
        .clone_shallow(&id, &upstream.location(), &main_ref())
        .unwrap();
    let config = std::fs::read_to_string(sandbox.checkout_dir(&id).join("config"))
        .unwrap()
        .to_ascii_lowercase();
    assert!(config.contains("unpacklimit = 1"), "{config}");
    assert_eq!(config.matches("unpacklimit = 1").count(), 2, "{config}");
}

#[test]
fn a_slow_git_operation_times_out() {
    let upstream = Upstream::with_files(&[("a.txt", b"1")]);
    let sandbox = Sandbox::new();
    let client = SystemGitClient::new(sandbox.layout.repos_dir(), sandbox.layout.tmp_dir())
        .with_timeouts(Duration::from_millis(1), Duration::from_millis(1));
    let id = upstream.source_id();

    let error = client
        .clone_shallow(&id, &upstream.location(), &main_ref())
        .unwrap_err();

    assert_eq!(error, GitError::Timeout);
    assert!(!sandbox.checkout_dir(&id).exists());
}
