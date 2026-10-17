//! Show output: a slice of an indexed file, or one chunk, as JSON or plain text.

use rag_core::application::dto::ShowResponse;

use crate::output::human::{cell, terminal_safe};

/// `ShowResponse` as one line of JSON.
pub fn render_show_json(response: &ShowResponse) -> String {
    serde_json::to_string(response).expect("show responses hold only strings and numbers")
}

/// A header line (`repo path:first-last @ commit`) and the text with line numbers.
/// Repository text is untrusted: control characters are replaced.
pub fn render_show_human(response: &ShowResponse) -> String {
    let [first, last] = response.lines;
    let mut out = format!(
        "{} {}:{first}-{last} @ {}",
        cell(&response.repo),
        cell(&response.path),
        cell(&response.commit)
    );
    if let Some(total) = response.total_lines {
        out += &format!(" ({total} lines in the file)");
    }
    if let Some(chunk) = &response.chunk {
        out += &format!(" (chunk {})", cell(chunk));
    }
    out.push('\n');
    if first == 0 {
        out += "(empty)\n";
    } else {
        let width = last.to_string().len();
        for (offset, line) in response.text.split('\n').enumerate() {
            out += &format!("{:>width$} | {}\n", first + offset, terminal_safe(line));
        }
    }
    if response.truncated {
        out += &format!(
            "output cut at line {last}: ask for the rest with --lines {}-<end>\n",
            last + 1
        );
    }
    out
}

#[cfg(test)]
mod tests {
    use super::{render_show_human, render_show_json};
    use rag_core::application::dto::{ShowResponse, SCHEMA_VERSION, UNTRUSTED_CONTENT};

    fn file_slice() -> ShowResponse {
        ShowResponse {
            schema_version: SCHEMA_VERSION,
            content: UNTRUSTED_CONTENT,
            repo: "github.com/o/r".to_owned(),
            commit: "abababa".to_owned(),
            indexed_at: "2023-11-14T22:13:20Z".to_owned(),
            path: "src/a.rs".to_owned(),
            language: "rust".to_owned(),
            lines: [9, 11],
            total_lines: Some(40),
            truncated: false,
            chunk: None,
            text: "fn a() {\n    b()\n}".to_owned(),
        }
    }

    #[test]
    fn json_is_one_line_in_schema_order_without_absent_fields() {
        assert_eq!(
            render_show_json(&file_slice()),
            concat!(
                r#"{"schema_version":1,"content":"untrusted","repo":"github.com/o/r","commit":"abababa","#,
                r#""indexed_at":"2023-11-14T22:13:20Z","path":"src/a.rs","language":"rust","#,
                r#""lines":[9,11],"total_lines":40,"truncated":false,"#,
                r#""text":"fn a() {\n    b()\n}"}"#
            )
        );
    }

    #[test]
    fn human_output_numbers_the_lines_under_a_header() {
        let expected = "\
github.com/o/r src/a.rs:9-11 @ abababa (40 lines in the file)
 9 | fn a() {
10 |     b()
11 | }
";
        assert_eq!(render_show_human(&file_slice()), expected);
    }

    #[test]
    fn a_chunk_names_its_id_and_a_cut_names_the_next_line() {
        let mut chunk = file_slice();
        chunk.total_lines = None;
        chunk.chunk = Some("ab".repeat(32));
        chunk.truncated = true;
        let text = render_show_human(&chunk);
        assert!(
            text.contains(&format!("(chunk {})", "ab".repeat(32))),
            "{text}"
        );
        assert!(text.contains("--lines 12-<end>"), "{text}");
    }

    #[test]
    fn an_empty_file_says_so() {
        let mut empty = file_slice();
        empty.lines = [0, 0];
        empty.text = String::new();
        assert!(render_show_human(&empty).ends_with("(empty)\n"));
    }

    #[test]
    fn terminal_escapes_in_file_text_are_not_written_out() {
        let mut hostile = file_slice();
        hostile.lines = [1, 1];
        hostile.text = "x \u{1b}[2J\u{202e}y".to_owned();
        let text = render_show_human(&hostile);
        assert!(
            !text.contains('\u{1b}') && !text.contains('\u{202e}'),
            "{text}"
        );
    }
}
