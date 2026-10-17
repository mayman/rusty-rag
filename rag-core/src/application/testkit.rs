//! In-memory fakes of every port, and contract test functions that any adapter of
//! the store and lock ports must pass. Enabled by the `testkit` feature (and always
//! in this crate's own tests). `rag-infra` runs `contract_source_repository`,
//! `contract_chunk_store`, `contract_chunk_search`, `contract_operation_lock`, and
//! `contract_commit_race` (which needs a thread runner from the caller) against its
//! real adapters.

mod contracts;
mod fakes;

pub use contracts::{
    contract_chunk_search, contract_chunk_store, contract_commit_race, contract_operation_lock,
    contract_source_repository,
};
pub use fakes::{FakeGitClient, FakeOperationLock, FixedClock, InMemoryStore};

#[cfg(test)]
mod tests {
    use super::*;
    use crate::application::error::{GitError, StorageError};
    use crate::application::ports::{
        ChunkSearcher, ChunkStore, Clock, GitClient, IndexChanges, IndexedFile, LockGuard,
        OperationLock, SourceRepository,
    };
    use crate::domain::git_location::GitLocation;
    use crate::domain::git_ref::GitRef;
    use crate::domain::indexed_at::IndexedAt;
    use crate::domain::oid::{BlobOid, CommitId};
    use crate::domain::repo_path::RepoPath;
    use crate::domain::source::Source;
    use crate::domain::source_id::SourceId;

    const REMOTE: &str = "https://example.com/o/r";

    fn source(name: &str) -> SourceId {
        SourceId::from_location(&location_of(name)).unwrap()
    }

    fn path(text: &str) -> RepoPath {
        RepoPath::parse(text).unwrap()
    }

    fn location() -> GitLocation {
        GitLocation::parse(REMOTE).unwrap()
    }

    fn location_of(name: &str) -> GitLocation {
        GitLocation::parse(&format!("https://example.com/o/{name}")).unwrap()
    }

    fn main_ref() -> GitRef {
        GitRef::parse("main").unwrap()
    }

    // --- every port is Send + Sync -------------------------------------------

    fn assert_send_sync<T: Send + Sync + ?Sized>() {}

    #[test]
    fn every_port_and_fake_is_send_and_sync() {
        assert_send_sync::<dyn Clock>();
        assert_send_sync::<dyn SourceRepository>();
        assert_send_sync::<dyn ChunkStore>();
        assert_send_sync::<dyn ChunkSearcher>();
        assert_send_sync::<dyn GitClient>();
        assert_send_sync::<dyn OperationLock>();
        assert_send_sync::<FixedClock>();
        assert_send_sync::<InMemoryStore>();
        assert_send_sync::<FakeGitClient>();
        assert_send_sync::<FakeOperationLock>();
    }

    #[test]
    fn a_lock_guard_can_move_between_threads() {
        fn assert_send<T: Send>() {}
        assert_send::<LockGuard>();
    }

    // --- contracts -----------------------------------------------------------

    #[test]
    fn in_memory_store_honours_the_source_repository_contract() {
        contract_source_repository(InMemoryStore::new);
    }

    #[test]
    fn in_memory_store_honours_the_chunk_store_contract() {
        contract_chunk_store(InMemoryStore::new);
    }

    #[test]
    fn in_memory_store_honours_the_search_contract() {
        contract_chunk_search(InMemoryStore::new);
    }

    #[test]
    fn the_race_contract_also_holds_when_the_commits_run_one_after_the_other() {
        // `tests/commit_race.rs` runs it on two real threads (feature `testkit`); a
        // sequential runner keeps the contract exercised by a plain `cargo test`.
        contract_commit_race(InMemoryStore::new, |first, second| {
            first();
            second();
        });
    }

    #[test]
    fn fake_operation_lock_honours_the_lock_contract() {
        contract_operation_lock(FakeOperationLock::new);
    }

    #[test]
    fn cloned_fake_locks_share_their_held_locks() {
        let lock = FakeOperationLock::new();
        let other = lock.clone();
        let _held = lock.acquire(&source("a")).unwrap();
        assert_eq!(other.acquire(&source("a")).unwrap_err(), StorageError::Busy);
    }

    // --- InMemoryStore failure injection ---------------------------------------

