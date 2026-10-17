//! RepoPath: a repository-relative file path that is safe by construction.
//!
//! There are two constructors. `RepoPath::parse` is for user input and normalises
//! `\` to `/`; `RepoPath::from_git` is for paths reported by git and rejects `\`
//! instead of reinterpreting it.
//!
//! Both reject: empty paths, paths over 1024 bytes, absolute and UNC paths, `.`,
//! `..` and empty components, control characters, invisible characters (Unicode
//! format characters Cf, line and paragraph separators Zl and Zp, tag characters,
//! and the default-ignorable joiners and fillers), and `.git` components (or the
//! NTFS short name `git~1`).
//!
//! Only `parse` also rejects what Windows cannot create, because a user-supplied
//! path may be matched against files on disk: drive letters (`C:`), `:` (NTFS
//! alternate streams such as `file.txt:stream`), components ending in `.` or a
//! space, and Windows device names (`CON`, `NUL.txt`, `COM1`). Git-reported paths
//! are never materialised (clones are `--no-checkout`), so `from_git` accepts them.
//! Adapters therefore never re-check paths.

use std::fmt;

/// Longest accepted path, in bytes.
pub const MAX_PATH_BYTES: usize = 1024;

/// A validated, `/`-separated path relative to a repository root.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct RepoPath(String);

/// Why a string is not a valid `RepoPath`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RepoPathError {
    Empty,
    TooLong,
    Absolute,
    DriveLetter,
    Unc,
    ParentTraversal,
    AlternateStream,
    InvalidComponent,
    /// `\` in a path that must already use `/` (`RepoPath::from_git`).
    Backslash,
    /// A component ends in `.` or a space (Windows cannot create it).
    TrailingDotOrSpace,
    /// A component is a Windows device name such as `CON` or `nul.txt`.
    ReservedName,
    /// A component is `.git` (any case) or its NTFS short name.
    GitComponent,
    /// An invisible character: format (Cf), line or paragraph separator, tag
    /// character, or default-ignorable joiner or filler.
    HiddenCharacter,
}

impl RepoPath {
    /// User input: normalises `\` to `/`, then validates.
    pub fn parse(text: &str) -> Result<Self, RepoPathError> {
        Self::validated(text.replace('\\', "/"), Rules::UserInput)
    }

    /// A path reported by git: must already use `/`, so `\` is rejected. Nothing is
    /// ever checked out, so the Windows materialisation rules (device names,
    /// trailing `.` or space, `:`) do not apply; traversal, empty components,
    /// invisible characters, `.git` components, and the length cap still do.
    pub fn from_git(text: &str) -> Result<Self, RepoPathError> {
        if text.contains('\\') {
            return Err(RepoPathError::Backslash);
        }
        Self::validated(text.to_owned(), Rules::Git)
    }

    fn validated(path: String, rules: Rules) -> Result<Self, RepoPathError> {
        if path.is_empty() {
            return Err(RepoPathError::Empty);
        }
        if path.len() > MAX_PATH_BYTES {
            return Err(RepoPathError::TooLong);
        }
        if path.starts_with("//") {
            return Err(RepoPathError::Unc);
        }
        if path.starts_with('/') {
            return Err(RepoPathError::Absolute);
        }
        let bytes = path.as_bytes();
        if rules == Rules::UserInput
            && bytes.len() >= 2
            && bytes[0].is_ascii_alphabetic()
            && bytes[1] == b':'
        {
            return Err(RepoPathError::DriveLetter);
        }
        for component in path.split('/') {
            check_component(component, rules)?;
        }
        Ok(Self(path))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// The last component.
    pub fn file_name(&self) -> &str {
        self.0.rsplit('/').next().unwrap_or(&self.0)
    }
}

/// Which rule set a constructor applies.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Rules {
    /// `RepoPath::parse`: everything, including what Windows cannot create.
    UserInput,
    /// `RepoPath::from_git`: no Windows materialisation rules.
    Git,
}

fn check_component(component: &str, rules: Rules) -> Result<(), RepoPathError> {
    let windows = rules == Rules::UserInput;
    if component == ".." {
        return Err(RepoPathError::ParentTraversal);
    }
    if windows && component.contains(':') {
        return Err(RepoPathError::AlternateStream);
    }
    if component.is_empty() || component == "." || component.chars().any(char::is_control) {
        return Err(RepoPathError::InvalidComponent);
    }
    if component.chars().any(is_hidden_character) {
        return Err(RepoPathError::HiddenCharacter);
    }
    if windows && component.ends_with(['.', ' ']) {
        return Err(RepoPathError::TrailingDotOrSpace);
    }
    if component.eq_ignore_ascii_case(".git") || component.eq_ignore_ascii_case("git~1") {
        return Err(RepoPathError::GitComponent);
    }
    if windows && is_device_name(component) {
        return Err(RepoPathError::ReservedName);
    }
    Ok(())
}

