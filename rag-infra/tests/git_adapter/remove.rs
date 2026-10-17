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
fn collect_garbage_leaves_the_volume_probe_another_process_is_using() {
    // Another rag process writes a probe file into tmp/ and renames it into repos/
    // a moment later; sweeping it in between made that rename fail (found on macOS CI).
    let sandbox = Sandbox::new();
    let fresh = sandbox.layout.tmp_dir().join(".volume-probe-4242-0");
    std::fs::write(&fresh, b"").unwrap();
    let abandoned = sandbox.layout.tmp_dir().join(".volume-probe-4243-0");
    std::fs::write(&abandoned, b"").unwrap();
    let long_ago = std::time::SystemTime::now() - std::time::Duration::from_secs(3600);
    std::fs::File::options()
        .write(true)
        .open(&abandoned)
        .unwrap()
        .set_modified(long_ago)
        .unwrap();

    sandbox.client.collect_garbage(&lock_for(&sandbox));

    assert!(fresh.exists(), "a probe in use by another process stays");
    assert!(!abandoned.exists(), "a probe left behind long ago goes");
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

#[test]
fn unregistered_checkouts_are_removed_and_registered_ones_kept() {
    let upstream = Upstream::with_files(&[("a.txt", b"1")]);
    let sandbox = Sandbox::new();
    let kept = upstream.source_id();
    sandbox
        .client
        .clone_shallow(&kept, &upstream.location(), &main_ref())
        .unwrap();
    let orphan = sandbox.layout.repos_dir().join("0123456789ab");
    std::fs::create_dir_all(orphan.join("objects")).unwrap();
    std::fs::write(orphan.join("objects").join("junk"), "x").unwrap();

    let kept_name = kept.checkout_dir_name().to_owned();
    sandbox
        .client
        .collect_unregistered_checkouts(&lock_for(&sandbox), &|name| name == kept_name);

    assert!(!orphan.exists());
    assert!(sandbox.checkout_dir(&kept).is_dir());
}

#[test]
fn an_unregistered_checkout_whose_lock_is_held_is_left_alone() {
    let sandbox = Sandbox::new();
    let locks = lock_for(&sandbox);
    let busy = sandbox.layout.repos_dir().join("aaaaaaaaaaaa");
    std::fs::create_dir_all(&busy).unwrap();
    let guard = locks.try_acquire_named("aaaaaaaaaaaa");

    sandbox
        .client
        .collect_unregistered_checkouts(&locks, &|_| false);
    assert!(busy.is_dir(), "an add may be cloning it right now");

    drop(guard);
    sandbox
        .client
        .collect_unregistered_checkouts(&locks, &|_| false);
    assert!(!busy.exists());
}

#[test]
fn the_registry_is_asked_only_after_the_lock_is_taken() {
    let sandbox = Sandbox::new();
    let locks = lock_for(&sandbox);
    let dir = sandbox.layout.repos_dir().join("bbbbbbbbbbbb");
    std::fs::create_dir_all(&dir).unwrap();
    let other = lock_for(&sandbox);
    sandbox
        .client
        .collect_unregistered_checkouts(&locks, &|name| {
            assert!(
                other.try_acquire_named(name).is_err(),
                "the lock is held while the registry is consulted"
            );
            true
        });
    assert!(dir.is_dir());
}