    #[test]
    fn an_injected_commit_failure_changes_nothing_and_hits_only_one_commit() {
        let store = InMemoryStore::new();
        let s = source("one");
        let mut indexed = Source::register(s.clone(), "one", location());
        store.save(&indexed).unwrap();
        indexed.mark_ready(
            CommitId::parse(&"11".repeat(20)).unwrap(),
            IndexedAt::from_epoch_seconds(5),
        );
        let file = IndexedFile {
            path: path("a.rs"),
            blob: BlobOid::parse(&"22".repeat(20)).unwrap(),
            size: 0,
            chunks: Vec::new(),
        };
        let changes = IndexChanges {
            upserts: vec![file],
            removals: Vec::new(),
        };

        store.fail_next_commit();
        assert!(matches!(
            store.commit_index(&indexed, None, &changes),
            Err(StorageError::Failed(_))
        ));
        assert!(store.manifest(&s).unwrap().is_empty(), "nothing was stored");
        assert!(
            store.get(&s).unwrap().unwrap().indexed().is_none(),
            "the source row was not updated"
        );
        store
            .commit_index(&indexed, None, &changes)
            .expect("only the next commit fails");
        assert_eq!(store.manifest(&s).unwrap().len(), 1);
    }

    // --- FixedClock ----------------------------------------------------------

    #[test]
    fn fixed_clock_moves_only_when_told() {
        let clock = FixedClock::new(IndexedAt::from_epoch_seconds(100));
        assert_eq!(clock.now().epoch_seconds(), 100);
        clock.advance(5);
        assert_eq!(clock.now().epoch_seconds(), 105);
        clock.set(IndexedAt::from_epoch_seconds(7));
        assert_eq!(clock.now().epoch_seconds(), 7);
    }

    // --- FakeGitClient -------------------------------------------------------

    fn git_with_remote() -> (FakeGitClient, SourceId) {
        let git = FakeGitClient::new();
        git.add_remote(REMOTE, "main");
        (git, source("r"))
    }

    #[test]
    fn fake_git_resolves_the_default_branch_and_clones_it() {
        let (git, s) = git_with_remote();
        let first = git.commit(REMOTE, &[("a.rs", "fn a() {}")]);
        assert_eq!(git.default_branch(&location()).unwrap(), main_ref());
        let cloned = git.clone_shallow(&s, &location(), &main_ref()).unwrap();
        assert_eq!(cloned, first);
        let listing = git.ls_tree(&s, &first).unwrap();
        assert_eq!(listing.entries.len(), 1);
        assert_eq!(listing.entries[0].path, path("a.rs"));
        assert_eq!(listing.entries[0].size, 9);
        assert_eq!(listing.skipped_unsafe, 0);
        let blobs = git
            .read_blobs(&s, &[listing.entries[0].blob.clone()])
            .unwrap();
        assert_eq!(blobs[&listing.entries[0].blob], b"fn a() {}");
    }

    #[test]
    fn fake_git_listing_is_ordered_by_path_and_counts_unsafe_paths() {
        let (git, s) = git_with_remote();
        let id = git.commit(
            REMOTE,
            &[
                ("z.rs", "z"),
                (".git/config", "evil"),
                ("a.rs", "a"),
                ("dir\\back.rs", "backslash"),
                ("evil\u{202e}gnp.exe", "bidi"),
                (
                    "con.txt",
                    "a device name is a fine name when nothing is checked out",
                ),
            ],
        );
        git.clone_shallow(&s, &location(), &main_ref()).unwrap();
        let listing = git.ls_tree(&s, &id).unwrap();
        let paths: Vec<&str> = listing.entries.iter().map(|e| e.path.as_str()).collect();
        assert_eq!(paths, ["a.rs", "con.txt", "z.rs"]);
        assert_eq!(
            listing.skipped_unsafe, 3,
            "unsafe paths are counted, not listed"
        );
    }

    #[test]
    fn fake_git_update_sees_new_commits_only_after_fetch() {
        let (git, s) = git_with_remote();
        let first = git.commit(REMOTE, &[("a.rs", "one")]);
        git.clone_shallow(&s, &location(), &main_ref()).unwrap();
        let second = git.commit(REMOTE, &[("a.rs", "two"), ("b.rs", "new")]);
        assert_ne!(first, second);
        assert!(
            matches!(git.ls_tree(&s, &second), Err(GitError::NotFound(_))),
            "not fetched yet"
        );
        assert_eq!(git.fetch_update(&s, &main_ref()).unwrap(), second);
        let old = git.ls_tree(&s, &first).unwrap();
        let new = git.ls_tree(&s, &second).unwrap();
        assert_ne!(
            old.entries[0].blob, new.entries[0].blob,
            "blob changed with content"
        );
        assert_eq!(new.entries.len(), 2);
    }

    #[test]
    fn fake_git_clone_is_idempotent() {
        let (git, s) = git_with_remote();
        let first = git.commit(REMOTE, &[("a.rs", "one")]);
        assert_eq!(
            git.clone_shallow(&s, &location(), &main_ref()).unwrap(),
            first
        );
        assert_eq!(
            git.clone_shallow(&s, &location(), &main_ref()).unwrap(),
            first,
            "cloning again over an existing clone succeeds"
        );
        let second = git.commit(REMOTE, &[("a.rs", "two")]);
        assert_eq!(
            git.clone_shallow(&s, &location(), &main_ref()).unwrap(),
            second,
            "a retried clone starts from the current remote"
        );
        assert!(git.ls_tree(&s, &second).is_ok());
    }

