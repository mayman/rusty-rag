//! Chunker: splits a document into indexable chunks.
//!
//! Markdown is split at ATX headings and each chunk records its heading path.
//! Headings inside fenced code (CommonMark fence rules) and inside YAML front
//! matter do not count. Markdown sections shorter than 200 characters, or with no
//! text below their heading, merge into the following section and keep the heading
//! path of the first heading in the merged chunk.
//! Other text, and any section over 60 lines or 4000 characters, is split into line
//! windows of at most 60 lines and 4000 characters, with a 5-line overlap, breaking
//! after the last blank line in range when one exists. A single line longer than
//! 4000 characters is cut at character boundaries.
//! Line endings are LF, CRLF, or a lone CR, and a leading UTF-8 BOM is ignored, so
//! chunk text never contains `\r`. Line numbers are 1-based and inclusive. Output is
//! deterministic.

use std::collections::BTreeMap;

use crate::domain::chunk_id::ChunkId;
use crate::domain::line_range::LineRange;

/// Version of the chunking rules. Bump whenever chunk boundaries, text, or id
/// inputs change; an index built with another version must be reindexed. Nothing
/// is committed yet, so this stays at 1.
pub const CHUNKER_VERSION: u32 = 1;

/// Shortest code window that is cut at a blank line (lines).
const MIN_WINDOW: usize = 40;
/// Most lines in one window or section.
const MAX_WINDOW: usize = 60;
/// Lines shared by consecutive windows.
const OVERLAP: usize = 5;
/// Markdown sections with less text than this (characters) merge into the next one.
const MIN_SECTION_CHARS: usize = 200;
/// Hard cap on a chunk's text, in characters. Longer lines are split at character
/// boundaries.
pub const MAX_CHUNK_CHARS: usize = 4000;

/// Which splitting strategy applies to a document.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DocumentKind {
    Markdown,
    Code,
}

/// A searchable slice of one document.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Chunk {
    pub id: ChunkId,
    pub lines: LineRange,
    /// Headings from the outermost to the innermost enclosing one. Empty for code
    /// and for markdown text before the first heading.
    pub heading_path: Vec<String>,
    /// The chunk's lines joined with `\n`.
    pub text: String,
}

/// Domain service that splits documents into chunks.
#[derive(Debug, Clone, Copy, Default)]
pub struct Chunker;

impl Chunker {
    /// Splits `text` (a document at `path` in repository `repo_id`) into chunks
    /// in document order. Windows or sections with only blank lines are dropped.
    pub fn chunk(repo_id: &str, path: &str, kind: DocumentKind, text: &str) -> Vec<Chunk> {
        let lines = split_lines(text);
        let spans: Vec<(LineRange, Vec<String>)> = match kind {
            DocumentKind::Markdown => markdown_sections(&lines),
            DocumentKind::Code => LineRange::new(1, lines.len())
                .map(|whole| {
                    line_windows(&lines, whole)
                        .into_iter()
                        .map(|range| (range, Vec::new()))
                        .collect()
                })
                .unwrap_or_default(),
        };

        // A span is one chunk, unless it holds a single line longer than the cap.
        let mut pieces: Vec<(LineRange, Vec<String>, String)> = Vec::with_capacity(spans.len());
        for (range, heading_path) in spans {
            for body in capped_text(&lines[range.start() - 1..range.end()]) {
                pieces.push((range, heading_path.clone(), body));
            }
        }

        let mut chunks = Vec::with_capacity(pieces.len());
        let mut occurrences: BTreeMap<Vec<String>, usize> = BTreeMap::new();
        for (ordinal, (range, heading_path, body)) in pieces.into_iter().enumerate() {
            let locator = match kind {
                DocumentKind::Markdown => {
                    let seen = occurrences.entry(heading_path.clone()).or_insert(0);
                    let occurrence = *seen;
                    *seen += 1;
                    Locator::Heading {
                        path: &heading_path,
                        occurrence,
                    }
                }
                DocumentKind::Code => Locator::Ordinal(ordinal),
            };
            let id = chunk_id(repo_id, path, &locator, &body);
            chunks.push(Chunk {
                id,
                lines: range,
                heading_path,
                text: body,
            });
        }
        chunks
    }
}

