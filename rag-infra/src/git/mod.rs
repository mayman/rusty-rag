//! The `GitClient` adapter: the system `git` as a subprocess.
//!
//! Each source is a bare, shallow object database at `<repos>/<hash>/` (no working
//! tree). A clone is built in `<tmp>/clone-<hash>/` (`init --bare --template=` +
//! `fetch --depth 1 origin <ref>`) and renamed into place, so a crash never leaves a
//! half-made checkout where a good one is expected. Replacing an existing checkout
//! keeps the old one until the new one is complete: the old directory is moved aside,
//! the new one moved in, and the old one is put back if that fails. The fetched
//! commit is pinned under `refs/rag/head` so it survives garbage collection. Every
//! subprocess runs hardened (see `process`) under a timeout, in `<tmp>`, with hooks
//! pointed at an empty directory.
//!
//! `tmp` and `repos` must be on one volume (both are children of the data
//! directory, see `config`), because clones and replacements are renames between them.
//! Callers hold the per-source `OperationLock` around `clone_shallow`,
//! `fetch_update`, and `remove_checkout`.
//!
//! Needs `git` 2.38 or newer: reusing a checkout asks `rev-parse
//! --show-object-format`. A question git cannot answer is an error, never a reason
//! to replace the checkout.

mod classify;
mod parse;
pub mod process;

use std::collections::{BTreeMap, BTreeSet};
use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::time::Duration;

use rag_core::application::error::GitError;
use rag_core::application::ports::{GitClient, TreeListing, MAX_BLOB_BYTES, MAX_READ_BYTES};
use rag_core::domain::git_location::GitLocation;
use rag_core::domain::git_ref::GitRef;
use rag_core::domain::oid::{BlobOid, CommitId};
use rag_core::domain::source_id::SourceId;

use crate::lock::FileOperationLock;

use self::classify::classify;
use self::parse::{parse_batch, parse_default_branch, parse_ls_tree, BatchEntry};
use self::process::{git_command, hooks_dir, run_capped, with_args, Output, RunError};

