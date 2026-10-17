//! `FileOperationLock`: the shared contract, then real cross-process behavior.
//!
//! The cross-process tests re-run this very test binary as a child that takes the
//! lock and holds it until its stdin closes (`lock_child_holder`, which does
//! nothing unless `RAG_LOCK_CHILD_DIR` is set).

use std::io::{BufRead, BufReader, Read, Write};
use std::path::Path;
use std::process::{Child, ChildStdin, Command, Stdio};
use std::time::{Duration, Instant};

use rag_core::application::error::StorageError;
use rag_core::application::ports::OperationLock;
use rag_core::application::testkit::contract_operation_lock;
use rag_core::domain::git_location::GitLocation;
use rag_core::domain::source_id::SourceId;
use rag_infra::lock::FileOperationLock;

const CHILD_DIR_ENV: &str = "RAG_LOCK_CHILD_DIR";
const LOCKED_MARKER: &str = "RAG-CHILD-LOCKED";

fn source(name: &str) -> SourceId {
    let location = GitLocation::parse(&format!("https://example.com/o/{name}")).unwrap();
    SourceId::from_location(&location).unwrap()
}

#[test]
fn file_lock_satisfies_the_operation_lock_contract() {
    let temp = tempfile::tempdir().unwrap();
    contract_operation_lock(|| FileOperationLock::new(temp.path().join("locks")));
}

#[test]
fn separate_instances_over_one_directory_exclude_each_other() {
    let temp = tempfile::tempdir().unwrap();
    let first = FileOperationLock::new(temp.path().join("locks"));
    let second = FileOperationLock::new(temp.path().join("locks"));
    let held = first.acquire(&source("a")).unwrap();
    assert_eq!(
        second.acquire(&source("a")).unwrap_err(),
        StorageError::Busy
    );
    drop(held);
    second.acquire(&source("a")).unwrap();
}

#[test]
fn a_timed_out_wait_names_the_operation_in_progress() {
    let temp = tempfile::tempdir().unwrap();
    let first = FileOperationLock::new(temp.path().join("locks"));
    let second = FileOperationLock::new(temp.path().join("locks"));
    let _held = first
        .acquire_waiting(&source("a"), "add", Duration::ZERO)
        .unwrap();
    for wait in [Duration::ZERO, Duration::from_millis(60)] {
        assert_eq!(
            second
                .acquire_waiting(&source("a"), "update", wait)
                .unwrap_err(),
            StorageError::locked_by("add")
        );
    }
    assert_eq!(
        second.acquire(&source("a")).unwrap_err(),
        StorageError::Busy,
        "a plain acquire stays Busy"
    );
}

#[test]
fn a_lock_taken_without_a_name_does_not_inherit_an_old_operation() {
    let temp = tempfile::tempdir().unwrap();
    let lock = FileOperationLock::new(temp.path().join("locks"));
    drop(
        lock.acquire_waiting(&source("a"), "add", Duration::ZERO)
            .unwrap(),
    );
    let _held = lock.acquire(&source("a")).unwrap();
    assert_eq!(
        lock.acquire_waiting(&source("a"), "update", Duration::ZERO)
            .unwrap_err(),
        StorageError::Busy
    );
}

#[test]
fn a_wait_ends_as_soon_as_the_holder_lets_go() {
    let temp = tempfile::tempdir().unwrap();
    let dir = temp.path().join("locks");
    let holder = FileOperationLock::new(dir.clone());
    let waiter = FileOperationLock::new(dir);
    let held = holder.acquire(&source("a")).unwrap();
    let releaser = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(150));
        drop(held);
    });
    let started = Instant::now();
    waiter
        .acquire_waiting(&source("a"), "update", Duration::from_secs(10))
        .expect("the lock frees up while waiting");
    assert!(started.elapsed() < Duration::from_secs(5));
    releaser.join().unwrap();
}

#[test]
fn the_lock_file_is_named_by_the_checkout_hash_not_the_slug() {
    let temp = tempfile::tempdir().unwrap();
    let lock = FileOperationLock::new(temp.path().join("locks"));
    let id = source("a");
    let _held = lock.acquire(&id).unwrap();
    let names: Vec<String> = std::fs::read_dir(temp.path().join("locks"))
        .unwrap()
        .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    assert_eq!(names, [format!("{}.lock", id.checkout_dir_name())]);
}

/// Child side. Takes the lock, announces it, then blocks until stdin closes.
#[test]
fn lock_child_holder() {
    let Some(dir) = std::env::var_os(CHILD_DIR_ENV) else {
        return;
    };
    let lock = FileOperationLock::new(Path::new(&dir).to_path_buf());
    let _held = lock
        .acquire(&source("shared"))
        .expect("child takes the lock");
    let mut stdout = std::io::stdout();
    writeln!(stdout, "{LOCKED_MARKER}").unwrap();
    stdout.flush().unwrap();
    let mut sink = Vec::new();
    std::io::stdin().read_to_end(&mut sink).unwrap();
}

fn spawn_holder(dir: &Path) -> (Child, ChildStdin) {
    let mut child = Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "lock_child_holder",
            "--nocapture",
            "--test-threads=1",
        ])
        .env(CHILD_DIR_ENV, dir)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let stdin = child.stdin.take().unwrap();
    let mut lines = BufReader::new(child.stdout.as_mut().unwrap()).lines();
    loop {
        let line = lines.next().expect("child exited before locking").unwrap();
        // The harness prefixes the line with "test lock_child_holder ... ".
        if line.contains(LOCKED_MARKER) {
            break;
        }
    }
    (child, stdin)
}

#[test]
fn a_lock_held_by_another_process_is_busy_until_that_process_releases_it() {
    let temp = tempfile::tempdir().unwrap();
    let dir = temp.path().join("locks");
    let (mut child, stdin) = spawn_holder(&dir);
    let lock = FileOperationLock::new(dir);

    assert_eq!(
        lock.acquire(&source("shared")).unwrap_err(),
        StorageError::Busy
    );
    lock.acquire(&source("other"))
        .expect("other sources are free");

    drop(stdin);
    child.wait().unwrap();
    lock.acquire(&source("shared"))
        .expect("released when the holder exits");
}

#[test]
fn a_lock_is_released_when_the_holder_process_is_killed() {
    let temp = tempfile::tempdir().unwrap();
    let dir = temp.path().join("locks");
    let (mut child, _stdin) = spawn_holder(&dir);
    let lock = FileOperationLock::new(dir);
    assert_eq!(
        lock.acquire(&source("shared")).unwrap_err(),
        StorageError::Busy
    );

    child.kill().unwrap();
    child.wait().unwrap();
    lock.acquire(&source("shared"))
        .expect("the OS releases the lock of a crashed process");
}
