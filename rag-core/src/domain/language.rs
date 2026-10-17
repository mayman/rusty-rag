//! Language and DocumentKind: the policy mapping a path to a language name and to
//! the chunking strategy that applies.

use std::fmt;

use crate::domain::repo_path::RepoPath;

const TEXT: &str = "text";
const MARKDOWN: &str = "markdown";

/// Lowercase extension to language name.
const EXTENSIONS: &[(&str, &str)] = &[
    ("rs", "rust"),
    ("py", "python"),
    ("js", "javascript"),
    ("jsx", "javascript"),
    ("mjs", "javascript"),
    ("ts", "typescript"),
    ("tsx", "typescript"),
    ("go", "go"),
    ("java", "java"),
    ("kt", "kotlin"),
    ("c", "c"),
    ("h", "c"),
    ("cpp", "cpp"),
    ("cc", "cpp"),
    ("hpp", "cpp"),
    ("cs", "csharp"),
    ("rb", "ruby"),
    ("php", "php"),
    ("swift", "swift"),
    ("sh", "shell"),
    ("bash", "shell"),
    ("ps1", "powershell"),
    ("sql", "sql"),
    ("toml", "toml"),
    ("yaml", "yaml"),
    ("yml", "yaml"),
    ("json", "json"),
    ("html", "html"),
    ("css", "css"),
    ("md", MARKDOWN),
    ("markdown", MARKDOWN),
    ("mdx", MARKDOWN),
    ("txt", TEXT),
];

/// Exact file names that decide the language regardless of extension.
const FILE_NAMES: &[(&str, &str)] = &[("Makefile", "makefile"), ("Dockerfile", "dockerfile")];

/// Which chunking strategy applies to a document.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DocumentKind {
    /// Split at headings.
    Markdown,
    /// Line windows preferring blank-line breaks.
    Code,
    /// Plain text; chunked in line windows like code.
    Text,
}

/// A language name such as `rust`, as shown in output and used by `--lang`.
/// Files of an unknown type are `text`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Language(&'static str);

impl Language {
    /// Language of a path: exact file names first (`Makefile`), then the extension
    /// (case-insensitive), else `text`.
    pub fn from_path(path: &RepoPath) -> Self {
        let file_name = path.file_name();
        if let Some((_, name)) = FILE_NAMES.iter().find(|(exact, _)| *exact == file_name) {
            return Self(name);
        }
        let extension = match file_name.rsplit_once('.') {
            Some((stem, extension)) if !stem.is_empty() => extension.to_ascii_lowercase(),
            _ => return Self(TEXT),
        };
        EXTENSIONS
            .iter()
            .find(|(known, _)| *known == extension)
            .map_or(Self(TEXT), |(_, name)| Self(name))
    }

    /// A known language by name (case-insensitive), for `--lang` and for reading
    /// stored values. `None` for names this policy does not produce.
    pub fn parse(name: &str) -> Option<Self> {
        let name = name.to_ascii_lowercase();
        std::iter::once(&TEXT)
            .chain(EXTENSIONS.iter().map(|(_, n)| n))
            .chain(FILE_NAMES.iter().map(|(_, n)| n))
            .find(|known| **known == name)
            .map(|known| Self(known))
    }

    pub fn name(&self) -> &'static str {
        self.0
    }

    pub fn document_kind(&self) -> DocumentKind {
        match self.0 {
            MARKDOWN => DocumentKind::Markdown,
            TEXT => DocumentKind::Text,
            _ => DocumentKind::Code,
        }
    }
}

impl fmt::Display for Language {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.0)
    }
}

#[cfg(test)]
mod tests {
    use super::{DocumentKind, Language};
    use crate::domain::repo_path::RepoPath;

    fn lang(path: &str) -> Language {
        Language::from_path(&RepoPath::parse(path).unwrap())
    }

    #[test]
    fn common_extensions_map_to_language_names() {
        for (path, name) in [
            ("src/lib.rs", "rust"),
            ("a.py", "python"),
            ("a.js", "javascript"),
            ("a.jsx", "javascript"),
            ("a.ts", "typescript"),
            ("a.tsx", "typescript"),
            ("main.go", "go"),
            ("A.java", "java"),
            ("a.cpp", "cpp"),
            ("a.hpp", "cpp"),
            ("a.cs", "csharp"),
            ("a.sh", "shell"),
            ("a.ps1", "powershell"),
            ("Cargo.toml", "toml"),
            ("ci.yml", "yaml"),
            ("ci.yaml", "yaml"),
        ] {
            assert_eq!(lang(path).name(), name, "{path}");
        }
    }

    #[test]
    fn extension_matching_ignores_case() {
        assert_eq!(lang("SRC/LIB.RS").name(), "rust");
    }

    #[test]
    fn exact_file_names_win_over_extensions() {
        assert_eq!(lang("build/Makefile").name(), "makefile");
        assert_eq!(lang("Dockerfile").name(), "dockerfile");
    }

    #[test]
    fn unknown_extensions_and_dotfiles_are_text() {
        assert_eq!(lang("a.zzz").name(), "text");
        assert_eq!(lang(".gitignore").name(), "text");
        assert_eq!(lang("LICENSE").name(), "text");
    }

    #[test]
    fn markdown_files_are_markdown_documents() {
        for path in ["README.md", "a.markdown", "docs/x.mdx"] {
            assert_eq!(lang(path).name(), "markdown", "{path}");
            assert_eq!(lang(path).document_kind(), DocumentKind::Markdown);
        }
    }

    #[test]
    fn source_languages_are_code_and_unknown_files_are_text() {
        assert_eq!(lang("a.rs").document_kind(), DocumentKind::Code);
        assert_eq!(lang("a.toml").document_kind(), DocumentKind::Code);
        assert_eq!(lang("notes.txt").document_kind(), DocumentKind::Text);
        assert_eq!(lang("a.zzz").document_kind(), DocumentKind::Text);
    }

    #[test]
    fn parse_finds_known_names_case_insensitively() {
        assert_eq!(Language::parse("Rust"), Some(lang("a.rs")));
        assert_eq!(Language::parse("text"), Some(lang("a.zzz")));
        assert_eq!(Language::parse("cobol"), None);
    }

    #[test]
    fn display_is_the_name() {
        assert_eq!(lang("a.rs").to_string(), "rust");
    }
}
