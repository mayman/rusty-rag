//! The overall deadline caps every git subprocess at the time that is left.

use std::time::Duration;

use rag_core::application::error::GitError;
use rag_core::application::ports::GitClient;
use rag_core::domain::git_ref::GitRef;
use rag_infra::deadline::Deadline;

use super::support::{Sandbox, Upstream};

fn main_ref() -> GitRef {
    GitRef::parse("main").unwrap()
}

#[test]
fn a_passed_deadline_stops_a_git_call_before_it_starts() {
    let upstream = Upstream::with_files(&[("a.txt", b"1")]);
    let sandbox = Sandbox::new();
    let client = sandbox
        .another_client()
        .with_deadline(Deadline::after(Duration::ZERO));

    let error = client.default_branch(&upstream.location()).unwrap_err();

    assert_eq!(error, GitError::DeadlineExceeded);
}

#[test]
fn a_deadline_during_a_clone_is_not_a_timeout_and_leaves_no_half_made_checkout() {
    let upstream = Upstream::with_files(&[("a.txt", b"1")]);
    let sandbox = Sandbox::new();
    let client = sandbox
        .another_client()
        .with_deadline(Deadline::after(Duration::from_millis(1)));
    let id = upstream.source_id();

    let error = client
        .clone_shallow(&id, &upstream.location(), &main_ref())
        .unwrap_err();

    assert_eq!(error, GitError::DeadlineExceeded);
    assert!(!sandbox.checkout_dir(&id).exists());
    assert_eq!(sandbox.scratch_leftovers(), Vec::<String>::new());
}

#[test]
fn a_fetch_into_an_existing_checkout_that_ran_out_of_time_keeps_the_checkout() {
    let upstream = Upstream::with_files(&[("a.txt", b"1")]);
    let sandbox = Sandbox::new();
    let id = upstream.source_id();
    let first = sandbox
        .client
        .clone_shallow(&id, &upstream.location(), &main_ref())
        .unwrap();
    upstream.write("b.txt", b"2");
    let newer = upstream.commit("two");
    let late = sandbox
        .another_client()
        .with_deadline(Deadline::after(Duration::ZERO));

    let error = late.fetch_update(&id, &main_ref()).unwrap_err();

    assert_eq!(error, GitError::DeadlineExceeded);
    // The re-run (no deadline) resumes on the same checkout and gets the news.
    let resumed = sandbox.client.fetch_update(&id, &main_ref()).unwrap();
    assert_ne!(first, resumed);
    assert_eq!(resumed.as_str(), newer);
}

#[test]
fn no_deadline_changes_nothing() {
    let upstream = Upstream::with_files(&[("a.txt", b"1")]);
    let sandbox = Sandbox::new();
    let client = sandbox.another_client().with_deadline(Deadline::none());
    assert!(client
        .clone_shallow(&upstream.source_id(), &upstream.location(), &main_ref())
        .is_ok());
}
