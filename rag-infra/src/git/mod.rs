//! The `GitClient` adapter: the system `git` as a subprocess.
//!
//! Each source is a bare, shallow object database at `<repos>/<hash>/` (no working
//! tree). A clone is built in `<tmp>/clone-<hash>/` (`init --bare` + `fetch --depth
//! 1 origin <ref>`) and renamed into place, so a crash never leaves a half-made
//! checkout where a good one is expected. The fetched commit is pinned under
//! `refs/rag/head` so it survives garbage collection. Every subprocess runs
//! hardened (see `process`) under a timeout.

mod classify;
mod parse;
pub mod process;

use std::collections::{BTreeMap, BTreeSet};
use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::time::Duration;

use rag_core::application::error::GitError;
use rag_core::application::ports::{GitClient, TreeListing};
use rag_core::domain::git_location::GitLocation;
use rag_core::domain::git_ref::GitRef;
use rag_core::domain::oid::{BlobOid, CommitId};
use rag_core::domain::source_id::SourceId;

use self::classify::classify;
use self::parse::{
    parse_batch, parse_batch_check, parse_default_branch, parse_ls_tree, ObjectInfo,
};
use self::process::{git_command, run, with_args, Output, RunError};

/// Time limit for operations that talk to the remote.
const DEFAULT_NETWORK_TIMEOUT: Duration = Duration::from_secs(300);
/// Time limit for operations on the local object database.
const DEFAULT_LOCAL_TIMEOUT: Duration = Duration::from_secs(120);
/// `read_blobs` asks `git cat-file --batch` for at most this many blobs at once...
const MAX_BATCH_BLOBS: usize = 256;
/// ...and for at most this many bytes of content at once (a single larger blob is
/// read on its own).
const MAX_BATCH_BYTES: u64 = 32 * 1024 * 1024;
/// Attempts to delete or rename a directory that antivirus or an indexer may be
/// holding open for a moment.
const FILESYSTEM_ATTEMPTS: u32 = 5;
/// The ref that pins the last fetched commit.
const PIN_REF: &str = "refs/rag/head";

/// `GitClient` over the system `git`.
#[derive(Debug, Clone)]
pub struct SystemGitClient {
    repos_dir: PathBuf,
    tmp_dir: PathBuf,
    network_timeout: Duration,
    local_timeout: Duration,
}

impl SystemGitClient {
    /// `repos_dir` holds the checkouts; `tmp_dir` is scratch space on the same
    /// filesystem (clone staging, trash). Both are created on demand.
    pub fn new(repos_dir: PathBuf, tmp_dir: PathBuf) -> Self {
        Self {
            repos_dir,
            tmp_dir,
            network_timeout: DEFAULT_NETWORK_TIMEOUT,
            local_timeout: DEFAULT_LOCAL_TIMEOUT,
        }
    }

    /// Overrides the time limits for remote and local operations.
    pub fn with_timeouts(mut self, network: Duration, local: Duration) -> Self {
        self.network_timeout = network;
        self.local_timeout = local;
        self
    }

