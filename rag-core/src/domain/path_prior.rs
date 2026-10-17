//! PathPrior: a small, explainable query-time boost for documentation and penalty
//! for tests and fixtures. An agent asking a conceptual question about a library
//! wants the docs and the library source before the tests that exercise them, yet
//! test files repeat the vocabulary of a feature far more often than prose does.
//!
//! The prior is a multiplier on the (higher-is-better) relevance score. It depends
//! on the path alone, is applied at query time only, and is not part of the index,
//! so changing it never requires a reindex.

use crate::domain::language::{DocumentKind, Language};
use crate::domain::repo_path::RepoPath;

/// Bumped whenever the multipliers or the path rules change. Query-time only: it is
/// deliberately not one of the `IndexVersions`, so no index goes stale.
pub const RANKING_VERSION: u32 = 1;

/// Multiplier for documentation: a prose file under a documentation directory, or a
/// top-level README or other top-level prose. Slight, so that a clearly better code
/// match still wins.
pub const DOCUMENTATION_BOOST: f64 = 1.15;

/// Multiplier for tests, fixtures, benchmarks, and specs.
pub const TEST_PENALTY: f64 = 0.7;

/// Multiplier for everything else, examples included.
pub const NEUTRAL: f64 = 1.0;

/// Directory names (any depth, compared in lowercase) that mark tests and fixtures.
const TEST_DIRECTORIES: &[&str] = &[
    "test",
    "tests",
    "testsuite",
    "__tests__",
    "spec",
    "specs",
    "fixtures",
    "testdata",
    "benches",
];

/// Directory names (any depth, compared in lowercase) that mark documentation.
const DOCUMENTATION_DIRECTORIES: &[&str] = &["docs", "doc", "documentation", "book", "guide"];

/// Top-level prose that is a history log, not documentation: it repeats every term
/// ever touched, so it is never boosted.
const HISTORY_FILE_STEMS: &[&str] = &["changelog", "changes", "history", "news", "releases"];

/// Prose extensions that count as documentation. `Language` knows markdown and
/// plain text; reStructuredText and AsciiDoc are unknown to it (they are `text`).
const PROSE_EXTENSIONS: &[&str] = &["md", "markdown", "mdx", "rst", "adoc", "asciidoc", "txt"];

/// The multiplier for a chunk at `path`: `TEST_PENALTY` for tests and fixtures
/// (which wins over documentation, so `tests/docs/x.md` is a test),
/// `DOCUMENTATION_BOOST` for documentation, else `NEUTRAL`.
pub fn path_prior(path: &RepoPath) -> f64 {
    if is_test_path(path) {
        TEST_PENALTY
    } else if is_documentation(path) {
        DOCUMENTATION_BOOST
    } else {
        NEUTRAL
    }
}

fn directories(path: &RepoPath) -> impl Iterator<Item = String> + '_ {
    let text = path.as_str();
    let directory_count = text.matches('/').count();
    text.split('/')
        .take(directory_count)
        .map(str::to_ascii_lowercase)
}

fn is_test_path(path: &RepoPath) -> bool {
    if directories(path).any(|d| TEST_DIRECTORIES.contains(&d.as_str())) {
        return true;
    }
    let name = path.file_name().to_ascii_lowercase();
    let Some((stem, extension)) = name.rsplit_once('.') else {
        return false;
    };
    match extension {
        "py" => stem.starts_with("test_") || stem.ends_with("_test"),
        "go" | "rs" => stem.ends_with("_test"),
        "ts" | "tsx" | "js" | "jsx" | "mjs" => stem.ends_with(".test") || stem.ends_with(".spec"),
        _ => false,
    }
}

fn is_documentation(path: &RepoPath) -> bool {
    let name = path.file_name().to_ascii_lowercase();
    let Some((stem, extension)) = name.rsplit_once('.') else {
        return false;
    };
    let prose = PROSE_EXTENSIONS.contains(&extension)
        || Language::from_path(path).document_kind() == DocumentKind::Markdown;
    if !prose {
        return false;
    }
    if directories(path).any(|d| DOCUMENTATION_DIRECTORIES.contains(&d.as_str())) {
        return true;
    }
    let top_level = !path.as_str().contains('/');
    top_level && !HISTORY_FILE_STEMS.contains(&stem)
}

#[cfg(test)]
mod tests {
    use super::{path_prior, DOCUMENTATION_BOOST, NEUTRAL, TEST_PENALTY};
    use crate::domain::repo_path::RepoPath;

    fn prior(path: &str) -> f64 {
        path_prior(&RepoPath::parse(path).unwrap())
    }

    #[test]
    fn documentation_is_boosted() {
        for path in [
            "doc/book/src/reference/build-scripts.md",
            "docs/guide/install.md",
            "docs/index.rst",
            "guide/intro.adoc",
            "Documentation/notes.txt",
            "website/book/ch01.md",
            "README.md",
            "CONTRIBUTING.md",
            "readme.rst",
        ] {
            assert_eq!(prior(path), DOCUMENTATION_BOOST, "{path}");
        }
    }

    #[test]
    fn tests_and_fixtures_are_penalised() {
        for path in [
            "tests/testsuite/build_script.rs",
            "src/tests/mod.rs",
            "crates/x/testsuite/a.rs",
            "web/__tests__/app.js",
            "spec/models/user_spec.rb",
            "testdata/golden.json",
            "pkg/fixtures/a.json",
            "benches/parse.rs",
            "test/index.js",
            "tests/test_billing.py",
            "billing/test_invoice.py",
            "invoice_test.py",
            "server/handler_test.go",
            "src/lib_test.rs",
            "src/app.test.ts",
            "src/app.spec.js",
            "src/App.test.tsx",
            "Docs/Tests/Mod.rs",
        ] {
            assert_eq!(prior(path), TEST_PENALTY, "{path}");
        }
    }

    #[test]
    fn a_test_directory_wins_over_a_documentation_one() {
        assert_eq!(prior("tests/docs/case.md"), TEST_PENALTY);
        assert_eq!(prior("docs/fixtures/example.md"), TEST_PENALTY);
    }

    #[test]
    fn everything_else_is_neutral() {
        for path in [
            "src/lib.rs",
            "src/testing.rs",
            "src/latest/mod.rs",
            "src/contest.py",
            "examples/basic.rs",
            "CHANGELOG.md",
            "HISTORY.rst",
            "src/notes.md",
            "src/billing/README.md",
            "docs/diagram.png",
            "docs/conf.py",
            "Cargo.toml",
            "LICENSE",
            "scripts/test.sh",
            "src/app.test.rs",
        ] {
            assert_eq!(prior(path), NEUTRAL, "{path}");
        }
    }

    const _: () = assert!(TEST_PENALTY < NEUTRAL && NEUTRAL < DOCUMENTATION_BOOST);
}
