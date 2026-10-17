//! Chunk: a searchable slice of one document, and the fields indexed for it.

use crate::domain::chunk_id::ChunkId;
use crate::domain::identifier_splitter::IdentifierSplitter;
use crate::domain::language::Language;
use crate::domain::line_range::LineRange;
use crate::domain::repo_path::RepoPath;
use crate::domain::source_id::SourceId;

/// A searchable slice of one document in one source.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Chunk {
    pub id: ChunkId,
    pub source: SourceId,
    pub path: RepoPath,
    pub lines: LineRange,
    /// Headings from the outermost to the innermost enclosing one. Empty for code
    /// and for markdown text before the first heading.
    pub heading_path: Vec<String>,
    /// The chunk's lines joined with `\n`. Raw text, as shown in snippets.
    pub text: String,
    pub language: Language,
}

/// The three full-text columns of a chunk, already tokenized: lowercase tokens
/// separated by single spaces (see `IdentifierSplitter::index_tokens`). Adapters
/// store these verbatim and never tokenize on their own.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IndexFields {
    pub path: String,
    pub heading: String,
    pub body: String,
}

impl Chunk {
    /// The enclosing headings joined with ` > `, or `None` without a heading.
    pub fn heading(&self) -> Option<String> {
        (!self.heading_path.is_empty()).then(|| self.heading_path.join(" > "))
    }

    /// Tokens of the path, the heading path, and the text.
    pub fn index_fields(&self) -> IndexFields {
        let tokens = |text: &str| IdentifierSplitter::index_tokens(text).join(" ");
        IndexFields {
            path: tokens(self.path.as_str()),
            heading: tokens(&self.heading_path.join(" ")),
            body: tokens(&self.text),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::Chunk;
    use crate::domain::chunk_id::ChunkId;
    use crate::domain::git_location::GitLocation;
    use crate::domain::language::Language;
    use crate::domain::line_range::LineRange;
    use crate::domain::repo_path::RepoPath;
    use crate::domain::source_id::SourceId;

    fn chunk(path: &str, headings: &[&str], text: &str) -> Chunk {
        let path = RepoPath::parse(path).unwrap();
        Chunk {
            id: ChunkId::parse(&"a".repeat(64)).unwrap(),
            source: SourceId::from_location(&GitLocation::parse("https://github.com/o/r").unwrap())
                .unwrap(),
            language: Language::from_path(&path),
            path,
            lines: LineRange::new(1, 3).unwrap(),
            heading_path: headings.iter().map(|h| (*h).to_owned()).collect(),
            text: text.to_owned(),
        }
    }

    #[test]
    fn heading_joins_the_path_with_arrows() {
        assert_eq!(
            chunk("a.md", &["Guide", "Setup"], "x").heading().as_deref(),
            Some("Guide > Setup")
        );
    }

    #[test]
    fn heading_is_none_without_headings() {
        assert_eq!(chunk("a.rs", &[], "x").heading(), None);
    }

    #[test]
    fn path_field_holds_split_path_tokens() {
        let fields = chunk("src/getUserName.rs", &[], "x").index_fields();
        assert_eq!(fields.path, "srcgetusernamers src get user name rs");
    }

    #[test]
    fn heading_field_holds_split_heading_tokens() {
        let fields = chunk("a.md", &["Guide", "parseConfig"], "x").index_fields();
        assert_eq!(fields.heading, "guide parseconfig parse config");
    }

    #[test]
    fn body_field_holds_split_text_tokens() {
        let fields = chunk("a.rs", &[], "let x = parse_config();").index_fields();
        assert_eq!(fields.body, "let x parseconfig parse config");
    }

    #[test]
    fn snake_and_camel_case_text_produce_the_same_body_tokens() {
        let snake = chunk("a.rs", &[], "parse_config").index_fields();
        let camel = chunk("a.rs", &[], "parseConfig").index_fields();
        assert_eq!(snake.body, camel.body);
    }

    #[test]
    fn empty_heading_path_gives_an_empty_heading_field() {
        assert_eq!(chunk("a.rs", &[], "x").index_fields().heading, "");
    }
}