    /// Empties the scratch directory: staging directories of crashed clones and
    /// trash from removals that were blocked. Best effort; call at start-up while
    /// no other `rag` process is cloning.
    pub fn collect_garbage(&self) {
        let Ok(entries) = std::fs::read_dir(&self.tmp_dir) else {
            return;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                let _ = self.remove_tree(&path);
            } else {
                let _ = std::fs::remove_file(&path);
            }
        }
    }

    fn checkout_dir(&self, source: &SourceId) -> PathBuf {
        self.repos_dir.join(source.checkout_dir_name())
    }

    fn staging_dir(&self, source: &SourceId) -> PathBuf {
        self.tmp_dir
            .join(format!("clone-{}", source.checkout_dir_name()))
    }

    fn existing_checkout(&self, source: &SourceId) -> Result<PathBuf, GitError> {
        let dir = self.checkout_dir(source);
        if dir.is_dir() {
            Ok(dir)
        } else {
            Err(GitError::NotFound(
                format!("no local copy of source {source}").into(),
            ))
        }
    }

    /// Runs `git` and returns its output whatever the exit status.
    fn run(
        &self,
        command: std::process::Command,
        input: Option<Vec<u8>>,
        timeout: Duration,
    ) -> Result<Output, GitError> {
        run(command, input, timeout).map_err(|error| match error {
            RunError::TimedOut => GitError::Timeout,
            RunError::Io(message) => GitError::Failed(
                format!("cannot run git (is it installed and on PATH?): {message}").into(),
            ),
        })
    }

    /// Runs `git` and turns a non-zero exit into a classified `GitError`.
    fn run_ok<I, S>(
        &self,
        git_dir: Option<&Path>,
        args: I,
        input: Option<Vec<u8>>,
        timeout: Duration,
    ) -> Result<Output, GitError>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        let command = with_args(git_command(git_dir), args);
        let output = self.run(command, input, timeout)?;
        if output.status.success() {
            Ok(output)
        } else {
            Err(classify(&String::from_utf8_lossy(&output.stderr)))
        }
    }

    /// True when `dir` is a bare SHA-1 repository whose origin is `location`.
    fn is_reusable(&self, dir: &Path, location: &GitLocation) -> bool {
        let answer = |args: &[&str]| -> Option<String> {
            self.run_ok(Some(dir), args, None, self.local_timeout)
                .ok()
                .map(|output| String::from_utf8_lossy(&output.stdout).trim().to_owned())
        };
        answer(&["rev-parse", "--is-bare-repository"]).as_deref() == Some("true")
            && answer(&["rev-parse", "--show-object-format"]).as_deref() == Some("sha1")
            && answer(&["config", "--get", "remote.origin.url"]).as_deref()
                == Some(location.as_str())
    }

    /// Shallow-fetches `git_ref` from origin into the repository at `dir`, peels it
    /// to a commit, and pins that commit.
    fn fetch_into(&self, dir: &Path, git_ref: &GitRef) -> Result<CommitId, GitError> {
        self.run_ok(
            Some(dir),
            [
                "fetch",
                "--quiet",
                "--depth",
                "1",
                "--no-tags",
                "--no-recurse-submodules",
                "origin",
                git_ref.as_str(),
            ],
            None,
            self.network_timeout,
        )?;
        let peeled = self.run_ok(
            Some(dir),
            ["rev-parse", "--verify", "--quiet", "FETCH_HEAD^{commit}"],
            None,
            self.local_timeout,
        )?;
        let text = String::from_utf8_lossy(&peeled.stdout);
        let commit = CommitId::parse(text.trim())
            .map_err(|_| GitError::Failed("git returned an unexpected commit id".into()))?;
        self.run_ok(
            Some(dir),
            ["update-ref", PIN_REF, commit.as_str()],
            None,
            self.local_timeout,
        )?;
        Ok(commit)
    }

    /// Builds a fresh repository in `staging` holding `git_ref` of `location`.
    fn populate(
        &self,
        staging: &Path,
        location: &GitLocation,
        git_ref: &GitRef,
    ) -> Result<CommitId, GitError> {
        let init = with_args(
            git_command(None),
            [
                OsStr::new("init"),
                OsStr::new("--quiet"),
                OsStr::new("--bare"),
                OsStr::new("--object-format=sha1"),
                staging.as_os_str(),
            ],
        );
        let output = self.run(init, None, self.local_timeout)?;
        if !output.status.success() {
            return Err(classify(&String::from_utf8_lossy(&output.stderr)));
        }
        self.run_ok(
            Some(staging),
            ["remote", "add", "origin", location.as_str()],
            None,
            self.local_timeout,
        )?;
        self.fetch_into(staging, git_ref)
    }

    /// Deletes a directory tree, retrying while something holds it open, and as a
    /// last resort moves it into the scratch directory for `collect_garbage`. Git
    /// marks object files read-only; `std::fs::remove_dir_all` deletes read-only
    /// files on Windows (pinned by the `remove` tests), so no attribute clearing
    /// is needed here.
    fn remove_tree(&self, path: &Path) -> Result<(), GitError> {
        if !path.exists() {
            return Ok(());
        }
        let mut last_error = String::new();
        for attempt in 0..FILESYSTEM_ATTEMPTS {
            match std::fs::remove_dir_all(path) {
                Ok(()) => return Ok(()),
                Err(_) if !path.exists() => return Ok(()),
                Err(error) => last_error = error.to_string(),
            }
            std::thread::sleep(Duration::from_millis(50 * u64::from(attempt + 1)));
        }
        self.move_to_trash(path).map_err(|rename_error| {
            GitError::Failed(
                format!(
                    "cannot delete {}: {last_error} (and cannot move it away: {rename_error})",
                    path.display()
                )
                .into(),
            )
        })
    }

    fn move_to_trash(&self, path: &Path) -> std::io::Result<()> {
        std::fs::create_dir_all(&self.tmp_dir)?;
        let unique = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |elapsed| elapsed.as_nanos());
        let name = path.file_name().unwrap_or_default().to_string_lossy();
        std::fs::rename(path, self.tmp_dir.join(format!("trash-{name}-{unique}")))
    }

    fn move_into_place(&self, staging: &Path, dest: &Path) -> Result<(), GitError> {
        let mut last_error = String::new();
        for attempt in 0..FILESYSTEM_ATTEMPTS {
            match std::fs::rename(staging, dest) {
                Ok(()) => return Ok(()),
                Err(error) => last_error = error.to_string(),
            }
            std::thread::sleep(Duration::from_millis(50 * u64::from(attempt + 1)));
        }
        Err(GitError::Failed(
            format!("cannot move the clone into place: {last_error}").into(),
        ))
    }
}

