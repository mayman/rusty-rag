//! SearchFilters: which chunks a search may consider.

use crate::domain::chunk::Chunk;
use crate::domain::language::Language;
use crate::domain::path_glob::PathGlob;
use crate::domain::source_id::SourceId;

/// Restrictions on a search. Each empty list or `None` means "no restriction";
/// a chunk must satisfy every restriction that is set.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct SearchFilters {
    /// Only these sources.
    pub sources: Vec<SourceId>,
    /// Only these languages.
    pub languages: Vec<Language>,
    /// Only paths matching this glob.
    pub path: Option<PathGlob>,
}

impl SearchFilters {
    pub fn matches(&self, chunk: &Chunk) -> bool {
        (self.sources.is_empty() || self.sources.contains(&chunk.source))
            && (self.languages.is_empty() || self.languages.contains(&chunk.language))
            && self
                .path
                .as_ref()
                .is_none_or(|glob| glob.matches(&chunk.path))
    }
}

#[cfg(test)]
mod tests {
    use super::SearchFilters;
    use crate::domain::chunk::Chunk;
    use crate::domain::chunk_id::ChunkId;
    use crate::domain::git_location::GitLocation;
    use crate::domain::language::Language;
    use crate::domain::line_range::LineRange;
    use crate::domain::path_glob::PathGlob;
    use crate::domain::repo_path::RepoPath;
    use crate::domain::source_id::SourceId;

    fn source(location: &str) -> SourceId {
        SourceId::from_location(&GitLocation::parse(location).unwrap()).unwrap()
    }

    fn chunk(location: &str, path: &str) -> Chunk {
        let path = RepoPath::parse(path).unwrap();
        Chunk {
            id: ChunkId::parse(&"b".repeat(64)).unwrap(),
            source: source(location),
            language: Language::from_path(&path),
            path,
            lines: LineRange::new(1, 1).unwrap(),
            heading_path: Vec::new(),
            text: "x".to_owned(),
        }
    }

    #[test]
    fn no_restrictions_match_everything() {
        assert!(SearchFilters::default().matches(&chunk("https://h/o/r", "a.rs")));
    }

    #[test]
    fn source_filter_keeps_only_listed_sources() {
        let filters = SearchFilters {
            sources: vec![source("https://h/o/r")],
            ..SearchFilters::default()
        };
        assert!(filters.matches(&chunk("https://h/o/r", "a.rs")));
        assert!(!filters.matches(&chunk("https://h/o/other", "a.rs")));
    }

    #[test]
    fn language_filter_keeps_only_listed_languages() {
        let filters = SearchFilters {
            languages: vec![Language::parse("rust").unwrap()],
            ..SearchFilters::default()
        };
        assert!(filters.matches(&chunk("https://h/o/r", "a.rs")));
        assert!(!filters.matches(&chunk("https://h/o/r", "a.py")));
    }

    #[test]
    fn path_filter_uses_the_glob() {
        let filters = SearchFilters {
            path: Some(PathGlob::parse("src/**").unwrap()),
            ..SearchFilters::default()
        };
        assert!(filters.matches(&chunk("https://h/o/r", "src/a.rs")));
        assert!(!filters.matches(&chunk("https://h/o/r", "docs/a.md")));
    }

    #[test]
    fn every_set_restriction_must_hold() {
        let filters = SearchFilters {
            sources: vec![source("https://h/o/r")],
            languages: vec![Language::parse("rust").unwrap()],
            path: None,
        };
        assert!(!filters.matches(&chunk("https://h/o/r", "a.py")));
    }
}
