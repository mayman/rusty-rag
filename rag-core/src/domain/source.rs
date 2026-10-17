//! Source: a registered repository (the Catalog aggregate root).
//!
//! A source has two separate facts. Its lifecycle `SourceStatus` says what is
//! happening to it (`Cloning`, `Indexing`, `Ready`, `Failed`, `Removing`). Its
//! optional `IndexedRevision` says which commit the stored index reflects, when it
//! was built, and with which tokenizer, chunker, and policy versions. A source is
//! searchable when it has an indexed revision and is not `Removing`, so an update
//! in progress, or a failed update, keeps serving the old index with its old
//! commit, while a source being removed stops appearing in results at once.
//!
//! Invariant: a `Ready` source has an indexed revision.

use std::fmt;

use crate::domain::git_location::GitLocation;
use crate::domain::git_ref::GitRef;
use crate::domain::index_versions::IndexVersions;
use crate::domain::indexed_at::IndexedAt;
use crate::domain::oid::CommitId;
use crate::domain::source_id::SourceId;

/// Where a source is in its life cycle.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SourceStatus {
    Cloning,
    Indexing,
    Ready,
    Failed,
    Removing,
}

impl SourceStatus {
    /// Stable string for storage and output.
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Cloning => "cloning",
            Self::Indexing => "indexing",
            Self::Ready => "ready",
            Self::Failed => "failed",
            Self::Removing => "removing",
        }
    }

    pub fn parse(text: &str) -> Option<Self> {
        [
            Self::Cloning,
            Self::Indexing,
            Self::Ready,
            Self::Failed,
            Self::Removing,
        ]
        .into_iter()
        .find(|status| status.as_str() == text)
    }
}

/// The commit a source was last indexed at, when, and with which rule versions.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IndexedRevision {
    pub commit: CommitId,
    pub at: IndexedAt,
    pub versions: IndexVersions,
}

/// The failure that last stopped an operation on a source: the stable error code,
/// the (already scrubbed) message, and when it happened. Cleared when the source is
/// indexed successfully again, so `rag list` can say why a source is `failed`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LastError {
    pub code: String,
    pub message: String,
    pub at: IndexedAt,
}

/// Rehydration found a `Ready` source without an indexed revision.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReadyWithoutRevision;

impl fmt::Display for ReadyWithoutRevision {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("a ready source needs an indexed commit")
    }
}

impl std::error::Error for ReadyWithoutRevision {}

/// The key under which source names are unique: the name, lowercased. Two names
/// with the same key are the same name. Stores use it for uniqueness and lookup.
pub fn name_key(name: &str) -> String {
    name.to_lowercase()
}

/// A registered repository.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Source {
    id: SourceId,
    name: String,
    location: GitLocation,
    git_ref: Option<GitRef>,
    status: SourceStatus,
    indexed: Option<IndexedRevision>,
    last_error: Option<LastError>,
}

impl Source {
    /// A newly registered source: `Cloning`, nothing indexed.
    pub fn register(id: SourceId, name: &str, location: GitLocation) -> Self {
        Self {
            id,
            name: name.to_owned(),
            location,
            git_ref: None,
            status: SourceStatus::Cloning,
            indexed: None,
            last_error: None,
        }
    }

    /// Rebuilds a stored source, checking the `Ready` invariant.
    pub fn restore(
        id: SourceId,
        name: String,
        location: GitLocation,
        git_ref: Option<GitRef>,
        status: SourceStatus,
        indexed: Option<IndexedRevision>,
    ) -> Result<Self, ReadyWithoutRevision> {
        if status == SourceStatus::Ready && indexed.is_none() {
            return Err(ReadyWithoutRevision);
        }
        Ok(Self {
            id,
            name,
            location,
            git_ref,
            status,
            indexed,
            last_error: None,
        })
    }

    /// Puts back the stored last error of a restored source.
    pub fn with_last_error(mut self, last_error: Option<LastError>) -> Self {
        self.last_error = last_error;
        self
    }

    pub fn id(&self) -> &SourceId {
        &self.id
    }

    /// The user-facing name (defaults to the slug). Unique across sources when
    /// compared through `name_key`.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// `name_key` of this source's name.
    pub fn name_key(&self) -> String {
        name_key(&self.name)
    }

    pub fn location(&self) -> &GitLocation {
        &self.location
    }

    /// The branch or ref being tracked, once resolved.
    pub fn git_ref(&self) -> Option<&GitRef> {
        self.git_ref.as_ref()
    }

    pub fn status(&self) -> SourceStatus {
        self.status
    }

    pub fn indexed(&self) -> Option<&IndexedRevision> {
        self.indexed.as_ref()
    }

    /// Why the source last failed; `None` after a successful index.
    pub fn last_error(&self) -> Option<&LastError> {
        self.last_error.as_ref()
    }

