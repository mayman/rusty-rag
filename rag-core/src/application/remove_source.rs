//! Remove: drop a source's index and catalog row, then its clone.

use serde::Serialize;

use crate::application::dto::SCHEMA_VERSION;
use crate::application::error::AppError;
use crate::application::indexer::resolve_source;
use crate::application::ports::{ChunkStore, GitClient, OperationLock, SourceRepository};

/// Input of the remove use case.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoveRequest {
    /// Source name or slug.
    pub name: String,
}

impl RemoveRequest {
    pub fn new(name: &str) -> Self {
        Self {
            name: name.to_owned(),
        }
    }
}

/// Output of the remove use case.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct RemoveResponse {
    pub schema_version: u32,
    pub repo: String,
    pub name: String,
    /// False when the clone could not be deleted; leftovers are garbage-collected.
    pub checkout_removed: bool,
}

pub struct RemoveSource<'a> {
    sources: &'a dyn SourceRepository,
    store: &'a dyn ChunkStore,
    git: &'a dyn GitClient,
    locks: &'a dyn OperationLock,
}

impl<'a> RemoveSource<'a> {
    pub fn new(
        sources: &'a dyn SourceRepository,
        store: &'a dyn ChunkStore,
        git: &'a dyn GitClient,
        locks: &'a dyn OperationLock,
    ) -> Self {
        Self {
            sources,
            store,
            git,
            locks,
        }
    }

    pub fn execute(&self, request: &RemoveRequest) -> Result<RemoveResponse, AppError> {
        let id = resolve_source(self.sources, &request.name)?.id().clone();
        let _lock = self.locks.acquire(&id)?;
        // Look again under the lock: the source may have been removed meanwhile.
        let source = self.sources.get(&id)?.ok_or_else(|| {
            AppError::NotFound(format!("no source named {:?}", request.name).into())
        })?;
        // The catalog and the index go first, in one transaction, so the source
        // stops appearing in results at once. The clone is only disk space: a
        // leftover is garbage-collected later.
        self.store.remove_source(&id)?;
        let checkout_removed = self.git.remove_checkout(&id).is_ok();
        Ok(RemoveResponse {
            schema_version: SCHEMA_VERSION,
            repo: id.slug().to_owned(),
            name: source.name().to_owned(),
            checkout_removed,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::application::error::{ErrorCode, GitError};
    use crate::application::indexer_test_support::{GitSpy, World};

    fn indexed_world() -> World {
        let w = World::new();
        w.commit(&[("a.rs", "fn a() {}\n")]);
        w.add().unwrap();
        w
    }

    #[test]
    fn remove_drops_the_index_the_catalog_row_and_the_clone() {
        let w = indexed_world();
        let commit = w
            .store
            .get(&w.id())
            .unwrap()
            .unwrap()
            .indexed()
            .unwrap()
            .commit
            .clone();

        let response = w
            .remove_source()
            .execute(&RemoveRequest::new("example.com/o/r"))
            .unwrap();

        assert_eq!(response.repo, "example.com/o/r");
        assert!(response.checkout_removed);
        assert!(w.store.get(&w.id()).unwrap().is_none());
        assert!(w.store.manifest(&w.id()).unwrap().is_empty());
        assert_eq!(w.store.chunk_count(&w.id()).unwrap(), 0);
        assert!(
            w.git.ls_tree(&w.id(), &commit).is_err(),
            "the clone is gone"
        );
    }

    #[test]
    fn remove_finds_the_source_by_name() {
        let w = World::new();
        w.commit(&[("a.rs", "fn a() {}\n")]);
        w.add_source()
            .execute(&crate::application::add_source::AddRequest::named(
                crate::application::indexer_test_support::REMOTE,
                "Docs",
            ))
            .unwrap();
        let response = w
            .remove_source()
            .execute(&RemoveRequest::new("docs"))
            .unwrap();
        assert_eq!(response.name, "Docs");
    }

    #[test]
    fn removing_an_unknown_source_is_not_found() {
        let w = indexed_world();
        let error = w
            .remove_source()
            .execute(&RemoveRequest::new("nope"))
            .unwrap_err();
        assert_eq!(error.code(), ErrorCode::NotFound);
        assert!(w.store.get(&w.id()).unwrap().is_some());
    }

    #[test]
    fn remove_deletes_the_catalog_row_before_the_clone() {
        // If the clone cannot be deleted the source is gone from search all the same.
        let w = indexed_world();
        w.git.fail_next(GitError::Failed("access denied".into()));

        let response = w
            .remove_source()
            .execute(&RemoveRequest::new("example.com/o/r"))
            .unwrap();

        assert!(!response.checkout_removed, "a leftover is tolerated");
        assert!(w.store.get(&w.id()).unwrap().is_none());
    }

    #[test]
    fn remove_does_not_touch_git_when_the_source_is_unknown() {
        let w = World::new();
        let spy = GitSpy::new(&w.git);
        assert!(w
            .remove_source_via(&spy)
            .execute(&RemoveRequest::new("nope"))
            .is_err());
        assert!(spy.calls().is_empty());
    }

    #[test]
    fn remove_is_refused_while_the_source_is_locked() {
        let w = indexed_world();
        let held = w.locks.acquire(&w.id()).unwrap();
        let error = w
            .remove_source()
            .execute(&RemoveRequest::new("example.com/o/r"))
            .unwrap_err();
        assert_eq!(error.code().exit_code(), 6);
        assert!(w.store.get(&w.id()).unwrap().is_some());
        drop(held);
        assert!(w
            .remove_source()
            .execute(&RemoveRequest::new("example.com/o/r"))
            .is_ok());
    }

    #[test]
    fn a_removed_source_can_be_added_again() {
        let w = indexed_world();
        w.remove_source()
            .execute(&RemoveRequest::new("example.com/o/r"))
            .unwrap();
        let response = w.add().unwrap();
        assert!(!response.already_added);
        assert_eq!(w.store.manifest(&w.id()).unwrap().len(), 1);
    }

    #[test]
    fn remove_response_serializes_in_schema_v1_style() {
        let w = indexed_world();
        let response = w
            .remove_source()
            .execute(&RemoveRequest::new("example.com/o/r"))
            .unwrap();
        assert_eq!(
            serde_json::to_string(&response).unwrap(),
            r#"{"schema_version":1,"repo":"example.com/o/r","name":"example.com/o/r","checkout_removed":true}"#
        );
    }
}
