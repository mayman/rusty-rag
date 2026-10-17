//! Add: register a source, clone it, and index its default branch.

use serde::Serialize;

use crate::application::dto::SCHEMA_VERSION;
use crate::application::error::{AppError, StorageError};
use crate::application::index_pipeline::IndexReport;
use crate::application::indexer::Indexer;
use crate::application::ports::{ChunkStore, Clock, GitClient, OperationLock, SourceRepository};
use crate::domain::git_location::GitLocation;
use crate::domain::source::Source;
use crate::domain::source_id::SourceId;

/// Input of the add use case.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AddRequest {
    /// A git URL or an absolute local path (the CLI canonicalises paths).
    pub location: String,
    /// `--name`; defaults to the slug.
    pub name: Option<String>,
}

impl AddRequest {
    pub fn new(location: &str) -> Self {
        Self {
            location: location.to_owned(),
            name: None,
        }
    }

    pub fn named(location: &str, name: &str) -> Self {
        Self {
            location: location.to_owned(),
            name: Some(name.to_owned()),
        }
    }
}

/// Output of the add use case.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct AddResponse {
    pub schema_version: u32,
    pub repo: String,
    pub name: String,
    pub status: String,
    pub commit: String,
    /// True when the location was already registered and the add acted as an update.
    pub already_added: bool,
    pub index: IndexReport,
}

pub struct AddSource<'a> {
    sources: &'a dyn SourceRepository,
    store: &'a dyn ChunkStore,
    git: &'a dyn GitClient,
    locks: &'a dyn OperationLock,
    clock: &'a dyn Clock,
}

impl<'a> AddSource<'a> {
    pub fn new(
        sources: &'a dyn SourceRepository,
        store: &'a dyn ChunkStore,
        git: &'a dyn GitClient,
        locks: &'a dyn OperationLock,
        clock: &'a dyn Clock,
    ) -> Self {
        Self {
            sources,
            store,
            git,
            locks,
            clock,
        }
    }

