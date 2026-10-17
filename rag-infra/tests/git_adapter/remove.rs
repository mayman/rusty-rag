//! remove_checkout and garbage collection of leftovers.

use rag_core::application::ports::{GitClient, OperationLock};
use rag_core::domain::git_ref::GitRef;
use rag_infra::lock::FileOperationLock;

use super::support::{Sandbox, Upstream};

fn main_ref() -> GitRef {
    GitRef::parse("main").unwrap()
}

#[test]
fn removing_deletes_the_checkout_including_read_only_files() {
    let upstream = Upstream::with_files(&[("a.txt", b"1")]);
    let sandbox = Sandbox::new();
    let id = upstream.source_id();
    sandbox
        .client
        .clone_shallow(&id, &upstream.location(), &main_ref())
        .unwrap();
    let dir = sandbox.checkout_dir(&id);
    // Git marks object files read-only on Windows; make sure at least one is.
    let readonly = dir.join("readonly.marker");
    std::fs::write(&readonly, "x").unwrap();
    let mut permissions = std::fs::metadata(&readonly).unwrap().permissions();
    permissions.set_readonly(true);
    std::fs::set_permissions(&readonly, permissions).unwrap();

    sandbox.client.remove_checkout(&id).unwrap();

    assert!(!dir.exists());
}

#[test]
fn removing_an_absent_checkout_is_a_no_op() {
    let upstream = Upstream::new();
    let sandbox = Sandbox::new();
    sandbox
        .client
        .remove_checkout(&upstream.source_id())
        .unwrap();
}

#[test]
fn a_source_can_be_added_again_after_removal() {
    let upstream = Upstream::with_files(&[("a.txt", b"1")]);
    let sandbox = Sandbox::new();
    let id = upstream.source_id();
    sandbox
        .client
        .clone_shallow(&id, &upstream.location(), &main_ref())
        .unwrap();
    sandbox.client.remove_checkout(&id).unwrap();

    let commit = sandbox
        .client
        .clone_shallow(&id, &upstream.location(), &main_ref())
        .unwrap();

    assert_eq!(commit.as_str(), upstream.head());
}

#[test]
fn removal_also_clears_a_staging_directory_left_by_a_crashed_clone() {
    let upstream = Upstream::new();
    let sandbox = Sandbox::new();
    let id = upstream.source_id();
    let stale = sandbox
        .layout
        .tmp_dir()
        .join(format!("clone-{}", id.checkout_dir_name()));
    std::fs::create_dir_all(&stale).unwrap();
    std::fs::write(stale.join("junk"), "x").unwrap();

    sandbox.client.remove_checkout(&id).unwrap();

    assert!(!stale.exists());
}

fn lock_for(sandbox: &Sandbox) -> FileOperationLock {
    FileOperationLock::new(sandbox.layout.locks_dir())
}

#[test]
fn collect_garbage_empties_the_scratch_directory() {
    let sandbox = Sandbox::new();
    let leftover = sandbox.layout.tmp_dir().join("trash-1");
    std::fs::create_dir_all(leftover.join("objects")).unwrap();
    std::fs::write(sandbox.layout.tmp_dir().join("stray-file"), "x").unwrap();
    let staging = sandbox.layout.tmp_dir().join("clone-0123456789ab");
    std::fs::create_dir_all(staging.join("objects")).unwrap();

    sandbox.client.collect_garbage(&lock_for(&sandbox));

    assert!(
        sandbox.scratch_leftovers().is_empty(),
        "{:?}",
        sandbox.scratch_leftovers()
    );
}

#[test]
fn collect_garbage_leaves_the_staging_directory_of_a_source_that_is_locked() {
    let upstream = Upstream::new();
    let sandbox = Sandbox::new();
    let id = upstream.source_id();
    let locks = lock_for(&sandbox);
    let staging = sandbox
        .layout
        .tmp_dir()
        .join(format!("clone-{}", id.checkout_dir_name()));
    let other_staging = sandbox.layout.tmp_dir().join("clone-0123456789ab");
    let trash = sandbox.layout.tmp_dir().join("trash-old-1");
    for dir in [&staging, &other_staging, &trash] {
        std::fs::create_dir_all(dir.join("objects")).unwrap();
    }

    // Another operation is cloning this source right now: it holds the lock.
    let held = locks.acquire(&id).unwrap();
    sandbox.client.collect_garbage(&locks);
    assert!(
        staging.exists(),
        "the staging directory of a locked source stays"
    );
    assert!(!other_staging.exists(), "unlocked staging directories go");
    assert!(!trash.exists(), "trash goes whatever is locked");

    // The lock is held while garbage is collected, so a clone cannot start meanwhile;
    // once released, the leftover is garbage.
    drop(held);
    sandbox.client.collect_garbage(&locks);
    assert!(!staging.exists());
}
