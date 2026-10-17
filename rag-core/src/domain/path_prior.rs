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

/// Multiplier for documentation: a prose file under a documentation directory, or a
/// top-level README-like file (`TOP_LEVEL_DOCUMENTATION_STEMS`). Slight, so that a clearly better code
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
    "fixtures",
    "testdata",
    "benches",
];

/// Directory names (any depth, compared in lowercase) that mark documentation.
const DOCUMENTATION_DIRECTORIES: &[&str] = &[
    "docs",
    "doc",
    "documentation",
    "book",
    "guide",
    "guides",
    "manual",
    "wiki",
];

/// Directory names that mark tests only for code files: `spec/` holds RSpec and
/// Jest tests, but `specs/design.md` is a prose specification.
const SPEC_DIRECTORIES: &[&str] = &["spec", "specs"];

/// Extensions of source code, for the directories that are test directories only
/// for code.
const CODE_EXTENSIONS: &[&str] = &[
    "rb", "js", "jsx", "mjs", "cjs", "ts", "tsx", "py", "rs", "go", "java", "kt", "kts", "scala",
    "php", "cs", "ex", "exs", "swift", "c", "cc", "cpp", "h", "hpp", "lua", "clj", "hs", "ml",
    "dart", "sh",
];

/// Top-level files (any prose extension) that are documentation, matched on the part
/// of the name before the first dot (`README.zh-CN.md` is `readme`). Other top-level
/// prose and text files (`CMakeLists.txt`, `requirements.txt`) are not.
const TOP_LEVEL_DOCUMENTATION_STEMS: &[&str] = &[
    "readme",
    "guide",
    "usage",
    "install",
    "installation",
    "faq",
    "tutorial",
    "overview",
    "architecture",
    "design",
];

/// Prose extensions that count as documentation. `Language` knows markdown; the
/// others are unknown to it. Plain `txt` is deliberately not here: it is
/// documentation only for README-like names (`PLAIN_TEXT_EXTENSION`).
const PROSE_EXTENSIONS: &[&str] = &["md", "markdown", "mdx", "rst", "adoc", "asciidoc"];

const PLAIN_TEXT_EXTENSION: &str = "txt";

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
    if CODE_EXTENSIONS.contains(&extension)
        && directories(path).any(|d| SPEC_DIRECTORIES.contains(&d.as_str()))
    {
        return true;
    }
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
    let readme_like =
        TOP_LEVEL_DOCUMENTATION_STEMS.contains(&stem.split('.').next().unwrap_or(stem));
    let prose = PROSE_EXTENSIONS.contains(&extension)
        || Language::from_path(path).document_kind() == DocumentKind::Markdown
        || (extension == PLAIN_TEXT_EXTENSION && readme_like);
    if !prose {
        return false;
    }
    if directories(path).any(|d| DOCUMENTATION_DIRECTORIES.contains(&d.as_str())) {
        return true;
    }
    let top_level = !path.as_str().contains('/');
    top_level && readme_like
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
            "Documentation/notes.md",
            "docs/README.txt",
            "docs/manual/setup.rst",
            "guides/start.md",
            "wiki/Home.md",
            "manual/ch1.adoc",
            "website/book/ch01.md",
            "README.md",
            "README.zh-CN.md",
            "readme.rst",
            "README.txt",
            "INSTALL.md",
            "Architecture.adoc",
            "FAQ.md",
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
            "spec/models/user_spec.rb",
            "specs/helpers.js",
            "spec/support/shared.py",
            "app/spec/rails_helper.rb",
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
            "CMakeLists.txt",
            "requirements.txt",
            "robots.txt",
            "docs/requirements.txt",
            "docs/robots.txt",
            "CONTRIBUTING.md",
            "SECURITY.md",
            "spec/support/helper.json",
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

    #[test]
    fn prose_inside_a_spec_directory_is_not_a_test() {
        assert_eq!(prior("spec/README.md"), NEUTRAL);
        assert_eq!(prior("specs/design.md"), NEUTRAL);
        assert_eq!(prior("docs/specs/api.md"), DOCUMENTATION_BOOST);
        assert_eq!(prior("spec/notes.txt"), NEUTRAL);
    }

    #[test]
    fn the_multipliers_are_not_neutral_so_the_prior_cannot_be_silently_removed() {
        const { assert!(DOCUMENTATION_BOOST >= 1.05) };
        const { assert!(TEST_PENALTY <= 0.9) };
        const { assert!(TEST_PENALTY > 0.0) };
        assert_ne!(DOCUMENTATION_BOOST, NEUTRAL);
        assert_ne!(TEST_PENALTY, NEUTRAL);
    }

    const _: () = assert!(TEST_PENALTY < NEUTRAL && NEUTRAL < DOCUMENTATION_BOOST);
}