/// What distinguishes a chunk from others with the same text in the same file.
enum Locator<'a> {
    /// Markdown: the heading path, plus how many earlier chunks had that path.
    Heading {
        path: &'a [String],
        occurrence: usize,
    },
    /// Code: the chunk's position in the file.
    Ordinal(usize),
}

/// Lines of `text`. A line ends at LF, CRLF, or a lone CR, so all three line
/// endings give the same lines. A leading UTF-8 BOM is skipped. A final line
/// break does not start a new line; empty text has no lines.
fn split_lines(text: &str) -> Vec<&str> {
    let text = text.strip_prefix('\u{feff}').unwrap_or(text);
    let bytes = text.as_bytes();
    let mut lines = Vec::new();
    let mut start = 0;
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'\n' => {
                lines.push(&text[start..i]);
                i += 1;
                start = i;
            }
            b'\r' => {
                lines.push(&text[start..i]);
                i += if bytes.get(i + 1) == Some(&b'\n') {
                    2
                } else {
                    1
                };
                start = i;
            }
            _ => i += 1,
        }
    }
    if start < bytes.len() {
        lines.push(&text[start..]);
    }
    lines
}

/// Number of characters in the lines of `range`, counting one newline between
/// consecutive lines.
fn span_chars(lines: &[&str], range: LineRange) -> usize {
    let span = &lines[range.start() - 1..range.end()];
    span.iter().map(|line| line.chars().count()).sum::<usize>() + span.len() - 1
}

/// The text of a span, cut into pieces of at most `MAX_CHUNK_CHARS` characters.
/// Spans of several lines are already within the cap, so only a single long line
/// is cut.
fn capped_text(lines: &[&str]) -> Vec<String> {
    let body = lines.join("\n");
    if body.chars().count() <= MAX_CHUNK_CHARS {
        return vec![body];
    }
    let chars: Vec<char> = body.chars().collect();
    chars
        .chunks(MAX_CHUNK_CHARS)
        .map(|piece| piece.iter().collect())
        .collect()
}

fn is_blank(lines: &[&str], line: usize) -> bool {
    lines[line - 1].trim().is_empty()
}

fn has_content(lines: &[&str], range: LineRange) -> bool {
    (range.start()..=range.end()).any(|line| !is_blank(lines, line))
}

/// Markdown sections: each heading starts a section that runs to the next
/// heading. Text before the first heading forms a section with an empty path.
/// Headings inside fenced code blocks and YAML front matter do not count. Short
/// sections merge into the following one, and sections too long for one chunk
/// are split into line windows that keep the section's heading path.
fn markdown_sections(lines: &[&str]) -> Vec<(LineRange, Vec<String>)> {
    // (first line, heading path) for every section start, in order.
    let mut starts: Vec<(usize, Vec<String>)> = vec![(1, Vec::new())];
    // Open headings, outermost first, as (level, title).
    let mut open: Vec<(usize, String)> = Vec::new();
    let mut fence: Option<Fence> = None;

    let mut index = front_matter_len(lines);
    while index < lines.len() {
        let line = lines[index];
        let number = index + 1;
        index += 1;
        if let Some(open_fence) = fence {
            if closes_fence(line, open_fence) {
                fence = None;
            }
            continue;
        }
        if let Some(opener) = fence_opener(line) {
            fence = Some(opener);
            continue;
        }
        if let Some((level, title)) = atx_heading(line) {
            open.retain(|(open_level, _)| *open_level < level);
            open.push((level, title));
            starts.push((number, open.iter().map(|(_, t)| t.clone()).collect()));
        }
    }

    let mut sections = Vec::new();
    for (i, (start, path)) in starts.iter().enumerate() {
        let end = starts.get(i + 1).map_or(lines.len(), |(next, _)| next - 1);
        // An empty range means a heading sits on the first line, so there is no preamble.
        if let Some(range) = LineRange::new(*start, end) {
            if has_content(lines, range) {
                sections.push((range, path.clone()));
            }
        }
    }

    let mut spans = Vec::with_capacity(sections.len());
    for (range, path) in merge_short_sections(lines, sections) {
        if is_oversized(lines, range) {
            spans.extend(
                line_windows(lines, range)
                    .into_iter()
                    .map(|window| (window, path.clone())),
            );
        } else {
            spans.push((range, path));
        }
    }
    spans
}

