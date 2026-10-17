//! Golden tests for the tokenizer and chunker. Inputs and expected outputs live in
//! `tests/golden/*.golden` and are loaded with `include_str!`. Expected outputs
//! were derived by hand from the rules in `domain::chunker` and
//! `domain::identifier_splitter`, not copied from program output.

use rag_core::domain::chunker::{Chunk, Chunker, DocumentKind};
use rag_core::domain::identifier_splitter::IdentifierSplitter;

const MARKDOWN_INPUT: &str = include_str!("golden/readme_sample.md.golden");
const MARKDOWN_CHUNKS: &str = include_str!("golden/readme_sample.chunks.golden");
const CODE_INPUT: &str = include_str!("golden/long_module.rs.golden");
const CODE_CHUNKS: &str = include_str!("golden/long_module.chunks.golden");
const IDENTIFIER_INPUT: &str = include_str!("golden/identifiers.golden");
const IDENTIFIER_TOKENS: &str = include_str!("golden/identifiers.tokens.golden");

/// One line per chunk: `start-end | heading path`, or `(none)` without a heading.
fn describe(chunks: &[Chunk]) -> String {
    chunks
        .iter()
        .map(|chunk| {
            let path = if chunk.heading_path.is_empty() {
                "(none)".to_owned()
            } else {
                chunk.heading_path.join(" / ")
            };
            format!("{}-{} | {}\n", chunk.lines.start(), chunk.lines.end(), path)
        })
        .collect()
}

#[test]
fn markdown_sample_merges_short_sections_into_the_following_section() {
    let chunks = Chunker::chunk("repo", "README.md", DocumentKind::Markdown, MARKDOWN_INPUT);
    assert_eq!(describe(&chunks), MARKDOWN_CHUNKS);
}

#[test]
fn long_code_sample_is_split_into_overlapping_windows_at_blank_lines() {
    let chunks = Chunker::chunk("repo", "src/long.rs", DocumentKind::Code, CODE_INPUT);
    assert_eq!(describe(&chunks), CODE_CHUNKS);
}

#[test]
fn identifier_sample_tokenizes_to_the_golden_tokens() {
    let actual: String = IDENTIFIER_INPUT
        .lines()
        .map(|line| format!("{}\n", IdentifierSplitter::index_tokens(line).join(" ")))
        .collect();
    assert_eq!(actual, IDENTIFIER_TOKENS);
}