/// Characters that make a path look different from what it is: every Unicode
/// format character (general category Cf: bidi controls, zero-width characters,
/// the byte order mark, soft hyphen, tag characters, ...), the line and paragraph
/// separators (Zl, Zp), and the default-ignorable joiners and fillers.
fn is_hidden_character(c: char) -> bool {
    matches!(
        c,
        // Cf
        '\u{00ad}'
            | '\u{0600}'..='\u{0605}'
            | '\u{061c}'
            | '\u{06dd}'
            | '\u{070f}'
            | '\u{0890}'..='\u{0891}'
            | '\u{08e2}'
            | '\u{180e}'
            | '\u{200b}'..='\u{200f}'
            | '\u{202a}'..='\u{202e}'
            | '\u{2060}'..='\u{2064}'
            | '\u{2066}'..='\u{206f}'
            | '\u{feff}'
            | '\u{fff9}'..='\u{fffb}'
            | '\u{110bd}'
            | '\u{110cd}'
            | '\u{13430}'..='\u{1343f}'
            | '\u{1bca0}'..='\u{1bca3}'
            | '\u{1d173}'..='\u{1d17a}'
            // Zl, Zp
            | '\u{2028}'
            | '\u{2029}'
            // Default-ignorable joiners and fillers
            | '\u{034f}'
            | '\u{115f}'..='\u{1160}'
            | '\u{17b4}'..='\u{17b5}'
            | '\u{180b}'..='\u{180d}'
            | '\u{180f}'
            | '\u{3164}'
            | '\u{ffa0}'
            // Every tag character, assigned or not
            | '\u{e0000}'..='\u{e007f}'
    )
}

/// `CON`, `PRN`, `AUX`, `NUL`, `CONIN$`, `CONOUT$`, `COM0` to `COM9`, `LPT0` to
/// `LPT9`, with any extension, in any case.
fn is_device_name(component: &str) -> bool {
    let stem = component.split('.').next().unwrap_or(component).trim_end();
    let stem = stem.to_ascii_uppercase();
    let numbered = |prefix: &str| {
        stem.strip_prefix(prefix)
            .is_some_and(|rest| rest.len() == 1 && rest.as_bytes()[0].is_ascii_digit())
    };
    matches!(
        stem.as_str(),
        "CON" | "PRN" | "AUX" | "NUL" | "CONIN$" | "CONOUT$"
    ) || numbered("COM")
        || numbered("LPT")
}

impl fmt::Display for RepoPath {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl fmt::Display for RepoPathError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let text = match self {
            Self::Empty => "path is empty",
            Self::TooLong => "path is longer than 1024 bytes",
            Self::Absolute => "path must be relative to the repository root",
            Self::DriveLetter => "path must not start with a drive letter",
            Self::Unc => "path must not be a UNC path",
            Self::ParentTraversal => "path must not contain '..'",
            Self::AlternateStream => "path must not contain ':' (alternate data stream)",
            Self::InvalidComponent => "path has an empty, '.' or control-character component",
            Self::Backslash => "path must use '/' separators",
            Self::TrailingDotOrSpace => "path component must not end with '.' or a space",
            Self::ReservedName => "path component is a reserved Windows device name",
            Self::GitComponent => "path must not contain a '.git' component",
            Self::HiddenCharacter => "path contains an invisible or direction-changing character",
        };
        f.write_str(text)
    }
}

impl std::error::Error for RepoPathError {}

#[cfg(test)]
mod tests {
    use super::{RepoPath, RepoPathError};

    fn rejected(text: &str) -> RepoPathError {
        RepoPath::parse(text).expect_err(text)
    }

    #[test]
    fn accepts_a_plain_relative_path() {
        assert_eq!(RepoPath::parse("src/x.rs").unwrap().as_str(), "src/x.rs");
    }

    #[test]
    fn normalises_backslashes_to_forward_slashes() {
        assert_eq!(
            RepoPath::parse("src\\a\\b.rs").unwrap().as_str(),
            "src/a/b.rs"
        );
    }

    #[test]
    fn rejects_the_empty_path() {
        assert_eq!(rejected(""), RepoPathError::Empty);
    }

    #[test]
    fn rejects_parent_traversal_in_any_position() {
        for text in ["..", "../x", "a/../b", "a/..", "a\\..\\b"] {
            assert_eq!(rejected(text), RepoPathError::ParentTraversal, "{text}");
        }
    }