    /// Searchable means "has an indexed revision and is not being removed". An
    /// update in progress or a failed update still serves the old index; a
    /// `Removing` source never appears in results.
    pub fn is_searchable(&self) -> bool {
        self.indexed.is_some() && self.status != SourceStatus::Removing
    }

    pub fn track(&mut self, git_ref: &GitRef) {
        self.git_ref = Some(git_ref.clone());
    }

    pub fn mark_indexing(&mut self) {
        self.status = SourceStatus::Indexing;
    }

    /// Marks the source `Ready` at `commit`, indexed with the current rule versions.
    pub fn mark_ready(&mut self, commit: CommitId, at: IndexedAt) {
        self.indexed = Some(IndexedRevision {
            commit,
            at,
            versions: IndexVersions::current(),
        });
        self.status = SourceStatus::Ready;
        self.last_error = None;
    }

    /// Marks the source `Failed`. A previous indexed revision is kept, and so is
    /// the index it describes.
    pub fn mark_failed(&mut self) {
        self.status = SourceStatus::Failed;
    }

    /// Marks the source `Failed` and keeps why, replacing any earlier error. The
    /// indexed revision is kept as with `mark_failed`.
    pub fn record_failure(&mut self, code: &str, message: &str, at: IndexedAt) {
        self.status = SourceStatus::Failed;
        self.last_error = Some(LastError {
            code: code.to_owned(),
            message: message.to_owned(),
            at,
        });
    }

    /// Marks the source as being removed: it is no longer searchable. The revision
    /// is kept until the removal completes.
    pub fn mark_removing(&mut self) {
        self.status = SourceStatus::Removing;
    }
}

#[cfg(test)]
mod tests {
    use super::{name_key, IndexedRevision, LastError, ReadyWithoutRevision, Source, SourceStatus};
    use crate::domain::git_location::GitLocation;
    use crate::domain::git_ref::GitRef;
    use crate::domain::index_versions::IndexVersions;
    use crate::domain::indexed_at::IndexedAt;
    use crate::domain::oid::CommitId;
    use crate::domain::source_id::SourceId;

    fn id() -> SourceId {
        SourceId::from_location(&GitLocation::parse("https://github.com/o/r").unwrap()).unwrap()
    }

    fn location() -> GitLocation {
        GitLocation::parse("https://github.com/o/r").unwrap()
    }

    fn commit() -> CommitId {
        CommitId::parse(&"ab".repeat(20)).unwrap()
    }

    fn at() -> IndexedAt {
        IndexedAt::from_epoch_seconds(10)
    }

    fn fresh() -> Source {
        Source::register(id(), "r", location())
    }

    fn all_statuses() -> [SourceStatus; 5] {
        [
            SourceStatus::Cloning,
            SourceStatus::Indexing,
            SourceStatus::Ready,
            SourceStatus::Failed,
            SourceStatus::Removing,
        ]
    }

    #[test]
    fn a_new_source_is_cloning_and_not_searchable() {
        let source = fresh();
        assert_eq!(source.status(), SourceStatus::Cloning);
        assert!(source.indexed().is_none());
        assert!(!source.is_searchable());
        assert_eq!(source.name(), "r");
        assert_eq!(source.location(), &location());
    }

    #[test]
    fn marking_ready_records_the_revision_with_the_current_index_versions() {
        let mut source = fresh();
        source.mark_indexing();
        assert_eq!(source.status(), SourceStatus::Indexing);
        source.mark_ready(commit(), at());
        assert_eq!(source.status(), SourceStatus::Ready);
        assert!(source.is_searchable());
        assert_eq!(
            source.indexed(),
            Some(&IndexedRevision {
                commit: commit(),
                at: at(),
                versions: IndexVersions::current()
            })
        );
    }

    #[test]
    fn failing_keeps_the_previous_revision_and_stays_searchable() {
        let mut source = fresh();
        source.mark_ready(commit(), at());
        source.mark_failed();
        assert_eq!(source.status(), SourceStatus::Failed);
        assert!(source.indexed().is_some());
        assert!(
            source.is_searchable(),
            "a failed update keeps serving the old index"
        );
    }

    #[test]
    fn an_update_in_progress_keeps_serving_the_old_index() {
        let mut source = fresh();
        source.mark_ready(commit(), at());
        source.mark_indexing();
        assert!(source.is_searchable());
        assert_eq!(source.indexed().map(|r| &r.commit), Some(&commit()));
    }

    #[test]
    fn a_source_that_never_finished_indexing_is_not_searchable() {
        let mut source = fresh();
        source.mark_indexing();
        source.mark_failed();
        assert!(!source.is_searchable());
    }

    #[test]
    fn removing_is_a_lifecycle_status_that_keeps_the_revision_but_is_not_searchable() {
        let mut source = fresh();
        source.mark_ready(commit(), at());
        source.mark_removing();
        assert_eq!(source.status(), SourceStatus::Removing);
        assert!(source.indexed().is_some());
        assert!(
            !source.is_searchable(),
            "a source being removed must stop appearing in results"
        );
    }