/// Number of lines in a YAML front matter block at the start of the document: a
/// `---` line, the lines up to the next `---` line, and that closing line. Zero
/// when the document does not start with a closed block.
fn front_matter_len(lines: &[&str]) -> usize {
    if lines.first().map(|line| line.trim_end()) != Some("---") {
        return 0;
    }
    lines
        .iter()
        .enumerate()
        .skip(1)
        .find(|(_, line)| line.trim_end() == "---")
        .map_or(0, |(closing, _)| closing + 1)
}

/// Merges each section shorter than `MIN_SECTION_CHARS`, or with no text below its
/// heading, into the following section. The merged section keeps the heading path
/// of its first heading. A short final section stays as it is.
fn merge_short_sections(
    lines: &[&str],
    sections: Vec<(LineRange, Vec<String>)>,
) -> Vec<(LineRange, Vec<String>)> {
    let mut merged = Vec::with_capacity(sections.len());
    let mut pending: Option<(LineRange, Vec<String>)> = None;
    for (range, path) in sections {
        let current = match pending.take() {
            Some((earlier, earlier_path)) => {
                let combined = LineRange::new(earlier.start(), range.end()).unwrap_or(range);
                let path = if earlier_path.is_empty() {
                    path
                } else {
                    earlier_path
                };
                (combined, path)
            }
            None => (range, path),
        };
        if is_short(lines, current.0) {
            pending = Some(current);
        } else {
            merged.push(current);
        }
    }
    merged.extend(pending);
    merged
}

/// A section is short when its text is under `MIN_SECTION_CHARS` characters, or
/// when it is a heading with nothing below it.
fn is_short(lines: &[&str], range: LineRange) -> bool {
    let heading_only = atx_heading(lines[range.start() - 1]).is_some()
        && !LineRange::new(range.start() + 1, range.end())
            .is_some_and(|body| has_content(lines, body));
    heading_only || span_chars(lines, range) < MIN_SECTION_CHARS
}

/// True when a section is too long for one chunk, in lines or characters.
fn is_oversized(lines: &[&str], range: LineRange) -> bool {
    range.end() - range.start() + 1 > MAX_WINDOW || span_chars(lines, range) > MAX_CHUNK_CHARS
}

/// An open code fence: its marker character and the length of its opening run.
#[derive(Debug, Clone, Copy)]
struct Fence {
    marker: u8,
    len: usize,
}

/// The line without its indentation, when the indentation is at most three spaces.
fn strip_indent(line: &str) -> Option<&str> {
    let indent = line.len() - line.trim_start_matches(' ').len();
    (indent <= 3).then(|| &line[indent..])
}

/// A CommonMark opening fence: at least three backticks or tildes. A backtick fence
/// may not have a backtick in its info string.
fn fence_opener(line: &str) -> Option<Fence> {
    let rest = strip_indent(line)?;
    let marker = *rest.as_bytes().first()?;
    if marker != b'`' && marker != b'~' {
        return None;
    }
    let len = rest.bytes().take_while(|&b| b == marker).count();
    if len < 3 || (marker == b'`' && rest[len..].contains('`')) {
        return None;
    }
    Some(Fence { marker, len })
}

/// A CommonMark closing fence: the same marker, at least as long as the opener,
/// with nothing but spaces or tabs after it.
fn closes_fence(line: &str, fence: Fence) -> bool {
    let Some(rest) = strip_indent(line) else {
        return false;
    };
    let run = rest.bytes().take_while(|&b| b == fence.marker).count();
    run >= fence.len && rest[run..].chars().all(|c| c == ' ' || c == '\t')
}

