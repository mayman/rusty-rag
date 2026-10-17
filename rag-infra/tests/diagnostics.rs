//! `SystemDiagnostics` against a real temp data directory, the real `git`, and the
//! real operation lock. Nothing here may create or delete anything in the directory.

use std::time::Duration;

use rag_core::application::ports::{Diagnostics, LockState, OperationLock};
use rag_core::domain::git_location::GitLocation;
use rag_core::domain::source_id::SourceId;
use rag_infra::config::DataLayout;
use rag_infra::diagnostics::SystemDiagnostics;
use rag_infra::lock::FileOperationLock;
use rag_infra::sqlite::SqliteStore;
use tempfile::TempDir;

fn id(name: &str) -> SourceId {
    SourceId::from_location(&GitLocation::parse(&format!("https://example.com/o/{name}")).unwrap())
        .unwrap()
}

fn diagnostics(dir: &TempDir) -> (SystemDiagnostics, DataLayout) {
    let layout = DataLayout::new(dir.path().join("data"));
    (SystemDiagnostics::new(layout.clone()), layout)
}

fn names(dir: &std::path::Path) -> Vec<String> {
    let mut found: Vec<String> = std::fs::read_dir(dir)
        .map(|entries| {
            entries
                .flatten()
                .map(|entry| entry.file_name().to_string_lossy().into_owned())
                .collect()
        })
        .unwrap_or_default();
    found.sort();
    found
}

#[test]
fn the_data_dir_is_shown_with_forward_slashes() {
    let temp = TempDir::new().unwrap();
    let (diagnostics, _) = diagnostics(&temp);
    let shown = diagnostics.data_dir();
    assert!(!shown.contains('\\'), "{shown}");
    assert!(shown.ends_with("/data"), "{shown}");
}

#[test]
fn a_data_dir_that_does_not_exist_yet_is_readable_and_stays_absent() {
    let temp = TempDir::new().unwrap();
    let (diagnostics, layout) = diagnostics(&temp);

    assert!(diagnostics.check_data_dir().is_ok());
    assert!(!diagnostics.database(true).exists);
    assert!(!diagnostics.checkout_present(&id("r")));
    assert_eq!(diagnostics.operation_lock(&id("r")), LockState::Free);
    assert_eq!(diagnostics.leftovers(&[]).tmp_entries, 0);
    assert!(!layout.root().exists(), "a read-only look creates nothing");
}

#[test]
fn a_data_dir_that_is_a_file_is_unreadable() {
    let temp = TempDir::new().unwrap();
    std::fs::write(temp.path().join("data"), b"not a directory").unwrap();
    let (diagnostics, _) = diagnostics(&temp);
    assert!(diagnostics.check_data_dir().is_err());
}

#[test]
fn the_database_is_measured_and_its_integrity_checked_only_on_request() {
    let temp = TempDir::new().unwrap();
    let (diagnostics, layout) = diagnostics(&temp);
    let store = SqliteStore::open_for_write(layout.root()).unwrap();

    let quick = diagnostics.database(false);
    assert!(quick.exists);
    assert!(quick.size_bytes > 0);
    assert_eq!(quick.integrity, None);

    let checked = diagnostics.database(true);
    assert_eq!(checked.integrity.as_deref(), Some("ok"));
    drop(store);
}

#[test]
fn a_damaged_database_reports_a_problem_instead_of_failing() {
    let temp = TempDir::new().unwrap();
    let (diagnostics, layout) = diagnostics(&temp);
    std::fs::create_dir_all(layout.root()).unwrap();
    std::fs::write(layout.index_db(), vec![0x42u8; 8192]).unwrap();

    let facts = diagnostics.database(true);

    assert!(facts.exists);
    let problem = facts.integrity.expect("an answer");
    assert_ne!(problem, "ok");
    assert!(!problem.contains('\n'), "{problem}");
}

#[test]
fn git_is_found_and_its_version_line_read() {
    let temp = TempDir::new().unwrap();
    let (diagnostics, _) = diagnostics(&temp);
    let git = diagnostics.git();
    assert!(git.on_path);
    assert!(
        git.version_text
            .as_deref()
            .unwrap_or("")
            .starts_with("git version "),
        "{git:?}"
    );
}

#[test]
fn checkouts_and_leftovers_are_counted_and_never_deleted() {
    let temp = TempDir::new().unwrap();
    let (diagnostics, layout) = diagnostics(&temp);
    layout.ensure_dirs().unwrap();
    let owned = id("owned");
    std::fs::create_dir_all(layout.repos_dir().join(owned.checkout_dir_name())).unwrap();
    std::fs::create_dir_all(layout.repos_dir().join("orphan-one")).unwrap();
    std::fs::create_dir_all(layout.repos_dir().join("orphan-two")).unwrap();
    std::fs::create_dir_all(layout.tmp_dir().join("no-hooks")).unwrap();
    std::fs::create_dir_all(layout.tmp_dir().join("clone-abc")).unwrap();
    std::fs::write(layout.tmp_dir().join("trash-x"), b"").unwrap();

    assert!(diagnostics.checkout_present(&owned));
    assert!(!diagnostics.checkout_present(&id("other")));
    let found = diagnostics.leftovers(&[owned.checkout_dir_name().to_owned()]);

    assert_eq!(found.unregistered_checkouts, 2);
    assert_eq!(
        found.tmp_entries, 2,
        "the empty hooks directory is not a leftover"
    );
    assert_eq!(names(&layout.repos_dir()).len(), 3, "nothing deleted");
    assert_eq!(names(&layout.tmp_dir()).len(), 3, "nothing deleted");
}

#[test]
fn the_operation_lock_is_probed_without_waiting_and_without_taking_it_for_good() {
    let temp = TempDir::new().unwrap();
    let (diagnostics, layout) = diagnostics(&temp);
    layout.ensure_dirs().unwrap();
    let locks = FileOperationLock::new(layout.locks_dir());
    let source = id("r");
    assert_eq!(diagnostics.operation_lock(&source), LockState::Free);

    let held = locks
        .acquire_waiting(&source, "update", Duration::ZERO)
        .unwrap();
    assert_eq!(
        diagnostics.operation_lock(&source),
        LockState::Held(Some("update".to_owned()))
    );
    assert_eq!(diagnostics.operation_lock(&id("other")), LockState::Free);

    drop(held);
    assert_eq!(diagnostics.operation_lock(&source), LockState::Free);
    assert!(
        locks.acquire(&source).is_ok(),
        "the probe left the lock free"
    );
}