    #[test]
    fn every_status_but_removing_is_searchable_given_a_revision() {
        for status in all_statuses() {
            let revision = IndexedRevision {
                commit: commit(),
                at: at(),
                versions: IndexVersions::current(),
            };
            let source =
                Source::restore(id(), "r".into(), location(), None, status, Some(revision))
                    .unwrap();
            assert_eq!(
                source.is_searchable(),
                status != SourceStatus::Removing,
                "{status:?}"
            );
        }
    }

    fn error_at(second: i64) -> LastError {
        LastError {
            code: "network_or_auth".to_owned(),
            message: "git: operation timed out".to_owned(),
            at: IndexedAt::from_epoch_seconds(second),
        }
    }

    #[test]
    fn a_new_source_has_no_last_error() {
        assert_eq!(fresh().last_error(), None);
    }

    #[test]
    fn recording_a_failure_marks_the_source_failed_and_keeps_the_error() {
        let mut source = fresh();
        source.mark_ready(commit(), at());
        source.record_failure("network_or_auth", "git: operation timed out", at());
        assert_eq!(source.status(), SourceStatus::Failed);
        assert_eq!(source.last_error(), Some(&error_at(10)));
        assert!(source.indexed().is_some(), "the old index is still served");
    }

    #[test]
    fn the_last_error_survives_status_changes_until_the_source_is_ready_again() {
        let mut source = fresh();
        source.record_failure("error", "boom", at());
        source.mark_indexing();
        assert!(source.last_error().is_some());
        source.mark_ready(commit(), at());
        assert_eq!(source.last_error(), None, "a good index clears the error");
    }

    #[test]
    fn a_later_failure_replaces_the_earlier_error() {
        let mut source = fresh();
        source.record_failure("error", "first", IndexedAt::from_epoch_seconds(1));
        source.record_failure("not_found", "second", IndexedAt::from_epoch_seconds(2));
        let error = source.last_error().unwrap();
        assert_eq!(
            (error.code.as_str(), error.message.as_str()),
            ("not_found", "second")
        );
        assert_eq!(error.at, IndexedAt::from_epoch_seconds(2));
    }

    #[test]
    fn a_stored_error_can_be_put_back_on_a_restored_source() {
        let source = Source::restore(
            id(),
            "r".into(),
            location(),
            None,
            SourceStatus::Failed,
            None,
        )
        .unwrap()
        .with_last_error(Some(error_at(10)));
        assert_eq!(source.last_error(), Some(&error_at(10)));
    }

    #[test]
    fn tracking_records_the_ref() {
        let mut source = fresh();
        assert_eq!(source.git_ref(), None);
        let main = GitRef::parse("main").unwrap();
        source.track(&main);
        assert_eq!(source.git_ref(), Some(&main));
    }

    #[test]
    fn restore_rejects_ready_without_a_revision() {
        let result = Source::restore(
            id(),
            "r".into(),
            location(),
            None,
            SourceStatus::Ready,
            None,
        );
        assert_eq!(result, Err(ReadyWithoutRevision));
    }

    #[test]
    fn restore_accepts_other_statuses_without_a_revision() {
        for status in all_statuses() {
            if status != SourceStatus::Ready {
                let result = Source::restore(id(), "r".into(), location(), None, status, None);
                assert!(result.is_ok(), "{status:?}");
            }
        }
    }

    #[test]
    fn restore_keeps_stale_index_versions() {
        let stale = IndexedRevision {
            commit: commit(),
            at: at(),
            versions: IndexVersions {
                tokenizer: IndexVersions::current().tokenizer + 1,
                chunker: 0,
                policy: 0,
            },
        };
        let source = Source::restore(
            id(),
            "r".into(),
            location(),
            None,
            SourceStatus::Failed,
            Some(stale.clone()),
        )
        .unwrap();
        assert_eq!(source.indexed(), Some(&stale));
    }

    #[test]
    fn status_strings_round_trip() {
        for status in all_statuses() {
            assert_eq!(SourceStatus::parse(status.as_str()), Some(status));
        }
        assert_eq!(SourceStatus::Ready.as_str(), "ready");
        assert_eq!(SourceStatus::Removing.as_str(), "removing");
        assert_eq!(SourceStatus::parse("bogus"), None);
    }

    #[test]
    fn names_compare_case_insensitively_through_the_name_key() {
        assert_eq!(name_key("Docs"), name_key("dOCS"));
        assert_ne!(name_key("docs"), name_key("doc"));
        assert_eq!(name_key("\u{c4}rger"), name_key("\u{e4}rger"));
        let source = Source::register(id(), "MyRepo", location());
        assert_eq!(source.name_key(), name_key("myrepo"));
    }
}