/// Parses an ATX heading (`#` to `######`, then a space or end of line, with at
/// most three spaces of indentation). Returns the level and the title.
fn atx_heading(line: &str) -> Option<(usize, String)> {
    let indent = line.len() - line.trim_start_matches(' ').len();
    if indent > 3 {
        return None;
    }
    let rest = &line[indent..];
    let level = rest.bytes().take_while(|&b| b == b'#').count();
    if !(1..=6).contains(&level) {
        return None;
    }
    let after = &rest[level..];
    if !(after.is_empty() || after.starts_with(' ') || after.starts_with('\t')) {
        return None;
    }
    let trimmed = after.trim();
    // A closing sequence of `#` counts only when preceded by a space.
    let without_closing = trimmed.trim_end_matches('#');
    let title = if without_closing.len() < trimmed.len()
        && (without_closing.is_empty() || without_closing.ends_with(' '))
    {
        without_closing.trim()
    } else {
        trimmed
    };
    Some((level, title.to_owned()))
}

/// Line windows covering `within`, each at most `MAX_WINDOW` lines and
/// `MAX_CHUNK_CHARS` characters. A window that is not the last one ends after the
/// last blank line in `MIN_WINDOW..` that fits, or at the largest that fits when
/// there is none. The next window starts `OVERLAP` lines before the previous end,
/// but always after the previous start, so every line is covered and each window
/// makes progress. A line longer than the cap forms a window of its own.
fn line_windows(lines: &[&str], within: LineRange) -> Vec<LineRange> {
    let mut windows = Vec::new();
    let mut start = within.start();
    loop {
        let fits = fitting_end(lines, start, within.end());
        let end = if fits == within.end() {
            fits
        } else {
            (start + MIN_WINDOW - 1..=fits)
                .rev()
                .find(|&line| is_blank(lines, line))
                .unwrap_or(fits)
        };
        if let Some(window) = LineRange::new(start, end) {
            if has_content(lines, window) {
                windows.push(window);
            }
        }
        if end == within.end() {
            return windows;
        }
        start = (end + 1).saturating_sub(OVERLAP).max(start + 1);
    }
}

/// The last line from `start` up to `limit` that keeps a window within both caps.
/// Returns `start` when that line alone is over the character cap.
fn fitting_end(lines: &[&str], start: usize, limit: usize) -> usize {
    let mut end = start;
    let mut chars = lines[start - 1].chars().count();
    for line in start + 1..=limit {
        if line - start + 1 > MAX_WINDOW {
            break;
        }
        let next = chars + 1 + lines[line - 1].chars().count();
        if next > MAX_CHUNK_CHARS {
            break;
        }
        chars = next;
        end = line;
    }
    end
}

/// Chunk id: blake3 over length-prefixed fields, so no two different inputs
/// share an encoding. The text enters as its own hash.
fn chunk_id(repo_id: &str, path: &str, locator: &Locator, text: &str) -> ChunkId {
    let mut hasher = blake3::Hasher::new();
    push_field(&mut hasher, repo_id.as_bytes());
    push_field(&mut hasher, path.as_bytes());
    match locator {
        Locator::Heading {
            path: headings,
            occurrence,
        } => {
            push_field(&mut hasher, b"heading");
            push_field(&mut hasher, &(headings.len() as u64).to_le_bytes());
            for heading in headings.iter() {
                push_field(&mut hasher, heading.as_bytes());
            }
            push_field(&mut hasher, &(*occurrence as u64).to_le_bytes());
        }
        Locator::Ordinal(ordinal) => {
            push_field(&mut hasher, b"ordinal");
            push_field(&mut hasher, &(*ordinal as u64).to_le_bytes());
        }
    }
    let text_hash = blake3::hash(text.as_bytes()).to_hex();
    push_field(&mut hasher, text_hash.as_bytes());
    ChunkId::from_hash(hasher.finalize())
}

fn push_field(hasher: &mut blake3::Hasher, bytes: &[u8]) {
    hasher.update(&(bytes.len() as u64).to_le_bytes());
    hasher.update(bytes);
}

#[cfg(test)]
mod tests {
    use super::{Chunk, Chunker, DocumentKind};

    /// Body text above the 200-character minimum, so a section is not merged away.
    const LONG: &str = "Lorem ipsum dolor sit amet, consectetur adipiscing elit, sed do eiusmod tempor incididunt ut labore et dolore magna aliqua. Ut enim ad minim veniam, quis nostrud exercitation ullamco laboris nisi ut aliquip ex ea commodo consequat.";