    #[test]
    fn rejects_absolute_paths() {
        assert_eq!(rejected("/etc/passwd"), RepoPathError::Absolute);
        assert_eq!(rejected("\\windows"), RepoPathError::Absolute);
    }

    #[test]
    fn rejects_drive_letters() {
        assert_eq!(rejected("C:/x"), RepoPathError::DriveLetter);
        assert_eq!(rejected("c:\\x"), RepoPathError::DriveLetter);
        assert_eq!(rejected("C:x"), RepoPathError::DriveLetter);
    }

    #[test]
    fn rejects_unc_paths() {
        assert_eq!(rejected("\\\\server\\share\\x"), RepoPathError::Unc);
        assert_eq!(rejected("//server/share"), RepoPathError::Unc);
    }

    #[test]
    fn rejects_alternate_data_streams() {
        for text in ["file.txt:hidden", "a/b.txt::$DATA", "dir:x/file"] {
            assert_eq!(rejected(text), RepoPathError::AlternateStream, "{text}");
        }
    }

    #[test]
    fn rejects_empty_dot_and_control_components() {
        for text in ["a//b", "a/", "./a", "a/./b", "a\u{0}b", "a\nb"] {
            assert_eq!(rejected(text), RepoPathError::InvalidComponent, "{text:?}");
        }
    }

    #[test]
    fn dotfiles_and_double_dot_names_inside_a_component_are_fine() {
        assert!(RepoPath::parse(".github/workflows/ci.yml").is_ok());
        assert!(RepoPath::parse("a/..b/c..d").is_ok());
    }

    #[test]
    fn file_name_is_the_last_component() {
        assert_eq!(RepoPath::parse("a/b/c.rs").unwrap().file_name(), "c.rs");
        assert_eq!(RepoPath::parse("c.rs").unwrap().file_name(), "c.rs");
    }

    /// Both constructors apply the safety rules; only `parse` normalises `\`.
    fn both(text: &str) -> [Result<RepoPath, RepoPathError>; 2] {
        [RepoPath::parse(text), RepoPath::from_git(text)]
    }

    #[test]
    fn from_git_accepts_what_git_reports() {
        assert_eq!(RepoPath::from_git("src/x.rs").unwrap().as_str(), "src/x.rs");
    }

    #[test]
    fn from_git_rejects_backslashes_instead_of_normalising_them() {
        assert_eq!(
            RepoPath::from_git("src\\x.rs"),
            Err(RepoPathError::Backslash)
        );
        assert_eq!(
            RepoPath::from_git("a\\..\\b"),
            Err(RepoPathError::Backslash)
        );
    }

    #[test]
    fn from_git_applies_the_same_rules_as_parse() {
        for text in ["", "/x", "//x", "../x", "a//b", "a\u{0}b", "a/", "a/./b"] {
            assert_eq!(
                RepoPath::from_git(text).unwrap_err(),
                RepoPath::parse(text).unwrap_err(),
                "{text:?}"
            );
        }
    }

    #[test]
    fn rejects_components_ending_in_a_dot_or_space() {
        for text in ["a.", "dir /x", "x/y.", "a/b ", "a./b", "...", "a/b. "] {
            assert_eq!(
                rejected(text),
                RepoPathError::TrailingDotOrSpace,
                "{text:?}"
            );
        }
    }

    #[test]
    fn rejects_windows_device_names_with_or_without_an_extension() {
        for text in [
            "CON",
            "con",
            "Aux.txt",
            "nul.tar.gz",
            "COM1",
            "lpt9.rs",
            "dir/NUL",
            "src/prn.c",
            "CONIN$",
            "com0",
        ] {
            assert_eq!(rejected(text), RepoPathError::ReservedName, "{text}");
        }
    }

    #[test]
    fn from_git_drops_the_windows_materialisation_rules_because_nothing_is_checked_out() {
        for text in [
            "a.",
            "dir /x",
            "x/y. ",
            "CON",
            "aux.txt",
            "dir/NUL",
            "file.txt:stream",
            "a/b.txt::$DATA",
            "dir:x/file",
            "C:/x",
            "c:x",
        ] {
            assert!(RepoPath::from_git(text).is_ok(), "{text}");
        }
    }

