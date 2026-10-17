//! Shared helpers for the git adapter tests: throwaway upstream repositories built
//! with the real `git`, and a `SystemGitClient` over a temp data directory.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use rag_core::domain::git_location::GitLocation;
use rag_core::domain::source_id::SourceId;
use rag_infra::config::DataLayout;
use rag_infra::git::SystemGitClient;
use tempfile::TempDir;

/// A command for the test's own `git` calls: fixed identity, no signing, no
/// interaction, and no dependence on the developer's global configuration.
pub fn test_git(dir: &Path) -> Command {
    let mut command = Command::new("git");
    command
        .current_dir(dir)
        .args([
            "-c",
            "user.name=Test",
            "-c",
            "user.email=test@example.com",
            "-c",
            "commit.gpgsign=false",
            "-c",
            "tag.gpgsign=false",
            "-c",
            "core.autocrlf=false",
            "-c",
            "protocol.file.allow=always",
        ])
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_CONFIG_NOSYSTEM", "1");
    command
}

/// Runs git, panics on failure, returns trimmed stdout.
pub fn git_ok(dir: &Path, args: &[&str]) -> String {
    let output = test_git(dir).args(args).output().expect("git runs");
    assert!(
        output.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).trim().to_owned()
}

/// An upstream repository with a working tree, for the tests to commit into.
pub struct Upstream {
    _temp: TempDir,
    pub path: PathBuf,
}

impl Upstream {
    /// A repository on branch `main` with no commits.
    pub fn new() -> Self {
        Self::on_branch("main")
    }

    pub fn on_branch(branch: &str) -> Self {
        Self::init(&["-b", branch])
    }

    /// A repository using SHA-256 object ids, which `rag` does not support.
    pub fn sha256() -> Self {
        Self::init(&["-b", "main", "--object-format=sha256"])
    }

    fn init(options: &[&str]) -> Self {
        let temp = tempfile::tempdir().expect("temp dir");
        let path = temp.path().join("upstream");
        std::fs::create_dir_all(&path).unwrap();
        let mut args = vec!["init", "-q"];
        args.extend_from_slice(options);
        git_ok(&path, &args);
        Self { _temp: temp, path }
    }

    /// A repository with one commit holding `files`.
    pub fn with_files(files: &[(&str, &[u8])]) -> Self {
        let upstream = Self::new();
        for (name, content) in files {
            upstream.write(name, content);
        }
        upstream.commit("initial");
        upstream
    }

    pub fn git(&self, args: &[&str]) -> String {
        git_ok(&self.path, args)
    }

    pub fn write(&self, name: &str, content: &[u8]) {
        let file = self.path.join(name);
        std::fs::create_dir_all(file.parent().unwrap()).unwrap();
        std::fs::write(file, content).unwrap();
    }

    pub fn remove(&self, name: &str) {
        std::fs::remove_file(self.path.join(name)).unwrap();
    }

    /// Stages everything and commits; returns the new commit id.
    pub fn commit(&self, message: &str) -> String {
        self.git(&["add", "-A"]);
        self.git(&["commit", "-q", "--allow-empty", "-m", message]);
        self.head()
    }

    pub fn head(&self) -> String {
        self.git(&["rev-parse", "HEAD"])
    }

    /// Builds a commit whose root tree holds exactly `entries` (mode, name bytes,
    /// content), bypassing the working tree so names can be arbitrary bytes, and
    /// points `main` at it.
    pub fn commit_raw_tree(&self, entries: &[(&str, &[u8], &[u8])]) -> String {
        let mut listing = Vec::new();
        for (mode, name, content) in entries {
            let oid = if *mode == "160000" {
                "1111111111111111111111111111111111111111".to_owned()
            } else {
                self.hash_object(content)
            };
            let kind = if *mode == "160000" { "commit" } else { "blob" };
            listing.extend_from_slice(format!("{mode} {kind} {oid}\t").as_bytes());
            listing.extend_from_slice(name);
            listing.push(0);
        }
        let tree = self.pipe(&["mktree", "-z", "--missing"], &listing);
        let commit = self.git(&["commit-tree", &tree, "-m", "raw"]);
        self.git(&["update-ref", "refs/heads/main", &commit]);
        commit
    }

    fn hash_object(&self, content: &[u8]) -> String {
        self.pipe(&["hash-object", "-w", "--stdin"], content)
    }

    fn pipe(&self, args: &[&str], input: &[u8]) -> String {
        let mut child = test_git(&self.path)
            .args(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        child.stdin.take().unwrap().write_all(input).unwrap();
        let output = child.wait_with_output().unwrap();
        assert!(
            output.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8_lossy(&output.stdout).trim().to_owned()
    }

    /// The upstream as a plain filesystem location.
    pub fn location(&self) -> GitLocation {
        GitLocation::parse(self.path.to_str().unwrap()).expect("local path location")
    }

    /// The upstream as a `file://` URL.
    pub fn file_url(&self) -> GitLocation {
        let path = self.path.to_str().unwrap().replace('\\', "/");
        let prefix = if path.starts_with('/') {
            "file://"
        } else {
            "file:///"
        };
        GitLocation::parse(&format!("{prefix}{path}")).expect("file location")
    }

    pub fn source_id(&self) -> SourceId {
        SourceId::from_location(&self.location()).expect("source id")
    }
}

/// A client over a fresh temp data directory.
pub struct Sandbox {
    _temp: TempDir,
    pub layout: DataLayout,
    pub client: SystemGitClient,
}

impl Sandbox {
    pub fn new() -> Self {
        let temp = tempfile::tempdir().expect("temp dir");
        let layout = DataLayout::new(temp.path().join("data"));
        layout.ensure_dirs().unwrap();
        let client = SystemGitClient::new(layout.repos_dir(), layout.tmp_dir());
        Self {
            _temp: temp,
            layout,
            client,
        }
    }

    pub fn checkout_dir(&self, source: &SourceId) -> PathBuf {
        self.layout.repos_dir().join(source.checkout_dir_name())
    }
}
