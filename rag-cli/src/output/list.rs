//! List output: the registered sources as schema v1 JSON.

use rag_core::application::dto::ListResponse;

/// `{"schema_version":1,"sources":[...]}` on one line.
pub fn render_list_json(response: &ListResponse) -> String {
    serde_json::to_string(response).expect("list responses hold only strings and numbers")
}

#[cfg(test)]
mod tests {
    use super::render_list_json;
    use crate::output::test_support::INDEXED_AT;
    use rag_core::application::dto::{ListResponse, SourceSummary, SCHEMA_VERSION};
    use rag_core::domain::git_location::GitLocation;
    use rag_core::domain::indexed_at::IndexedAt;
    use rag_core::domain::oid::CommitId;
    use rag_core::domain::source::Source;
    use rag_core::domain::source_id::SourceId;

    const GOLDEN: &str = include_str!("../../tests/golden/list_two_sources.json.golden");

    /// `r`: ready at `github.com/o/r`, two chunks. `x`: registered, still cloning.
    fn sources() -> Vec<SourceSummary> {
        let now = IndexedAt::from_epoch_seconds(INDEXED_AT + 90);
        let ready_location = GitLocation::parse("https://github.com/o/r").unwrap();
        let mut ready = Source::register(
            SourceId::from_location(&ready_location).unwrap(),
            "r",
            ready_location,
        );
        ready.mark_ready(
            CommitId::parse(&"ab".repeat(20)).unwrap(),
            IndexedAt::from_epoch_seconds(INDEXED_AT),
        );
        let cloning = Source::register(
            SourceId::from_location(&GitLocation::parse("https://github.com/o/x").unwrap())
                .unwrap(),
            "x",
            GitLocation::parse("https://github.com/o/x").unwrap(),
        );
        let failed_location = GitLocation::parse("https://github.com/o/y").unwrap();
        let mut failed = Source::register(
            SourceId::from_location(&failed_location).unwrap(),
            "y",
            failed_location,
        );
        failed.mark_ready(
            CommitId::parse(&"ab".repeat(20)).unwrap(),
            IndexedAt::from_epoch_seconds(INDEXED_AT),
        );
        failed.record_failure(
            "network_or_auth",
            "git: operation timed out",
            IndexedAt::from_epoch_seconds(INDEXED_AT + 60),
        );
        vec![
            SourceSummary::new(&ready, 2, now),
            SourceSummary::new(&cloning, 0, now),
            SourceSummary::new(&failed, 4, now),
        ]
    }

    #[test]
    fn list_output_matches_the_golden_file() {
        let json = render_list_json(&ListResponse {
            schema_version: SCHEMA_VERSION,
            sources: sources(),
        });
        assert_eq!(json, GOLDEN.trim_end());
    }

    #[test]
    fn an_empty_registry_is_an_empty_list_not_an_error() {
        assert_eq!(
            render_list_json(&ListResponse {
                schema_version: SCHEMA_VERSION,
                sources: vec![],
            }),
            r#"{"schema_version":1,"sources":[]}"#
        );
    }
}
