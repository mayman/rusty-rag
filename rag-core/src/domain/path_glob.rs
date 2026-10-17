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

/// Longest accepted pattern, in characters.
pub const MAX_GLOB_CHARS: usize = 256;
/// Most wildcards (`*`, `**`, `**/`) accepted in one pattern.
pub const MAX_GLOB_WILDCARDS: usize = 8;

/// Why a pattern was refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InvalidGlob {
    Empty,
    /// More than `MAX_GLOB_CHARS` characters.
    TooLong,
    /// More than `MAX_GLOB_WILDCARDS` wildcards.
    TooComplex,
}

impl PathGlob {
    pub fn parse(pattern: &str) -> Result<Self, InvalidGlob> {
        if pattern.is_empty() {
            return Err(InvalidGlob::Empty);
        }
        if pattern.chars().count() > MAX_GLOB_CHARS {
            return Err(InvalidGlob::TooLong);
        }
        let wildcards = tokenize(pattern)
            .iter()
            .filter(|token| matches!(token, Token::Star | Token::GlobStar | Token::GlobStarSlash))
            .count();
        if wildcards > MAX_GLOB_WILDCARDS {
            return Err(InvalidGlob::TooComplex);
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

/// Whether `tokens` match all of `text`. Tracks the set of text positions reachable
/// after each token, so the cost is tokens x characters whatever the pattern.
fn matches_from(tokens: &[Token], text: &[char]) -> bool {
    let n = text.len();
    let mut reach = vec![false; n + 1];
    reach[0] = true;
    for token in tokens {
        let mut next = vec![false; n + 1];
        match token {
            Token::Literal(c) => {
                for i in 0..n {
                    next[i + 1] = reach[i] && text[i] == *c;
                }
            }
            Token::AnyOne => {
                for i in 0..n {
                    next[i + 1] = reach[i] && text[i] != '/';
                }
            }
            Token::Star => {
                for j in 0..=n {
                    next[j] = reach[j] || (j > 0 && next[j - 1] && text[j - 1] != '/');
                }
            }
            Token::GlobStar => {
                for j in 0..=n {
                    next[j] = reach[j] || (j > 0 && next[j - 1]);
                }
            }
            Token::GlobStarSlash => {
                let mut seen = false;
                for j in 0..=n {
                    if j > 0 {
                        seen |= reach[j - 1];
                    }
                    next[j] = reach[j] || (j > 0 && text[j - 1] == '/' && seen);
                }
            }
        }
        reach = next;
    }
    reach[n]
}
#[cfg(test)]
mod tests {
    use super::{InvalidGlob, PathGlob, MAX_GLOB_CHARS, MAX_GLOB_WILDCARDS};
    use crate::domain::repo_path::RepoPath;

    fn matches(pattern: &str, path: &str) -> bool {
        PathGlob::parse(pattern)
            .unwrap()
            .matches(&RepoPath::parse(path).unwrap())
    }

    #[test]
    fn empty_pattern_is_rejected() {
        assert_eq!(PathGlob::parse(""), Err(InvalidGlob::Empty));
    }

    #[test]
    fn a_pattern_with_too_many_wildcards_is_rejected() {
        let pattern = "*a".repeat(MAX_GLOB_WILDCARDS + 1);
        assert_eq!(PathGlob::parse(&pattern), Err(InvalidGlob::TooComplex));
        let allowed = "*a".repeat(MAX_GLOB_WILDCARDS);
        assert!(PathGlob::parse(&allowed).is_ok());
    }

    #[test]
    fn a_pattern_longer_than_the_cap_is_rejected() {
        let pattern = "a".repeat(MAX_GLOB_CHARS + 1);
        assert_eq!(PathGlob::parse(&pattern), Err(InvalidGlob::TooLong));
        assert!(PathGlob::parse(&"a".repeat(MAX_GLOB_CHARS)).is_ok());
    }

    #[test]
    fn a_backtracking_bomb_is_answered_in_linear_time() {
        // Eight stars between literals over a long path: exponential for a naive
        // recursive matcher, instant for one that tracks reachable positions.
        let pattern = "*a".repeat(MAX_GLOB_WILDCARDS) + "b";
        let path = "a".repeat(900) + ".rs";
        assert!(!matches(&pattern, &path));
        let hit = "a".repeat(900) + "b";
        assert!(matches(&pattern, &hit));
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