    pub fn execute(&self, request: &AddRequest) -> Result<AddResponse, AppError> {
        let location = GitLocation::parse(&request.location)
            .map_err(|error| AppError::Usage(error.to_string().into()))?;
        let id = SourceId::from_location(&location)
            .map_err(|error| AppError::Usage(error.to_string().into()))?;
        let _lock = self.locks.acquire(&id)?;
        let indexer = Indexer::new(self.sources, self.store, self.git, self.clock);

        let (source, report, already_added) = match self.sources.get(&id)? {
            // The same location again is an update.
            Some(source) if source.indexed().is_some() => {
                let refreshed = indexer.refresh(source)?;
                (refreshed.source, refreshed.report, true)
            }
            // A first attempt that failed: try again with the stored name.
            Some(source) => {
                let indexed = indexer.first_index(source)?;
                (indexed.source, indexed.report, false)
            }
            None => {
                let name = match request.name.as_deref().map(str::trim) {
                    Some(name) if !name.is_empty() => name.to_owned(),
                    _ => id.slug().to_owned(),
                };
                if self.sources.find_by_name(&name)?.is_some() {
                    return Err(StorageError::NameTaken(name.as_str().into()).into());
                }
                let indexed = indexer.first_index(Source::register(id, &name, location))?;
                (indexed.source, indexed.report, false)
            }
        };
        let commit = source
            .indexed()
            .map(|revision| revision.commit.to_string())
            .unwrap_or_default();
        Ok(AddResponse {
            schema_version: SCHEMA_VERSION,
            repo: source.id().slug().to_owned(),
            name: source.name().to_owned(),
            status: source.status().as_str().to_owned(),
            commit,
            already_added,
            index: report,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::application::error::{ErrorCode, GitError, StorageError};
    use crate::application::indexer_test_support::{id_of, GitSpy, World, REMOTE};
    use crate::domain::source::SourceStatus;

    fn code(error: &AppError) -> ErrorCode {
        error.code()
    }

    #[test]
    fn add_indexes_the_default_branch_and_marks_the_source_ready() {
        let w = World::new();
        let head = w.commit(&[
            ("src/lib.rs", "pub fn parse_config() {}\n"),
            ("README.md", "# Hi\ntext\n"),
        ]);

        let response = w.add().unwrap();

        assert_eq!(response.repo, "example.com/o/r");
        assert_eq!(response.name, "example.com/o/r");
        assert_eq!(response.status, "ready");
        assert_eq!(response.commit, head.to_string());
        assert!(!response.already_added);
        assert_eq!(response.index.files_added, 2);
        let source = w.store.get(&w.id()).unwrap().unwrap();
        assert_eq!(source.status(), SourceStatus::Ready);
        assert_eq!(source.indexed().unwrap().commit, head);
        assert_eq!(w.store.manifest(&w.id()).unwrap().len(), 2);
        assert!(w.store.chunk_count(&w.id()).unwrap() >= 2);
    }

    #[test]
    fn add_response_serializes_with_the_schema_version() {
        let w = World::new();
        w.commit(&[("a.rs", "fn a() {}\n")]);
        let json = serde_json::to_string(&w.add().unwrap()).unwrap();
        assert!(
            json.starts_with(r#"{"schema_version":1,"repo":"example.com/o/r""#),
            "{json}"
        );
        assert!(json.contains(r#""skipped":{}"#), "{json}");
    }

    #[test]
    fn add_names_the_source_by_slug_unless_told_otherwise() {
        let w = World::new();
        w.commit(&[("a.rs", "fn a() {}\n")]);
        let response = w
            .add_source()
            .execute(&AddRequest::named(REMOTE, "Docs"))
            .unwrap();
        assert_eq!(response.name, "Docs");
        assert_eq!(w.store.find_by_name("docs").unwrap().unwrap().id(), &w.id());
    }

    #[test]
    fn add_rejects_an_invalid_location_without_touching_anything() {
        let w = World::new();
        for bad in ["", "-oProxyCommand=x", "relative/path", "ext::sh -c x"] {
            let error = w.add_source().execute(&AddRequest::new(bad)).unwrap_err();
            assert_eq!(code(&error), ErrorCode::Usage, "{bad:?}");
        }
        assert!(w.store.list().unwrap().is_empty());
    }

    #[test]
    fn add_accepts_an_absolute_local_path() {
        let w = World::new();
        w.git.add_remote("/srv/repos/proj", "main");
        let head = w.git.commit("/srv/repos/proj", &[("a.rs", "fn a() {}\n")]);
        let response = w
            .add_source()
            .execute(&AddRequest::new("/srv/repos/proj"))
            .unwrap();
        assert_eq!(response.commit, head.to_string());
        assert!(response.repo.starts_with("local/proj"), "{}", response.repo);
    }

    #[test]
    fn add_refuses_a_name_used_by_another_source_before_cloning() {
        let w = World::new();
        w.commit(&[("a.rs", "fn a() {}\n")]);
        w.add_source()
            .execute(&AddRequest::named(REMOTE, "docs"))
            .unwrap();
        let other = "https://example.com/o/other";
        w.git.add_remote(other, "main");
        w.git.commit(other, &[("b.rs", "fn b() {}\n")]);

        let spy = GitSpy::new(&w.git);
        let error = w
            .add_source_via(&spy)
            .execute(&AddRequest::named(other, "DOCS"))
            .unwrap_err();

        assert!(
            matches!(error, AppError::Storage(StorageError::NameTaken(_))),
            "{error}"
        );
        assert_eq!(code(&error), ErrorCode::Usage);
        assert!(spy.calls().is_empty(), "no network before the name check");
        assert!(w.store.get(&id_of(other)).unwrap().is_none());
    }

    #[test]
    fn add_is_refused_with_exit_six_while_another_process_holds_the_lock() {
        let w = World::new();
        w.commit(&[("a.rs", "fn a() {}\n")]);
        let _held = w.locks.acquire(&w.id()).unwrap();
        let error = w.add().unwrap_err();
        assert_eq!(error, AppError::Storage(StorageError::Busy));
        assert_eq!(error.code().exit_code(), 6);
        assert!(w.store.get(&w.id()).unwrap().is_none());
    }

    #[test]
    fn add_releases_the_lock_when_it_finishes() {
        let w = World::new();
        w.commit(&[("a.rs", "fn a() {}\n")]);
        w.add().unwrap();
        assert!(w.locks.acquire(&w.id()).is_ok());
    }

    #[test]
    fn a_failed_clone_marks_the_source_failed_and_a_retry_succeeds() {
        let w = World::new();
        let head = w.commit(&[("a.rs", "fn a() {}\n")]);
        w.git.fail_next(GitError::Timeout);

        let error = w.add().unwrap_err();

        assert_eq!(code(&error), ErrorCode::NetworkOrAuth);
        let source = w.store.get(&w.id()).unwrap().unwrap();
        assert_eq!(source.status(), SourceStatus::Failed);
        assert!(!source.is_searchable(), "nothing was ever indexed");

        let response = w.add().unwrap();
        assert!(
            !response.already_added,
            "a source that never indexed is added anew"
        );
        assert_eq!(response.commit, head.to_string());
        assert_eq!(
            w.store.get(&w.id()).unwrap().unwrap().status(),
            SourceStatus::Ready
        );
    }

    #[test]
    fn a_failed_index_commit_leaves_no_files_and_a_retry_succeeds() {
        let w = World::new();
        w.commit(&[("a.rs", "fn a() {}\n")]);
        w.store.fail_next_commit();

        assert!(w.add().is_err());

        assert!(w.store.manifest(&w.id()).unwrap().is_empty());
        assert_eq!(
            w.store.get(&w.id()).unwrap().unwrap().status(),
            SourceStatus::Failed
        );
        w.add().unwrap();
        assert_eq!(w.store.manifest(&w.id()).unwrap().len(), 1);
    }

    #[test]
    fn adding_a_repository_without_commits_fails_with_not_found() {
        let w = World::new();
        let error = w.add().unwrap_err();
        assert_eq!(code(&error), ErrorCode::NotFound);
    }

    #[test]
    fn adding_a_location_that_does_not_exist_fails_with_not_found() {
        let w = World::new();
        let error = w
            .add_source()
            .execute(&AddRequest::new("https://example.com/o/missing"))
            .unwrap_err();
        assert_eq!(code(&error), ErrorCode::NotFound);
    }

    #[test]
    fn adding_a_registered_location_again_updates_it() {
        let w = World::new();
        w.commit(&[("a.rs", "fn a() {}\n"), ("b.rs", "fn b() {}\n")]);
        w.add().unwrap();
        let second = w.commit(&[("a.rs", "fn a2() {}\n"), ("b.rs", "fn b() {}\n")]);

        let response = w.add().unwrap();

        assert!(response.already_added);
        assert_eq!(response.commit, second.to_string());
        assert_eq!(response.index.files_changed, 1);
        assert_eq!(response.index.files_unchanged, 1);
        assert_eq!(w.store.list().unwrap().len(), 1);
    }

    #[test]
    fn re_adding_while_nothing_changed_is_a_no_op_that_keeps_the_index() {
        let w = World::new();
        let head = w.commit(&[("a.rs", "fn a() {}\n")]);
        w.add().unwrap();
        let before = w.store.chunks(&w.id()).unwrap();

        let response = w.add().unwrap();

        assert!(response.already_added);
        assert_eq!(response.commit, head.to_string());
        assert_eq!(w.store.chunks(&w.id()).unwrap(), before);
    }

    #[test]
    fn add_reports_skipped_files_and_never_indexes_secrets() {
        let token = format!("const T: &str = \"ghp_{}\";\n", "aB3".repeat(12));
        let w = World::new();
        w.commit(&[
            ("src/lib.rs", "pub fn parse_config() {}\n"),
            ("README.md", "# Hi\ntext\n"),
            (".env", "SECRET=hunter2\n"),
            ("deploy/id_rsa", "key material\n"),
            ("keys/server.pem", "pem material\n"),
            ("package-lock.json", "{}\n"),
            ("logo.png", "not really a png\n"),
            ("config/token.rs", &token),
            ("data/blob", "a\0b"),
            ("vendor/dep.rs", "fn dep() {}\n"),
        ]);

        let response = w.add().unwrap();

        let manifest = w.store.manifest(&w.id()).unwrap();
        let stored: Vec<&str> = manifest.iter().map(|(path, _)| path.as_str()).collect();
        assert_eq!(stored, ["README.md", "data/blob", "src/lib.rs"]);
        let skipped = &response.index.skipped;
        assert_eq!(skipped.get("secret_name"), Some(&3), "{skipped:?}");
        assert_eq!(skipped.get("secret_content"), Some(&1), "{skipped:?}");
        assert_eq!(skipped.get("lockfile"), Some(&1), "{skipped:?}");
        assert_eq!(skipped.get("binary"), Some(&2), "{skipped:?}");
        assert_eq!(skipped.get("vendored"), Some(&1), "{skipped:?}");
        let text: String = w
            .store
            .chunks(&w.id())
            .unwrap()
            .iter()
            .map(|chunk| chunk.text.clone())
            .collect();
        assert!(
            !text.contains("hunter2") && !text.contains("ghp_"),
            "{text}"
        );
        assert!(
            !stored.contains(&"config/token.rs"),
            "a file judged secret must not become showable through the manifest"
        );
    }

    #[test]
    fn files_skipped_by_content_are_recorded_without_chunks() {
        let w = World::new();
        w.commit(&[("data/blob", "a\0b"), ("src/lib.rs", "fn a() {}\n")]);
        w.add().unwrap();
        let chunks = w.store.chunks(&w.id()).unwrap();
        assert!(chunks
            .iter()
            .all(|chunk| chunk.path.as_str() != "data/blob"));
        assert_eq!(w.store.manifest(&w.id()).unwrap().len(), 2);
    }

    #[test]
    fn unsafe_paths_are_counted_in_the_report() {
        let w = World::new();
        w.commit(&[
            ("a.rs", "fn a() {}\n"),
            (".git/config", "x"),
            ("b\\c.rs", "x"),
        ]);
        let response = w.add().unwrap();
        assert_eq!(response.index.skipped_unsafe_paths, 2);
    }

    #[test]
    fn a_repository_over_the_file_cap_is_refused_with_a_clear_error() {
        use crate::domain::index_policy::MAX_FILES_PER_SOURCE;

        let w = World::new();
        let names: Vec<String> = (0..=MAX_FILES_PER_SOURCE)
            .map(|i| format!("f{i}.txt"))
            .collect();
        let files: Vec<(&str, &str)> = names.iter().map(|n| (n.as_str(), "x\n")).collect();
        w.commit(&files);

        let spy = GitSpy::new(&w.git);
        let error = w
            .add_source_via(&spy)
            .execute(&AddRequest::new(REMOTE))
            .unwrap_err();

        assert!(error.to_string().contains("too large"), "{error}");
        assert!(!spy.called("read_blobs"), "refused before any blob is read");
        let source = w.store.get(&w.id()).unwrap().unwrap();
        assert_eq!(source.status(), SourceStatus::Failed);
        assert!(source.indexed().is_none());
        assert!(w.store.manifest(&w.id()).unwrap().is_empty());
    }
}