    fn lines_of(chunks: &[Chunk]) -> Vec<(usize, usize)> {
        chunks
            .iter()
            .map(|c| (c.lines.start(), c.lines.end()))
            .collect()
    }

    fn markdown(text: &str) -> Vec<Chunk> {
        Chunker::chunk("repo", "README.md", DocumentKind::Markdown, text)
    }

    fn code(text: &str) -> Vec<Chunk> {
        Chunker::chunk("repo", "src/lib.rs", DocumentKind::Code, text)
    }

    /// `count` non-blank lines, `line 1` .. `line N`.
    fn numbered_lines(count: usize) -> String {
        (1..=count)
            .map(|i| format!("line {i}"))
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// `count` lines where the 1-based positions in `blanks` are empty.
    fn with_blanks_at(count: usize, blanks: &[usize]) -> String {
        (1..=count)
            .map(|i| {
                if blanks.contains(&i) {
                    String::new()
                } else {
                    format!("x{i}")
                }
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn markdown_splits_at_each_heading_and_records_the_heading_path() {
        let chunks = markdown(&format!("# A\n{LONG}\n## B\n{LONG}\n"));
        assert_eq!(lines_of(&chunks), [(1, 2), (3, 4)]);
        assert_eq!(chunks[1].heading_path, ["A", "B"]);
    }

    #[test]
    fn markdown_text_before_the_first_heading_has_an_empty_heading_path() {
        let chunks = markdown(&format!("{LONG}\n# A\n{LONG}"));
        assert_eq!(lines_of(&chunks), [(1, 1), (2, 3)]);
        assert!(chunks[0].heading_path.is_empty());
    }

    #[test]
    fn short_preamble_merges_into_the_first_heading_section() {
        let chunks = markdown("intro\n# A\nbody");
        assert_eq!(lines_of(&chunks), [(1, 3)]);
        assert_eq!(chunks[0].heading_path, ["A"]);
    }

    #[test]
    fn markdown_shallower_heading_replaces_the_deeper_path() {
        let chunks = markdown(&format!("# A\n{LONG}\n## B\n{LONG}\n# C\n{LONG}\n"));
        assert_eq!(lines_of(&chunks), [(1, 2), (3, 4), (5, 6)]);
        assert_eq!(chunks[1].heading_path, ["A", "B"]);
        assert_eq!(chunks[2].heading_path, ["C"]);
    }

    #[test]
    fn heading_only_section_merges_into_the_following_section() {
        let chunks = markdown(&format!("# A\n## B\n{LONG}"));
        assert_eq!(lines_of(&chunks), [(1, 3)]);
        assert_eq!(chunks[0].heading_path, ["A"]);
    }

    #[test]
    fn short_section_merges_into_the_following_section_keeping_the_first_heading_path() {
        let chunks = markdown(&format!("# A\nshort\n## B\n{LONG}\n# C\n{LONG}"));
        assert_eq!(lines_of(&chunks), [(1, 4), (5, 6)]);
        assert_eq!(chunks[0].heading_path, ["A"]);
        assert_eq!(chunks[1].heading_path, ["C"]);
    }

    #[test]
    fn short_final_section_is_not_merged_backwards() {
        let chunks = markdown(&format!("# A\n{LONG}\n# Z\nx"));
        assert_eq!(lines_of(&chunks), [(1, 2), (3, 4)]);
        assert_eq!(chunks[1].heading_path, ["Z"]);
    }

    #[test]
    fn bom_at_the_start_is_not_text_and_does_not_hide_the_first_heading() {
        let chunks = markdown(&format!("\u{feff}# A\n{LONG}"));
        assert_eq!(lines_of(&chunks), [(1, 2)]);
        assert_eq!(chunks[0].heading_path, ["A"]);
        assert!(!chunks[0].text.starts_with('\u{feff}'));
    }

    #[test]
    fn lone_carriage_returns_are_line_breaks() {
        let chunks = code("a\rb\rc");
        assert_eq!(lines_of(&chunks), [(1, 3)]);
        assert_eq!(chunks[0].text, "a\nb\nc");
    }

    #[test]
    fn lone_carriage_returns_before_a_line_feed_count_as_one_break() {
        let chunks = code("a\r\rb");
        assert_eq!(chunks[0].text, "a\n\nb");
    }

    #[test]
    fn unclosed_fence_hides_headings_to_the_end_of_the_document() {
        let chunks = markdown(&format!("```\n# H\n{LONG}"));
        assert_eq!(lines_of(&chunks), [(1, 3)]);
        assert!(chunks[0].heading_path.is_empty());
    }

    #[test]
    fn four_backtick_fence_is_not_closed_by_an_inner_triple_fence() {
        let chunks = markdown(&format!(
            "````\n```\n# Hidden\n```\n{LONG}\n````\n# Shown\n{LONG}"
        ));
        assert_eq!(lines_of(&chunks), [(1, 6), (7, 8)]);
        assert_eq!(chunks[1].heading_path, ["Shown"]);
    }

    #[test]
    fn fence_closer_with_an_info_string_does_not_close_the_fence() {
        let chunks = markdown(&format!(
            "```\n# H\n```js\n# Hidden\n{LONG}\n```\n# Shown\n{LONG}"
        ));
        assert_eq!(lines_of(&chunks), [(1, 6), (7, 8)]);
        assert_eq!(chunks[1].heading_path, ["Shown"]);
    }

    #[test]
    fn backtick_fence_info_string_may_not_contain_a_backtick() {
        let chunks = markdown(&format!("```a`b\n# H\n{LONG}"));
        assert_eq!(chunks[0].heading_path, ["H"]);
    }

    #[test]
    fn fence_indented_four_spaces_is_not_a_fence() {
        let chunks = markdown(&format!("    ```\n# H\n{LONG}"));
        assert_eq!(chunks[0].heading_path, ["H"]);
    }

    #[test]
    fn yaml_front_matter_is_not_scanned_for_headings() {
        let chunks = markdown(&format!(
            "---\ntitle: {LONG}\n# not a heading\n---\n# Real\n{LONG}"
        ));
        assert_eq!(lines_of(&chunks), [(1, 4), (5, 6)]);
        assert_eq!(chunks[1].heading_path, ["Real"]);
    }

    #[test]
    fn oversized_markdown_section_is_split_into_line_windows_keeping_its_heading_path() {
        let chunks = markdown(&format!("# A\n{}", numbered_lines(130)));
        assert_eq!(lines_of(&chunks), [(1, 60), (56, 115), (111, 131)]);
        assert!(chunks.iter().all(|c| c.heading_path == ["A"]));
    }

    #[test]
    fn a_single_line_longer_than_the_cap_is_split_at_character_boundaries() {
        let line = "é".repeat(10_000);
        let chunks = code(&line);
        let sizes: Vec<usize> = chunks.iter().map(|c| c.text.chars().count()).collect();
        assert_eq!(sizes, [4000, 4000, 2000]);
        assert!(chunks
            .iter()
            .all(|c| c.lines.start() == 1 && c.lines.end() == 1));
        let rebuilt: String = chunks.iter().map(|c| c.text.as_str()).collect();
        assert_eq!(rebuilt, line);
    }

    #[test]
    fn code_window_ends_before_the_character_cap_is_exceeded() {
        let text = (1..=100)
            .map(|_| "y".repeat(100))
            .collect::<Vec<_>>()
            .join("\n");
        assert_eq!(lines_of(&code(&text)), [(1, 39), (35, 73), (69, 100)]);
    }

    #[test]
    fn markdown_hashes_inside_code_fences_are_not_headings() {
        let chunks = markdown("# A\n```sh\n# not a heading\n```\n");
        assert_eq!(lines_of(&chunks), [(1, 4)]);
    }

    #[test]
    fn markdown_hashtag_and_four_space_indent_are_not_headings() {
        let chunks = markdown("#hashtag\n    # indented\n");
        assert_eq!(lines_of(&chunks), [(1, 2)]);
        assert!(chunks[0].heading_path.is_empty());
    }

    #[test]
    fn markdown_repeated_heading_path_produces_separate_chunks() {
        let chunks = markdown(&format!("# A\n{LONG}\n# A\n{LONG}\n"));
        assert_eq!(lines_of(&chunks), [(1, 2), (3, 4)]);
    }

    #[test]
    fn markdown_crlf_headings_are_detected_and_text_has_no_carriage_return() {
        let chunks = markdown(&format!("# A\r\n{LONG}\r\n## B\r\n{LONG}"));
        assert_eq!(lines_of(&chunks), [(1, 2), (3, 4)]);
        assert!(chunks.iter().all(|c| !c.text.contains('\r')));
    }

    #[test]
    fn empty_documents_have_no_chunks() {
        assert!(markdown("").is_empty());
        assert!(code("").is_empty());
    }

    #[test]
    fn code_short_file_is_one_chunk() {
        assert_eq!(lines_of(&code(&numbered_lines(10))), [(1, 10)]);
    }

    #[test]
    fn code_sixty_lines_is_one_chunk() {
        assert_eq!(lines_of(&code(&numbered_lines(60))), [(1, 60)]);
    }

    #[test]
    fn code_window_overlaps_the_next_window_by_five_lines() {
        assert_eq!(lines_of(&code(&numbered_lines(61))), [(1, 60), (56, 61)]);
    }

    #[test]
    fn code_breaks_after_the_last_blank_line_inside_the_window() {
        let text = with_blanks_at(100, &[45, 55]);
        assert_eq!(lines_of(&code(&text)), [(1, 55), (51, 100)]);
    }

    #[test]
    fn code_ignores_blank_lines_before_the_minimum_window_length() {
        let text = with_blanks_at(100, &[30]);
        assert_eq!(lines_of(&code(&text)), [(1, 60), (56, 100)]);
    }

    #[test]
    fn code_text_joins_lines_with_lf_and_drops_carriage_returns() {
        let chunks = code("a\r\nb\r\n");
        assert_eq!(chunks[0].text, "a\nb");
    }

    #[test]
    fn code_treats_each_carriage_return_before_a_line_feed_as_one_break() {
        let chunks = code("a\r\r\nb");
        assert_eq!(chunks[0].text, "a\n\nb");
    }

    #[test]
    fn code_windows_with_only_blank_lines_are_not_emitted() {
        let text = format!("x{}", "\n".repeat(120));
        assert_eq!(lines_of(&code(&text)), [(1, 60)]);
    }

    #[test]
    fn chunk_id_is_64_lowercase_hex_characters() {
        let chunks = markdown("# A\nx");
        let id = chunks[0].id.as_str();
        assert_eq!(id.len(), 64);
        assert!(id
            .chars()
            .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase()));
    }

    #[test]
    fn chunk_id_is_stable_for_identical_input() {
        let first = code(&numbered_lines(70));
        let second = code(&numbered_lines(70));
        assert_eq!(first, second);
    }

    #[test]
    fn chunk_id_changes_when_the_text_changes() {
        let before = markdown("# A\nx");
        let after = markdown("# A\ny");
        assert_ne!(before[0].id, after[0].id);
    }

    #[test]
    fn chunk_id_changes_when_the_path_changes() {
        let a = Chunker::chunk("repo", "a.md", DocumentKind::Markdown, "# A\nx");
        let b = Chunker::chunk("repo", "b.md", DocumentKind::Markdown, "# A\nx");
        assert_ne!(a[0].id, b[0].id);
    }

    #[test]
    fn chunk_id_changes_when_the_repo_changes() {
        let a = Chunker::chunk("host/o/one", "a.md", DocumentKind::Markdown, "# A\nx");
        let b = Chunker::chunk("host/o/two", "a.md", DocumentKind::Markdown, "# A\nx");
        assert_ne!(a[0].id, b[0].id);
    }

    #[test]
    fn repeated_identical_sections_still_get_distinct_ids() {
        let chunks = markdown(&format!("# A\n{LONG}\n# A\n{LONG}\n"));
        assert_ne!(chunks[0].id, chunks[1].id);
    }
}

#[cfg(test)]
mod properties {
    use super::{split_lines, Chunker, DocumentKind, MAX_CHUNK_CHARS, MAX_WINDOW, OVERLAP};
    use proptest::prelude::*;

    /// Structured documents: blank lines, headings, fences, LF or CRLF, and arbitrary text.
    fn document() -> impl Strategy<Value = String> {
        let line = prop_oneof![
            Just(String::new()),
            Just("   ".to_owned()),
            Just("# Heading".to_owned()),
            Just("## Sub ##".to_owned()),
            Just("```".to_owned()),
            "[^\n]{0,30}",
        ];
        let line = prop_oneof![
            4 => line,
            1 => Just("z".repeat(4500)),
            1 => Just("\r".to_owned()),
        ];
        (prop::collection::vec(line, 0..150), any::<bool>())
            .prop_map(|(lines, crlf)| lines.join(if crlf { "\r\n" } else { "\n" }))
    }

    fn kinds() -> impl Strategy<Value = DocumentKind> {
        prop_oneof![Just(DocumentKind::Markdown), Just(DocumentKind::Code)]
    }

    proptest! {
        #[test]
        fn chunking_never_panics_on_arbitrary_text(text in any::<String>(), kind in kinds()) {
            let _ = Chunker::chunk("repo", "f", kind, &text);
        }

        #[test]
        fn ranges_are_ordered_and_inside_the_document(text in document(), kind in kinds()) {
            let line_count = split_lines(&text).len();
            for chunk in Chunker::chunk("repo", "f", kind, &text) {
                prop_assert!(chunk.lines.start() >= 1);
                prop_assert!(chunk.lines.start() <= chunk.lines.end());
                prop_assert!(chunk.lines.end() <= line_count);
            }
        }

        #[test]
        fn every_non_blank_line_is_covered_by_a_chunk(text in document(), kind in kinds()) {
            let lines = split_lines(&text);
            let chunks = Chunker::chunk("repo", "f", kind, &text);
            for (index, line) in lines.iter().enumerate() {
                if line.trim().is_empty() {
                    continue;
                }
                let number = index + 1;
                prop_assert!(
                    chunks.iter().any(|c| c.lines.contains(number)),
                    "line {} is not covered", number
                );
            }
        }

        #[test]
        fn chunks_start_in_non_decreasing_order(text in document(), kind in kinds()) {
            let chunks = Chunker::chunk("repo", "f", kind, &text);
            for pair in chunks.windows(2) {
                prop_assert!(pair[0].lines.start() <= pair[1].lines.start());
            }
        }

        #[test]
        fn no_chunk_exceeds_the_character_cap(text in document(), kind in kinds()) {
            for chunk in Chunker::chunk("repo", "f", kind, &text) {
                prop_assert!(chunk.text.chars().count() <= MAX_CHUNK_CHARS);
            }
        }

        #[test]
        fn code_windows_stay_within_limits(text in document()) {
            let chunks = Chunker::chunk("repo", "f", DocumentKind::Code, &text);
            for chunk in &chunks {
                let length = chunk.lines.end() - chunk.lines.start() + 1;
                prop_assert!(length <= MAX_WINDOW);
            }
            for pair in chunks.windows(2) {
                if pair[1].lines.start() <= pair[0].lines.end() {
                    let overlap = pair[0].lines.end() - pair[1].lines.start() + 1;
                    prop_assert!(overlap <= OVERLAP);
                }
            }
        }

        #[test]
        fn chunking_is_deterministic(text in document(), kind in kinds()) {
            prop_assert_eq!(
                Chunker::chunk("r", "f", kind, &text),
                Chunker::chunk("r", "f", kind, &text)
            );
        }

        #[test]
        fn chunk_text_never_contains_carriage_returns(text in document(), kind in kinds()) {
            for chunk in Chunker::chunk("r", "f", kind, &text) {
                prop_assert!(!chunk.text.contains('\r'));
            }
        }

        #[test]
        fn line_endings_do_not_change_chunks_or_ids(text in document(), kind in kinds()) {
            let lf = text.replace("\r\n", "\n").replace('\r', "\n");
            let crlf = lf.replace('\n', "\r\n");
            prop_assert_eq!(
                Chunker::chunk("r", "f", kind, &lf),
                Chunker::chunk("r", "f", kind, &crlf)
            );
        }
    }
}
