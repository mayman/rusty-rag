//! GitRef: a branch, tag, or ref name that is safe to hand to `git`.
//!
//! Validated at construction so adapters never re-check: non-empty, no leading
//! `-` (it would be read as an option), no whitespace, no `..`, no control
//! characters, and none of the characters git forbids in ref names or reads as
//! refspec syntax (`: ~ ^ ? * [ \` and `@{`).

use std::fmt;

/// A validated ref name.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct GitRef(String);

/// Why a string is not a valid `GitRef`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GitRefError {
    Empty,
    LeadingDash,
    Whitespace,
    DoubleDot,
    ControlCharacter,
    ForbiddenCharacter,
}

impl GitRef {
    pub fn parse(text: &str) -> Result<Self, GitRefError> {
        if text.is_empty() {
            return Err(GitRefError::Empty);
        }
        if text.starts_with('-') {
            return Err(GitRefError::LeadingDash);
        }
        if text.chars().any(char::is_whitespace) {
            return Err(GitRefError::Whitespace);
        }
        if text.chars().any(char::is_control) {
            return Err(GitRefError::ControlCharacter);
        }
        if text.contains("..") {
            return Err(GitRefError::DoubleDot);
        }
        let forbidden = |c: char| matches!(c, ':' | '~' | '^' | '?' | '*' | '[' | '\\');
        if text.chars().any(forbidden) || text.contains("@{") {
            return Err(GitRefError::ForbiddenCharacter);
        }
        Ok(Self(text.to_owned()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for GitRef {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl fmt::Display for GitRefError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let text = match self {
            Self::Empty => "ref name is empty",
            Self::LeadingDash => "ref name must not start with '-'",
            Self::Whitespace => "ref name must not contain whitespace",
            Self::DoubleDot => "ref name must not contain '..'",
            Self::ControlCharacter => "ref name must not contain control characters",
            Self::ForbiddenCharacter => "ref name contains a character git does not allow",
        };
        f.write_str(text)
    }
}

impl std::error::Error for GitRefError {}

#[cfg(test)]
mod tests {
    use super::{GitRef, GitRefError};

    #[test]
    fn accepts_ordinary_branch_tag_and_ref_names() {
        for text in [
            "main",
            "release/1.2",
            "feature/x_y",
            "v1.0.0",
            "refs/heads/main",
            "HEAD",
        ] {
            assert_eq!(GitRef::parse(text).expect(text).as_str(), text);
        }
    }

    #[test]
    fn rejects_empty() {
        assert_eq!(GitRef::parse(""), Err(GitRefError::Empty));
    }

    #[test]
    fn rejects_a_leading_dash_so_it_cannot_become_an_option() {
        for text in ["-x", "--upload-pack=evil", "-"] {
            assert_eq!(GitRef::parse(text), Err(GitRefError::LeadingDash), "{text}");
        }
    }

    #[test]
    fn rejects_whitespace() {
        for text in ["a b", "a\tb", " main", "main "] {
            assert_eq!(
                GitRef::parse(text),
                Err(GitRefError::Whitespace),
                "{text:?}"
            );
        }
    }

    #[test]
    fn rejects_double_dots() {
        assert_eq!(GitRef::parse("a..b"), Err(GitRefError::DoubleDot));
    }

    #[test]
    fn rejects_control_characters() {
        for text in ["a\u{7}b", "a\u{0}b", "a\u{7f}b"] {
            assert_eq!(
                GitRef::parse(text),
                Err(GitRefError::ControlCharacter),
                "{text:?}"
            );
        }
    }

    #[test]
    fn rejects_characters_git_refuses_or_reads_as_a_refspec() {
        for text in ["a:b", "a~1", "a^", "a?", "a*", "a[b", "a\\b", "a@{u}"] {
            assert_eq!(
                GitRef::parse(text),
                Err(GitRefError::ForbiddenCharacter),
                "{text}"
            );
        }
    }

    #[test]
    fn displays_as_the_ref_text() {
        assert_eq!(GitRef::parse("main").unwrap().to_string(), "main");
    }
}