    #[test]
    fn from_git_keeps_traversal_empty_components_and_the_length_cap() {
        assert_eq!(
            RepoPath::from_git("a/../b"),
            Err(RepoPathError::ParentTraversal)
        );
        assert_eq!(
            RepoPath::from_git("a//b"),
            Err(RepoPathError::InvalidComponent)
        );
        assert_eq!(
            RepoPath::from_git("a/./b"),
            Err(RepoPathError::InvalidComponent)
        );
        assert_eq!(
            RepoPath::from_git(&"a".repeat(1025)),
            Err(RepoPathError::TooLong)
        );
    }

    #[test]
    fn names_that_only_resemble_device_names_are_fine() {
        for text in [
            "console.rs",
            "com10.txt",
            "conf",
            "auxiliary/x",
            "nullable.rs",
            "lpt.rs",
        ] {
            for result in both(text) {
                assert!(result.is_ok(), "{text}");
            }
        }
    }

    #[test]
    fn rejects_any_dot_git_component_in_any_case() {
        for text in [
            ".git/config",
            "a/.GIT/x",
            "a/.git",
            ".Git",
            "git~1/x",
            "a/GIT~1",
        ] {
            for result in both(text) {
                assert_eq!(result, Err(RepoPathError::GitComponent), "{text}");
            }
        }
    }

    #[test]
    fn git_lookalikes_are_fine() {
        for text in [
            ".github/x",
            ".gitignore",
            ".gitattributes",
            "x.git/y",
            "a/gitx",
        ] {
            for result in both(text) {
                assert!(result.is_ok(), "{text}");
            }
        }
    }

    #[test]
    fn rejects_bidi_zero_width_and_line_separator_characters() {
        for ch in [
            '\u{202e}', '\u{202a}', '\u{2066}', '\u{2069}', '\u{200b}', '\u{200d}', '\u{200e}',
            '\u{2060}', '\u{feff}', '\u{2028}', '\u{2029}', '\u{061c}',
        ] {
            let text = format!("src/a{ch}b.rs");
            for result in both(&text) {
                assert_eq!(result, Err(RepoPathError::HiddenCharacter), "{ch:?}");
            }
        }
    }

    #[test]
    fn rejects_every_format_separator_tag_and_ignorable_joiner_character() {
        for ch in [
            '\u{00ad}',  // soft hyphen (Cf)
            '\u{0600}',  // Arabic number sign (Cf)
            '\u{06dd}',  // Arabic end of ayah (Cf)
            '\u{070f}',  // Syriac abbreviation mark (Cf)
            '\u{200c}',  // zero width non-joiner
            '\u{206a}',  // deprecated format characters
            '\u{206f}',  //
            '\u{fff9}',  // interlinear annotation anchor (Cf)
            '\u{110bd}', // Kaithi number sign (Cf, supplementary plane)
            '\u{e0001}', // language tag
            '\u{e0041}', // tag latin capital letter a
            '\u{e007f}', // cancel tag
            '\u{2028}',  // line separator (Zl)
            '\u{2029}',  // paragraph separator (Zp)
            '\u{034f}',  // combining grapheme joiner
            '\u{115f}',  // Hangul choseong filler
            '\u{3164}',  // Hangul filler
            '\u{ffa0}',  // halfwidth Hangul filler
            '\u{17b4}',  // Khmer inherent vowel
        ] {
            let text = format!("src/a{ch}b.rs");
            for result in both(&text) {
                assert_eq!(result, Err(RepoPathError::HiddenCharacter), "{ch:?}");
            }
        }
    }

    #[test]
    fn ordinary_non_ascii_names_are_fine() {
        assert!(RepoPath::parse("docs/caf\u{e9}/\u{65e5}\u{672c}\u{8a9e}.md").is_ok());
        for text in [
            "docs/e\u{301}.md",         // combining acute accent
            "docs/\u{2764}\u{fe0f}.md", // emoji with a variation selector
            "docs/\u{1f600}.md",
            "docs/\u{5e9}\u{5dc}\u{5d5}\u{5dd}.md",
        ] {
            for result in both(text) {
                assert!(result.is_ok(), "{text:?}");
            }
        }
    }

    #[test]
    fn rejects_paths_over_1024_bytes() {
        let longest = "a".repeat(1024);
        for result in both(&longest) {
            assert!(result.is_ok());
        }
        for result in both(&"a".repeat(1025)) {
            assert_eq!(result, Err(RepoPathError::TooLong));
        }
        let two_byte_chars = "\u{e9}".repeat(513);
        for result in both(&two_byte_chars) {
            assert_eq!(result, Err(RepoPathError::TooLong), "length is in bytes");
        }
    }

    #[test]
    fn paths_order_lexicographically() {
        let a = RepoPath::parse("a/b").unwrap();
        let b = RepoPath::parse("a/c").unwrap();
        assert!(a < b);
    }
}