    #[test]
    fn fake_git_reads_blobs_in_a_batch_keyed_by_blob() {
        let (git, s) = git_with_remote();
        let id = git.commit(
            REMOTE,
            &[("a.rs", "alpha"), ("b.rs", "beta"), ("c.rs", "alpha")],
        );
        git.clone_shallow(&s, &location(), &main_ref()).unwrap();
        let listing = git.ls_tree(&s, &id).unwrap();
        let blob = |name: &str| {
            listing
                .entries
                .iter()
                .find(|e| e.path.as_str() == name)
                .unwrap()
                .blob
                .clone()
        };
        assert_eq!(blob("a.rs"), blob("c.rs"), "equal content, equal blob");
        let wanted = [blob("a.rs"), blob("b.rs"), blob("c.rs")];
        let contents = git.read_blobs(&s, &wanted).unwrap();
        assert_eq!(contents.len(), 2, "duplicate blobs collapse");
        assert_eq!(contents[&blob("a.rs")], b"alpha");
        assert_eq!(contents[&blob("b.rs")], b"beta");
        assert!(git.read_blobs(&s, &[]).unwrap().is_empty());
    }

    #[test]
    fn fake_git_missing_blob_fails_the_whole_batch() {
        let (git, s) = git_with_remote();
        let id = git.commit(REMOTE, &[("a.rs", "x")]);
        git.clone_shallow(&s, &location(), &main_ref()).unwrap();
        let present = git.ls_tree(&s, &id).unwrap().entries[0].blob.clone();
        let absent = BlobOid::parse(&"00".repeat(20)).unwrap();
        assert!(matches!(
            git.read_blobs(&s, &[present, absent]),
            Err(GitError::NotFound(_))
        ));
    }

    #[test]
    fn fake_git_blobs_of_unfetched_commits_are_not_readable() {
        let (git, s) = git_with_remote();
        git.commit(REMOTE, &[("a.rs", "one")]);
        git.clone_shallow(&s, &location(), &main_ref()).unwrap();
        let second = git.commit(REMOTE, &[("a.rs", "two")]);

        // A second clone, made after the new commit, can see its blob.
        let late = source("late");
        git.clone_shallow(&late, &location(), &main_ref()).unwrap();
        let unfetched = git.ls_tree(&late, &second).unwrap().entries[0].blob.clone();
        assert!(git
            .read_blobs(&late, std::slice::from_ref(&unfetched))
            .is_ok());
        assert!(
            matches!(git.read_blobs(&s, &[unfetched]), Err(GitError::NotFound(_))),
            "the first clone has not fetched that commit"
        );
    }
    #[test]
    fn fake_git_reports_missing_things_as_not_found() {
        let (git, s) = git_with_remote();
        let elsewhere = GitLocation::parse("https://nowhere.example/x/y").unwrap();
        assert!(matches!(
            git.default_branch(&elsewhere),
            Err(GitError::NotFound(_))
        ));
        assert!(
            matches!(
                git.clone_shallow(&s, &location(), &main_ref()),
                Err(GitError::NotFound(_))
            ),
            "empty repository"
        );
        let id = git.commit(REMOTE, &[("a.rs", "x")]);
        assert!(matches!(
            git.clone_shallow(&s, &location(), &GitRef::parse("other").unwrap()),
            Err(GitError::NotFound(_))
        ));
        assert!(
            matches!(git.ls_tree(&s, &id), Err(GitError::NotFound(_))),
            "not cloned"
        );
    }

    #[test]
    fn fake_git_injected_failure_hits_one_operation() {
        let (git, _) = git_with_remote();
        git.fail_next(GitError::Timeout);
        assert_eq!(git.default_branch(&location()), Err(GitError::Timeout));
        assert!(git.default_branch(&location()).is_ok());
        git.fail_next(GitError::Busy);
        assert_eq!(git.default_branch(&location()), Err(GitError::Busy));
    }

    #[test]
    fn fake_git_remove_checkout_forgets_it_and_tolerates_absence() {
        let (git, s) = git_with_remote();
        let id = git.commit(REMOTE, &[("a.rs", "x")]);
        git.clone_shallow(&s, &location(), &main_ref()).unwrap();
        git.remove_checkout(&s).unwrap();
        git.remove_checkout(&s).unwrap();
        assert!(git.ls_tree(&s, &id).is_err());
    }
}
