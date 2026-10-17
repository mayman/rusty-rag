//! The list use case: every registered source with its status, indexed commit, age,
//! and chunk count. Clones have no working tree, so no clone path is reported.

use crate::application::dto::{ListResponse, SourceSummary};
use crate::application::error::AppError;
use crate::application::ports::{ChunkStore, Clock, SourceRepository};

/// `rag list`.
pub struct ListSources<'a> {
    sources: &'a dyn SourceRepository,
    chunks: &'a dyn ChunkStore,
    clock: &'a dyn Clock,
}

impl<'a> ListSources<'a> {
    pub fn new(
        sources: &'a dyn SourceRepository,
        chunks: &'a dyn ChunkStore,
        clock: &'a dyn Clock,
    ) -> Self {
        Self {
            sources,
            chunks,
            clock,
        }
    }

    pub fn execute(&self) -> Result<ListResponse, AppError> {
        let now = self.clock.now();
        let sources = self
            .sources
            .list()?
            .iter()
            .map(|source| {
                let chunks = self.chunks.chunk_count(source.id())?;
                Ok(SourceSummary::new(source, chunks, now))
            })
            .collect::<Result<Vec<_>, AppError>>()?;
        Ok(ListResponse::of(sources))
    }
}

#[cfg(test)]
mod tests {
    use super::ListSources;
    use crate::application::dto::SCHEMA_VERSION;
    use crate::application::ports::SourceRepository;
    use crate::application::search::fixtures::{index, COMMIT, INDEXED_AT};
    use crate::application::testkit::{FixedClock, InMemoryStore};
    use crate::domain::git_location::GitLocation;
    use crate::domain::indexed_at::IndexedAt;
    use crate::domain::source::Source;
    use crate::domain::source_id::SourceId;

    fn clock() -> FixedClock {
        FixedClock::new(IndexedAt::from_epoch_seconds(INDEXED_AT + 3600))
    }

    #[test]
    fn no_sources_is_an_empty_list_not_an_error() {
        let store = InMemoryStore::new();
        let response = ListSources::new(&store, &store, &clock())
            .execute()
            .unwrap();
        assert_eq!(response.schema_version, SCHEMA_VERSION);
        assert!(response.sources.is_empty());
        assert!(response.hint.unwrap().contains("rag add"));
    }

    #[test]
    fn an_indexed_source_reports_status_commit_age_and_chunks() {
        let store = InMemoryStore::new();
        index(
            &store,
            "https://github.com/o/r",
            "docs",
            &[("a.rs", "fn a() {}"), ("b.rs", "fn b() {}")],
        );
        let response = ListSources::new(&store, &store, &clock())
            .execute()
            .unwrap();
        let [summary] = &response.sources[..] else {
            panic!("one source")
        };
        assert_eq!(summary.repo, "github.com/o/r");
        assert_eq!(summary.name, "docs");
        assert_eq!(summary.status, "ready");
        assert_eq!(summary.commit.as_deref(), Some(&COMMIT[..7]));
        assert_eq!(summary.indexed_at.as_deref(), Some("2023-11-14T22:13:20Z"));
        assert_eq!(summary.age_seconds, Some(3600));
        assert_eq!(summary.chunks, 2);
    }

    #[test]
    fn a_source_still_cloning_has_no_commit_and_no_chunks() {
        let store = InMemoryStore::new();
        let location = GitLocation::parse("https://github.com/o/new").unwrap();
        let id = SourceId::from_location(&location).unwrap();
        store.save(&Source::register(id, "new", location)).unwrap();
        let response = ListSources::new(&store, &store, &clock())
            .execute()
            .unwrap();
        let summary = &response.sources[0];
        assert_eq!(summary.status, "cloning");
        assert_eq!(
            (summary.commit.as_deref(), summary.age_seconds),
            (None, None)
        );
        assert_eq!(summary.chunks, 0);
    }

    #[test]
    fn sources_are_listed_in_id_order() {
        let store = InMemoryStore::new();
        index(
            &store,
            "https://github.com/o/zeta",
            "zeta",
            &[("a.rs", "x")],
        );
        index(
            &store,
            "https://github.com/o/alpha",
            "alpha",
            &[("a.rs", "x")],
        );
        let response = ListSources::new(&store, &store, &clock())
            .execute()
            .unwrap();
        let names: Vec<&str> = response.sources.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(names, ["alpha", "zeta"]);
    }
}
