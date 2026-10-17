//! PathGlob: the `--path GLOB` filter, matched against repository paths.
//!
//! `?` matches one character except `/`, `*` any run except `/`, `**` any run
//! including `/`, and `**/` zero or more whole directories. A pattern without `/`
//! matches the file name at any depth (`*.rs` finds every Rust file).

use crate::domain::repo_path::RepoPath;

/// A validated glob pattern.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PathGlob {
    pattern: String,
}

/// The pattern was empty.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EmptyGlob;

impl PathGlob {
    pub fn parse(pattern: &str) -> Result<Self, EmptyGlob> {
        if pattern.is_empty() {
            return Err(EmptyGlob);
        }
        Ok(Self {
            pattern: pattern.replace('\\', "/"),
        })
    }

    pub fn as_str(&self) -> &str {
        &self.pattern
    }

    pub fn matches(&self, path: &RepoPath) -> bool {
        let tokens = tokenize(&self.pattern);
        let subject = if self.pattern.contains('/') {
            path.as_str()
        } else {
            path.file_name()
        };
        let chars: Vec<char> = subject.chars().collect();
        matches_from(&tokens, &chars)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Token {
    Literal(char),
    AnyOne,
    Star,
    GlobStar,
    GlobStarSlash,
}

fn tokenize(pattern: &str) -> Vec<Token> {
    let chars: Vec<char> = pattern.chars().collect();
    let mut tokens = Vec::new();
    let mut i = 0;
    while i < chars.len() {
        match chars[i] {
            '*' if chars.get(i + 1) == Some(&'*') => {
                if chars.get(i + 2) == Some(&'/') {
                    tokens.push(Token::GlobStarSlash);
                    i += 3;
                } else {
                    tokens.push(Token::GlobStar);
                    i += 2;
                }
            }
            '*' => {
                tokens.push(Token::Star);
                i += 1;
            }
            '?' => {
                tokens.push(Token::AnyOne);
                i += 1;
            }
            c => {
                tokens.push(Token::Literal(c));
                i += 1;
            }
        }
    }
    tokens
}

fn matches_from(tokens: &[Token], text: &[char]) -> bool {
    let Some((token, rest)) = tokens.split_first() else {
        return text.is_empty();
    };
    match token {
        Token::Literal(c) => text.first() == Some(c) && matches_from(rest, &text[1..]),
        Token::AnyOne => text.first().is_some_and(|c| *c != '/') && matches_from(rest, &text[1..]),
        Token::Star => {
            let run = text.iter().take_while(|c| **c != '/').count();
            (0..=run).any(|n| matches_from(rest, &text[n..]))
        }
        Token::GlobStar => (0..=text.len()).any(|n| matches_from(rest, &text[n..])),
        Token::GlobStarSlash => {
            matches_from(rest, text)
                || text
                    .iter()
                    .enumerate()
                    .filter(|(_, c)| **c == '/')
                    .any(|(i, _)| matches_from(rest, &text[i + 1..]))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{EmptyGlob, PathGlob};
    use crate::domain::repo_path::RepoPath;

    fn matches(pattern: &str, path: &str) -> bool {
        PathGlob::parse(pattern)
            .unwrap()
            .matches(&RepoPath::parse(path).unwrap())
    }

    #[test]
    fn empty_pattern_is_rejected() {
        assert_eq!(PathGlob::parse(""), Err(EmptyGlob));
    }

    #[test]
    fn pattern_without_slash_matches_file_names_at_any_depth() {
        assert!(matches("*.rs", "lib.rs"));
        assert!(matches("*.rs", "src/deep/lib.rs"));
        assert!(!matches("*.rs", "src/lib.rs.bak"));
    }

    #[test]
    fn star_does_not_cross_directories() {
        assert!(matches("src/*.rs", "src/lib.rs"));
        assert!(!matches("src/*.rs", "src/a/lib.rs"));
    }

    #[test]
    fn double_star_crosses_directories() {
        assert!(matches("src/**", "src/a/b/c.rs"));
        assert!(matches("src/**/*.rs", "src/a/b/c.rs"));
    }

    #[test]
    fn double_star_slash_matches_zero_directories() {
        assert!(matches("src/**/*.rs", "src/lib.rs"));
        assert!(matches("**/lib.rs", "lib.rs"));
    }

    #[test]
    fn question_mark_matches_one_character_but_not_a_slash() {
        assert!(matches("src/?.rs", "src/a.rs"));
        assert!(!matches("src/?.rs", "src/ab.rs"));
        assert!(!matches("a?b", "a/b"));
    }

    #[test]
    fn literal_patterns_match_exactly() {
        assert!(matches("docs/guide.md", "docs/guide.md"));
        assert!(!matches("docs/guide.md", "x/docs/guide.md"));
    }
}