/// Time limit for operations that talk to the remote.
const DEFAULT_NETWORK_TIMEOUT: Duration = Duration::from_secs(300);
/// Time limit for operations on the local object database.
const DEFAULT_LOCAL_TIMEOUT: Duration = Duration::from_secs(120);
/// Room per requested blob for the `<oid> blob <size>\n` header and the trailing
/// newline that `git cat-file --batch` prints around the content.
const BATCH_HEADER_SLACK: usize = 128;
/// Output limit for the commands that print listings (`ls-tree`, `ls-remote`, ...).
const MAX_LISTING_BYTES: usize = 256 * 1024 * 1024;
/// The empty hooks directory is kept in the scratch directory.
const HOOKS_DIR_NAME: &str = "no-hooks";
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
    max_read_bytes: u64,
    isolated_config: bool,
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
            max_read_bytes: MAX_READ_BYTES,
            isolated_config: false,
        }
    }

    /// Lowers the most content one `read_blobs` call returns (default
    /// `MAX_READ_BYTES`), for tests of the limit.
    pub fn with_max_read_bytes(mut self, bytes: u64) -> Self {
        self.max_read_bytes = bytes;
        self
    }

    /// Makes `git` ignore the system and the user's global configuration (the
    /// latter points at a file that does not exist). For hermetic tests; normal use
    /// keeps the user's configuration, which may carry credential helpers, ssh
    /// settings, and proxies.
    pub fn with_isolated_config(mut self) -> Self {
        self.isolated_config = true;
        self
    }

    /// Overrides the time limits for remote and local operations.
    pub fn with_timeouts(mut self, network: Duration, local: Duration) -> Self {
        self.network_timeout = network;
        self.local_timeout = local;
        self
    }

    /// Empties the scratch directory: trash from removals that were blocked, and
    /// staging directories of crashed clones. Best effort.
    ///
    /// A staging directory `clone-<hash>` may belong to a clone that is running right
    /// now in another process, so it is removed only if the lock of that source can be
    /// taken (the operation that clones holds it), and the lock is held while the
    /// directory is deleted. Trash is nobody''s working data and goes freely.
    pub fn collect_garbage(&self, locks: &FileOperationLock) {
        let Ok(entries) = std::fs::read_dir(&self.tmp_dir) else {
            return;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let name = entry.file_name().to_string_lossy().into_owned();
            if name == HOOKS_DIR_NAME {
                continue;
            }
            // Held until the end of this iteration.
            let _lock = match name.strip_prefix("clone-") {
                Some(hash) => match locks.try_acquire_named(hash) {
                    Ok(guard) => Some(guard),
                    Err(_) => continue,
                },
                None => None,
            };
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

    /// A hardened `git` command for this client: scratch directory as the working
    /// directory, an empty directory for hooks, and (in isolated mode) no system or
    /// global configuration.
    fn command(&self, git_dir: Option<&Path>) -> std::process::Command {
        // Best effort: if this fails, spawning the command reports the problem.
        let _ = std::fs::create_dir_all(hooks_dir(&self.tmp_dir));
        let mut command = git_command(&self.tmp_dir, git_dir);
        if self.isolated_config {
            command.env("GIT_CONFIG_NOSYSTEM", "1").env(
                "GIT_CONFIG_GLOBAL",
                self.tmp_dir.join("no-global-gitconfig"),
            );
        }
        command
    }

    /// Runs `git` and returns its output whatever the exit status. Stdout beyond
    /// `max_stdout` bytes kills the process (`TooLarge`).
    fn run(
        &self,
        command: std::process::Command,
        input: Option<Vec<u8>>,
        timeout: Duration,
        max_stdout: usize,
    ) -> Result<Output, GitError> {
        run_capped(command, input, timeout, max_stdout).map_err(|error| match error {
            RunError::TimedOut => GitError::Timeout,
            RunError::OutputTooLarge => {
                GitError::TooLarge("git printed more output than the limit allows".into())
            }
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
        let command = with_args(self.command(git_dir), args);
        let output = self.run(command, input, timeout, MAX_LISTING_BYTES)?;
        if output.status.success() {
            Ok(output)
        } else {
            Err(classify(&String::from_utf8_lossy(&output.stderr)))
        }
    }

    /// Runs one `git` question about the checkout at `dir`. `Some(answer)` on
    /// success; `None` when git says the question has no answer because the
    /// directory is no repository (or, with `exit_1_is_absent`, the setting is
    /// unset). Every other failure, including a timeout, is an error: a probe that
    /// could not be answered must never be read as "not reusable".
    fn probe_value(
        &self,
        dir: &Path,
        args: &[&str],
        exit_1_is_absent: bool,
    ) -> Result<Option<String>, GitError> {
        let command = with_args(self.command(Some(dir)), args);
        let output = self.run(command, None, self.local_timeout, MAX_LISTING_BYTES)?;
        if output.status.success() {
            return Ok(Some(
                String::from_utf8_lossy(&output.stdout).trim().to_owned(),
            ));
        }
        let stderr = String::from_utf8_lossy(&output.stderr);
        if stderr.to_ascii_lowercase().contains("not a git repository") {
            return Ok(None);
        }
        if exit_1_is_absent && output.status.code() == Some(1) && stderr.trim().is_empty() {
            return Ok(None);
        }
        Err(classify(&stderr))
    }

    /// Whether `dir` is a bare SHA-1 repository whose origin is `location`. `false`
    /// means "something else lives there" (a half-made clone, another origin);
    /// an error means the question could not be answered.
    fn is_reusable(&self, dir: &Path, location: &GitLocation) -> Result<bool, GitError> {
        let bare = self.probe_value(dir, &["rev-parse", "--is-bare-repository"], false)?;
        if bare.as_deref() != Some("true") {
            return Ok(false);
        }
        let format = self.probe_value(dir, &["rev-parse", "--show-object-format"], false)?;
        if format.as_deref() != Some("sha1") {
            return Ok(false);
        }
        let origin = self.probe_value(dir, &["config", "--get", "remote.origin.url"], true)?;
        Ok(origin.as_deref() == Some(location.as_str()))
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
        // An empty `--template=` copies no template files, so no hooks (sample or
        // not) ever land in a checkout.
        let init = with_args(
            self.command(None),
            [
                OsStr::new("init"),
                OsStr::new("--quiet"),
                OsStr::new("--bare"),
                OsStr::new("--object-format=sha1"),
                OsStr::new("--template="),
                staging.as_os_str(),
            ],
        );
        let output = self.run(init, None, self.local_timeout, MAX_LISTING_BYTES)?;
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
        self.move_aside(path).map(|_| ())
    }

    /// Renames `path` to a fresh `trash-*` name in the scratch directory and returns
    /// that name.
    fn move_aside(&self, path: &Path) -> std::io::Result<PathBuf> {
        std::fs::create_dir_all(&self.tmp_dir)?;
        let unique = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |elapsed| elapsed.as_nanos());
        let name = path.file_name().unwrap_or_default().to_string_lossy();
        let aside = self.tmp_dir.join(format!("trash-{name}-{unique}"));
        rename_with_retries(path, &aside)?;
        Ok(aside)
    }

    /// Puts the finished `staging` directory at `dest`. An existing `dest` is moved
    /// aside first and put back if the new one cannot be moved in, so a failure
    /// leaves the old checkout where it was; it is deleted only after the new one is
    /// in place.
    fn swap_into_place(&self, staging: &Path, dest: &Path) -> Result<(), GitError> {
        if !dest.exists() {
            return self.move_into_place(staging, dest);
        }
        let aside = self.move_aside(dest).map_err(|error| {
            GitError::Failed(
                format!("cannot replace the existing local copy (it stays as it was): {error}")
                    .into(),
            )
        })?;
        match self.move_into_place(staging, dest) {
            Ok(()) => {
                // Best effort: a leftover is collected by `collect_garbage`.
                let _ = self.remove_tree(&aside);
                Ok(())
            }
            Err(error) => {
                let _ = rename_with_retries(&aside, dest);
                Err(error)
            }
        }
    }

    fn move_into_place(&self, staging: &Path, dest: &Path) -> Result<(), GitError> {
        rename_with_retries(staging, dest).map_err(|error| {
            GitError::Failed(format!("cannot move the clone into place: {error}").into())
        })
    }
}

/// `std::fs::rename`, retried a few times: antivirus or an indexer may hold a
/// fresh directory open for a moment.
fn rename_with_retries(from: &Path, to: &Path) -> std::io::Result<()> {
    let mut attempt = 0;
    loop {
        match std::fs::rename(from, to) {
            Ok(()) => return Ok(()),
            Err(error) if attempt + 1 >= FILESYSTEM_ATTEMPTS => return Err(error),
            Err(_) => {
                attempt += 1;
                std::thread::sleep(Duration::from_millis(50 * u64::from(attempt)));
            }
        }
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
        // A probe that cannot be answered is an error, never a reason to replace.
        if dest.exists() && self.is_reusable(&dest, location)? {
            return self.fetch_into(&dest, git_ref);
        }
        let io_failure = |error: std::io::Error| {
            GitError::Failed(format!("cannot prepare the clone directory: {error}").into())
        };
        std::fs::create_dir_all(&self.tmp_dir).map_err(io_failure)?;
        std::fs::create_dir_all(&self.repos_dir).map_err(io_failure)?;

        // The replacement is built next to the old checkout and swapped in only when
        // complete; the old checkout, if any, survives every failure on the way.
        let staging = self.staging_dir(source);
        self.remove_tree(&staging)?;
        let result = self
            .populate(&staging, location, git_ref)
            .and_then(|commit| self.swap_into_place(&staging, &dest).map(|()| commit));
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
        let request: Vec<u8> = wanted
            .iter()
            .flat_map(|id| format!("{id}\n").into_bytes())
            .collect();

        // One process answers for every blob: its header tells the type and size, so
        // a missing object, a non-blob, and an oversized blob are each seen in the
        // answer itself. The output cap keeps a request for too much from filling
        // memory; the process is killed when it is exceeded.
        let max_read = usize::try_from(self.max_read_bytes).unwrap_or(usize::MAX);
        let cap = max_read
            .saturating_add(wanted.len().saturating_mul(BATCH_HEADER_SLACK))
            .saturating_add(1024);
        let command = with_args(self.command(Some(&dir)), ["cat-file", "--batch"]);
        let output = self.run(command, Some(request), self.local_timeout, cap)?;
        if !output.status.success() {
            return Err(classify(&String::from_utf8_lossy(&output.stderr)));
        }
        let expected: Vec<BlobOid> = wanted.iter().map(|id| (*id).clone()).collect();
        let entries = parse_batch(&output.stdout, &expected, MAX_BLOB_BYTES)
            .map_err(|message| GitError::Failed(message.into()))?;

        let mut contents = BTreeMap::new();
        let mut total = 0u64;
        for (id, entry) in expected.into_iter().zip(entries) {
            match entry {
                BatchEntry::Blob(bytes) => {
                    total += bytes.len() as u64;
                    contents.insert(id, bytes);
                }
                BatchEntry::TooLarge { size } => {
                    return Err(GitError::TooLarge(
                        format!("blob {id} is {size} bytes, the limit is {MAX_BLOB_BYTES}").into(),
                    ))
                }
                BatchEntry::Missing => {
                    return Err(GitError::NotFound(
                        format!("blob {id} is not stored").into(),
                    ))
                }
                BatchEntry::OtherType => {
                    return Err(GitError::Failed(
                        format!("object {id} is not a blob").into(),
                    ))
                }
            }
        }
        if total > self.max_read_bytes {
            return Err(GitError::TooLarge(
                format!(
                    "{total} bytes requested, the limit is {}; slice the request",
                    self.max_read_bytes
                )
                .into(),
            ));
        }
        Ok(contents)
    }
    fn remove_checkout(&self, source: &SourceId) -> Result<(), GitError> {
        self.remove_tree(&self.checkout_dir(source))?;
        self.remove_tree(&self.staging_dir(source))
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::SystemGitClient;

    fn client(isolated: bool) -> (SystemGitClient, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let client = SystemGitClient::new(dir.path().join("repos"), dir.path().join("tmp"));
        (
            if isolated {
                client.with_isolated_config()
            } else {
                client
            },
            dir,
        )
    }

    fn env(command: &std::process::Command, name: &str) -> Option<Option<PathBuf>> {
        command
            .get_envs()
            .find(|(key, _)| *key == name)
            .map(|(_, value)| value.map(PathBuf::from))
    }

    #[test]
    fn an_isolated_client_ignores_the_system_and_user_git_configuration() {
        let (client, dir) = client(true);
        let command = client.command(None);
        assert_eq!(env(&command, "GIT_CONFIG_NOSYSTEM"), Some(Some("1".into())));
        let global = env(&command, "GIT_CONFIG_GLOBAL").unwrap().unwrap();
        assert!(global.starts_with(dir.path()) && !global.exists());
    }

    #[test]
    fn a_normal_client_keeps_the_users_git_configuration() {
        let (client, _dir) = client(false);
        let command = client.command(None);
        assert_eq!(env(&command, "GIT_CONFIG_NOSYSTEM"), None);
        assert_eq!(env(&command, "GIT_CONFIG_GLOBAL"), None);
    }

    #[test]
    fn every_command_runs_in_the_scratch_directory_with_an_empty_hooks_directory() {
        let (client, dir) = client(false);
        let command = client.command(None);
        assert_eq!(
            command.get_current_dir(),
            Some(dir.path().join("tmp").as_path())
        );
        let hooks = dir.path().join("tmp").join("no-hooks");
        assert!(hooks.is_dir());
        assert_eq!(std::fs::read_dir(hooks).unwrap().count(), 0);
    }
}
