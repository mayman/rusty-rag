//! The fixed synthetic corpus of the relevance suite: two repositories (`widgets`,
//! `tools`) whose files live in `fixtures/`, indexed the way the real pipeline does
//! it. Each file goes through `IndexPolicy` (skip rules, by path and by content)
//! and then `Chunker`, and the survivors are written with `commit_index` into a real
//! SQLite store in a temp directory.

use std::path::Path;

use rag_core::application::ports::{ChunkStore, IndexChanges, IndexedFile, SourceRepository};
use rag_core::domain::chunker::Chunker;
use rag_core::domain::git_location::GitLocation;
use rag_core::domain::index_policy::{IndexPolicy, SkipReason};
use rag_core::domain::indexed_at::IndexedAt;
use rag_core::domain::oid::{BlobOid, CommitId};
use rag_core::domain::repo_path::RepoPath;
use rag_core::domain::source::Source;
use rag_core::domain::source_id::SourceId;
use rag_core::domain::tree_entry::TreeEntry;
use rag_infra::sqlite::SqliteStore;
use tempfile::TempDir;

/// Source slugs of the two repositories.
pub const WIDGETS_SLUG: &str = "github.com/acme/widgets";
pub const TOOLS_SLUG: &str = "github.com/acme/tools";
/// When the corpus was indexed, and the "now" the searches run at (two minutes later).
pub const INDEXED_AT: i64 = 1_700_000_000;
pub const NOW: i64 = 1_700_000_120;

const WIDGETS_COMMIT: &str = "0123456789abcdef0123456789abcdef01234567";
const TOOLS_COMMIT: &str = "fedcba9876543210fedcba9876543210fedcba98";

/// (source name, clone URL, indexed commit)
const REPOS: &[(&str, &str, &str)] = &[
    ("widgets", "https://github.com/acme/widgets", WIDGETS_COMMIT),
    ("tools", "https://github.com/acme/tools", TOOLS_COMMIT),
];

/// One file of the corpus: its repository name, its repo-relative path, and its text.
pub struct Doc {
    pub repo: &'static str,
    pub path: &'static str,
    pub text: &'static str,
}

pub const DOCS: &[Doc] = &[
    Doc {
        repo: "widgets",
        path: "src/config/loader.rs",
        text: include_str!("fixtures/widgets/src/config/loader.rs"),
    },
    Doc {
        repo: "widgets",
        path: "src/limits/fanout.rs",
        text: include_str!("fixtures/widgets/src/limits/fanout.rs"),
    },
    Doc {
        repo: "widgets",
        path: "src/net/http_server.rs",
        text: include_str!("fixtures/widgets/src/net/http_server.rs"),
    },
    Doc {
        repo: "widgets",
        path: "src/util/retry.rs",
        text: include_str!("fixtures/widgets/src/util/retry.rs"),
    },
    Doc {
        repo: "widgets",
        path: "src/auth/token_refresh.py",
        text: include_str!("fixtures/widgets/src/auth/token_refresh.py"),
    },
    Doc {
        repo: "widgets",
        path: "src/web/session.ts",
        text: include_str!("fixtures/widgets/src/web/session.ts"),
    },
    Doc {
        repo: "widgets",
        path: "src/web/router.ts",
        text: include_str!("fixtures/widgets/src/web/router.ts"),
    },
    Doc {
        repo: "widgets",
        path: "src/billing/invoice.py",
        text: include_str!("fixtures/widgets/src/billing/invoice.py"),
    },
    Doc {
        repo: "widgets",
        path: "tests/test_billing.py",
        text: include_str!("fixtures/widgets/tests/test_billing.py"),
    },
    Doc {
        repo: "widgets",
        path: "docs/guide/installation.md",
        text: include_str!("fixtures/widgets/docs/guide/installation.md"),
    },
    Doc {
        repo: "widgets",
        path: "docs/guide/configuration.md",
        text: include_str!("fixtures/widgets/docs/guide/configuration.md"),
    },
    Doc {
        repo: "widgets",
        path: "docs/guide/usage.md",
        text: include_str!("fixtures/widgets/docs/guide/usage.md"),
    },
    Doc {
        repo: "widgets",
        path: "docs/api/errors.md",
        text: include_str!("fixtures/widgets/docs/api/errors.md"),
    },
    Doc {
        repo: "widgets",
        path: "CHANGELOG.md",
        text: include_str!("fixtures/widgets/CHANGELOG.md"),
    },
    Doc {
        repo: "widgets",
        path: "README.md",
        text: include_str!("fixtures/widgets/README.md"),
    },
    Doc {
        repo: "widgets",
        path: "Cargo.toml",
        text: include_str!("fixtures/widgets/Cargo.toml"),
    },
    Doc {
        repo: "widgets",
        path: "vendor/fastjson/parser.js",
        text: include_str!("fixtures/widgets/vendor/fastjson/parser.js"),
    },
    Doc {
        repo: "widgets",
        path: "dist/app.bundle.min.js",
        text: include_str!("fixtures/widgets/dist/app.bundle.min.js"),
    },
    Doc {
        repo: "widgets",
        path: "src/gen/api.pb.go",
        text: include_str!("fixtures/widgets/src/gen/api.pb.go"),
    },
    Doc {
        repo: "widgets",
        path: "Cargo.lock",
        text: include_str!("fixtures/widgets/Cargo.lock"),
    },
    Doc {
        repo: "widgets",
        path: "package-lock.json",
        text: include_str!("fixtures/widgets/package-lock.json"),
    },
    Doc {
        repo: "widgets",
        path: "docs/guide/leases.md",
        text: include_str!("fixtures/widgets/docs/guide/leases.md"),
    },
    Doc {
        repo: "widgets",
        path: "tests/test_leases.py",
        text: include_str!("fixtures/widgets/tests/test_leases.py"),
    },
    Doc {
        repo: "widgets",
        path: "docs/api/circuit_breaker.md",
        text: include_str!("fixtures/widgets/docs/api/circuit_breaker.md"),
    },
    Doc {
        repo: "widgets",
        path: "src/net/circuit.spec.ts",
        text: include_str!("fixtures/widgets/src/net/circuit.spec.ts"),
    },
    Doc {
        repo: "widgets",
        path: "docs/guide/shutdown.md",
        text: include_str!("fixtures/widgets/docs/guide/shutdown.md"),
    },
    Doc {
        repo: "widgets",
        path: "src/net/shutdown_test.go",
        text: include_str!("fixtures/widgets/src/net/shutdown_test.go"),
    },
    Doc {
        repo: "widgets",
        path: "docs/api/pagination.md",
        text: include_str!("fixtures/widgets/docs/api/pagination.md"),
    },
    Doc {
        repo: "widgets",
        path: "tests/test_pagination.py",
        text: include_str!("fixtures/widgets/tests/test_pagination.py"),
    },
    Doc {
        repo: "tools",
        path: "src/config/loader.rs",
        text: include_str!("fixtures/tools/src/config/loader.rs"),
    },
    Doc {
        repo: "tools",
        path: "src/net/http_client.rs",
        text: include_str!("fixtures/tools/src/net/http_client.rs"),
    },
    Doc {
        repo: "tools",
        path: "src/cli/args.rs",
        text: include_str!("fixtures/tools/src/cli/args.rs"),
    },
    Doc {
        repo: "tools",
        path: "docs/release.md",
        text: include_str!("fixtures/tools/docs/release.md"),
    },
    Doc {
        repo: "tools",
        path: "scripts/bump_version.py",
        text: include_str!("fixtures/tools/scripts/bump_version.py"),
    },
];

