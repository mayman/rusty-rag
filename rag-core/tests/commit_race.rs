//! Runs the two-thread `commit_index` race contract against the in-memory store.
//!
//! The contract itself lives in `rag_core::application::testkit` and cannot spawn
//! threads (the architecture test keeps `std::thread` out of the application layer),
//! so this file supplies the runner. `rag-infra` runs the same contract against the
//! SQLite adapter with a runner like this one.
//!
//! Needs the `testkit` feature (`cargo test --all-features`, as `scripts/check.ps1`
//! does); without it this file compiles to nothing.
#![cfg(feature = "testkit")]

use std::sync::Barrier;

use rag_core::application::testkit::{contract_commit_race, InMemoryStore};

/// Starts both closures on their own OS thread at the same moment and returns when
/// both have finished. A panic in either closure fails the test.
fn run_concurrently(first: &(dyn Fn() + Sync), second: &(dyn Fn() + Sync)) {
    let start = Barrier::new(2);
    std::thread::scope(|scope| {
        scope.spawn(|| {
            start.wait();
            first();
        });
        scope.spawn(|| {
            start.wait();
            second();
        });
    });
}

#[test]
fn in_memory_store_lets_exactly_one_of_two_racing_commits_win() {
    contract_commit_race(InMemoryStore::new, run_concurrently);
}