impl GitClient for SystemGitClient {
    fn default_branch(&self, location: &GitLocation) -> Result<GitRef, GitError> {
        let output = self.run_ok(
            None,
            ["ls-remote", "--symref", location.as_str(), "HEAD"],
            None,
            self.network_timeout,
        )?;
        let branch = parse_default_branch(&output.stdout).ok_or_else(|| {
            if output.stdout.is_empty() {
                GitError::NotFound("the repository has no commits".into())
            } else {
                GitError::Failed("the remote HEAD does not point to a branch".into())
            }
        })?;
        GitRef::parse(&branch)
            .map_err(|error| GitError::Failed(format!("unusable default branch: {error}").into()))
    }

    fn clone_shallow(
        &self,
        source: &SourceId,
        location: &GitLocation,
        git_ref: &GitRef,
    ) -> Result<CommitId, GitError> {
        let dest = self.checkout_dir(source);
        if dest.exists() {
            if self.is_reusable(&dest, location) {
                return self.fetch_into(&dest, git_ref);
            }
            self.remove_tree(&dest)?;
        }
        let io_failure = |error: std::io::Error| {
            GitError::Failed(format!("cannot prepare the clone directory: {error}").into())
        };
        std::fs::create_dir_all(&self.tmp_dir).map_err(io_failure)?;
        std::fs::create_dir_all(&self.repos_dir).map_err(io_failure)?;

        let staging = self.staging_dir(source);
        self.remove_tree(&staging)?;
        let result = self
            .populate(&staging, location, git_ref)
            .and_then(|commit| self.move_into_place(&staging, &dest).map(|()| commit));
        if result.is_err() {
            let _ = self.remove_tree(&staging);
        }
        result
    }

    fn fetch_update(&self, source: &SourceId, git_ref: &GitRef) -> Result<CommitId, GitError> {
        let dir = self.existing_checkout(source)?;
        self.fetch_into(&dir, git_ref)
    }

    fn ls_tree(&self, source: &SourceId, commit: &CommitId) -> Result<TreeListing, GitError> {
        let dir = self.existing_checkout(source)?;
        let output = self.run_ok(
            Some(&dir),
            ["ls-tree", "-r", "-z", "-l", commit.as_str()],
            None,
            self.local_timeout,
        )?;
        parse_ls_tree(&output.stdout).map_err(|message| GitError::Failed(message.into()))
    }

    fn read_blobs(
        &self,
        source: &SourceId,
        blobs: &[BlobOid],
    ) -> Result<BTreeMap<BlobOid, Vec<u8>>, GitError> {
        let wanted: Vec<&BlobOid> = blobs.iter().collect::<BTreeSet<_>>().into_iter().collect();
        if wanted.is_empty() {
            return Ok(BTreeMap::new());
        }
        let dir = self.existing_checkout(source)?;
        let request = |ids: &[&BlobOid]| -> Vec<u8> {
            ids.iter()
                .flat_map(|id| format!("{id}\n").into_bytes())
                .collect()
        };

        let checked = self.run_ok(
            Some(&dir),
            ["cat-file", "--batch-check"],
            Some(request(&wanted)),
            self.local_timeout,
        )?;
        let infos = parse_batch_check(&checked.stdout)
            .ok()
            .filter(|infos| infos.len() == wanted.len())
            .ok_or_else(|| GitError::Failed("unexpected cat-file output".into()))?;

        let mut sizes = Vec::with_capacity(wanted.len());
        for (id, info) in wanted.iter().zip(&infos) {
            match info {
                ObjectInfo::Blob { size } => sizes.push(*size),
                ObjectInfo::Missing => {
                    return Err(GitError::NotFound(
                        format!("blob {id} is not stored").into(),
                    ))
                }
                ObjectInfo::OtherType => {
                    return Err(GitError::Failed(
                        format!("object {id} is not a blob").into(),
                    ))
                }
            }
        }

        let mut contents = BTreeMap::new();
        let mut start = 0;
        while start < wanted.len() {
            let mut end = start;
            let mut bytes = 0u64;
            while end < wanted.len()
                && end - start < MAX_BATCH_BLOBS
                && (end == start || bytes + sizes[end] <= MAX_BATCH_BYTES)
            {
                bytes += sizes[end];
                end += 1;
            }
            let batch = &wanted[start..end];
            let output = self.run_ok(
                Some(&dir),
                ["cat-file", "--batch"],
                Some(request(batch)),
                self.local_timeout,
            )?;
            let expected: Vec<BlobOid> = batch.iter().map(|id| (*id).clone()).collect();
            let bodies = parse_batch(&output.stdout, &expected)
                .map_err(|message| GitError::Failed(message.into()))?;
            contents.extend(expected.into_iter().zip(bodies));
            start = end;
        }
        Ok(contents)
    }

    fn remove_checkout(&self, source: &SourceId) -> Result<(), GitError> {
        self.remove_tree(&self.checkout_dir(source))?;
        self.remove_tree(&self.staging_dir(source))
    }
}