/// A corpus file that `IndexPolicy` refused, and why.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Skipped {
    pub repo: &'static str,
    pub path: &'static str,
    pub reason: SkipReason,
}

/// The indexed corpus: the temp directory that holds the database, the store reopened
/// read-only, and the files the policy refused.
pub struct Corpus {
    pub dir: TempDir,
    pub store: SqliteStore,
    pub skipped: Vec<Skipped>,
}

/// Indexes every `DOCS` file the policy accepts into a fresh store.
///
/// # Panics
/// On any setup failure: this is test support.
pub fn build() -> Corpus {
    let dir = tempfile::tempdir().expect("temp dir");
    let writer = SqliteStore::open_for_write(dir.path()).expect("open for write");
    let mut skipped = Vec::new();
    for (name, url, commit) in REPOS {
        index_repo(&writer, &mut skipped, name, url, commit);
    }
    drop(writer);
    let store = SqliteStore::open_for_read(dir.path()).expect("open for read");
    Corpus {
        dir,
        store,
        skipped,
    }
}

fn index_repo(
    writer: &SqliteStore,
    skipped: &mut Vec<Skipped>,
    name: &str,
    url: &str,
    commit: &str,
) {
    let location = GitLocation::parse(url).expect("repo url");
    let id = SourceId::from_location(&location).expect("source id");
    let mut source = Source::register(id.clone(), name, location);
    writer.save(&source).expect("save the source");
    source.mark_ready(
        CommitId::parse(commit).expect("commit"),
        IndexedAt::from_epoch_seconds(INDEXED_AT),
    );
    let mut upserts = Vec::new();
    for (index, doc) in DOCS.iter().enumerate().filter(|(_, d)| d.repo == name) {
        let path = RepoPath::parse(doc.path).expect("corpus path");
        let entry = TreeEntry {
            path: path.clone(),
            blob: BlobOid::parse(&format!("{:040x}", index + 1)).expect("blob"),
            size: doc.text.len() as u64,
        };
        let verdict = IndexPolicy::check_entry(&entry)
            .and_then(|()| IndexPolicy::check_content(&path, doc.text.as_bytes()));
        match verdict {
            Err(reason) => skipped.push(Skipped {
                repo: doc.repo,
                path: doc.path,
                reason,
            }),
            Ok(()) => upserts.push(IndexedFile {
                chunks: Chunker::chunk(&id, &path, doc.text),
                blob: entry.blob,
                size: entry.size,
                path,
            }),
        }
    }
    writer
        .commit_index(
            &source,
            None,
            &IndexChanges {
                upserts,
                removals: Vec::new(),
            },
        )
        .expect("commit the corpus");
}

/// The text of the corpus file `path` in repository `repo`.
pub fn text_of(repo: &str, path: &str) -> &'static str {
    DOCS.iter()
        .find(|d| d.repo == repo && d.path == path)
        .map(|d| d.text)
        .expect("a corpus file")
}

/// The source id of a repository by name.
pub fn source_id(repo: &str) -> SourceId {
    let url = REPOS
        .iter()
        .find(|(name, _, _)| *name == repo)
        .map(|(_, url, _)| *url)
        .expect("a corpus repo");
    SourceId::from_location(&GitLocation::parse(url).expect("url")).expect("source id")
}

/// The slug of a repository by name.
pub fn slug_of(repo: &str) -> &'static str {
    match repo {
        "widgets" => WIDGETS_SLUG,
        "tools" => TOOLS_SLUG,
        other => panic!("no corpus repo named {other}"),
    }
}

/// The corpus database file, for tests that age it the way an older build would.
pub fn database_in(dir: &Path) -> std::path::PathBuf {
    dir.join("index.db")
}
