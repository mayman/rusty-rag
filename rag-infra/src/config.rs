//! Data directory resolution and layout.
//!
//! The root is `RUSTY_RAG_HOME` when set (and non-empty, absolute), otherwise
//! `<OS local data dir>/rag` (`dirs::data_local_dir`: `%LOCALAPPDATA%` on Windows,
//! never the roaming profile, since an index and its clones are machine-local and
//! large). Resolution is a pure function of an environment lookup, so tests never
//! touch the real environment.
//!
//! ```text
//! <root>/index.db        the SQLite index
//! <root>/repos/<hash>/   one bare object database per source
//! <root>/locks/          per-source operation lock files
//! <root>/tmp/            scratch space (clone staging); safe to empty at start
//! ```
//!
//! `repos/` and `tmp/` are both children of the root, so they are on one volume
//! unless somebody mounts or links one of them elsewhere. The git adapter builds a
//! clone in `tmp/` and renames it into `repos/`, which only works within a volume;
//! `DataLayout::verify_same_volume` checks that with a probe rename.

use std::ffi::OsString;
use std::path::{Path, PathBuf};

/// The environment variable that overrides the data directory.
pub const HOME_ENV: &str = "RUSTY_RAG_HOME";

/// Why the data directory could not be determined or created.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ConfigError {
    #[error("no data directory: set {HOME_ENV} to an absolute path")]
    NoDataDirectory,
    #[error("{HOME_ENV} must be an absolute path")]
    RelativeHome,
    #[error("cannot create data directory {path}: {message}")]
    CreateFailed { path: String, message: String },
    #[error("cannot move a file from {from} to {to} (are they on different volumes?): {message}")]
    CannotMoveBetweenDirs {
        from: String,
        to: String,
        message: String,
    },
}

/// Where everything `rag` stores lives.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DataLayout {
    root: PathBuf,
}

impl DataLayout {
    pub fn new(root: PathBuf) -> Self {
        Self { root }
    }

    /// Resolves the root: the override from `env` if non-empty, else
    /// `<default_data_dir>/rag`.
    pub fn resolve(
        env: impl Fn(&str) -> Option<OsString>,
        default_data_dir: Option<PathBuf>,
    ) -> Result<Self, ConfigError> {
        if let Some(home) = env(HOME_ENV).filter(|value| !value.is_empty()) {
            let home = PathBuf::from(home);
            if !home.is_absolute() {
                return Err(ConfigError::RelativeHome);
            }
            return Ok(Self::new(home));
        }
        default_data_dir
            .map(|dir| Self::new(dir.join("rag")))
            .ok_or(ConfigError::NoDataDirectory)
    }

    /// The OS default parent of the root: the local (non-roaming) data directory.
    pub fn os_default_dir() -> Option<PathBuf> {
        dirs::data_local_dir()
    }

    /// Resolves from the real process environment and the OS defaults. Used by the
    /// composition root only.
    pub fn from_environment() -> Result<Self, ConfigError> {
        Self::resolve(|name| std::env::var_os(name), Self::os_default_dir())
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn index_db(&self) -> PathBuf {
        self.root.join("index.db")
    }

    pub fn repos_dir(&self) -> PathBuf {
        self.root.join("repos")
    }

    pub fn locks_dir(&self) -> PathBuf {
        self.root.join("locks")
    }

    pub fn tmp_dir(&self) -> PathBuf {
        self.root.join("tmp")
    }

    /// Checks that a file can be renamed from `tmp/` into `repos/` (the move a clone
    /// makes), which fails across volumes. Both directories must exist
    /// (`ensure_dirs`). Leaves nothing behind.
    pub fn verify_same_volume(&self) -> Result<(), ConfigError> {
        // Unique per process and call, so concurrent `rag` processes never share a probe file.
        static PROBES: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let name = format!(
            ".volume-probe-{}-{}",
            std::process::id(),
            PROBES.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        );
        let from = self.tmp_dir().join(&name);
        let to = self.repos_dir().join(&name);
        let failure = |message: String| ConfigError::CannotMoveBetweenDirs {
            from: self.tmp_dir().display().to_string(),
            to: self.repos_dir().display().to_string(),
            message,
        };
        std::fs::write(&from, b"").map_err(|error| failure(error.to_string()))?;
        let moved = std::fs::rename(&from, &to);
        let _ = std::fs::remove_file(&from);
        let _ = std::fs::remove_file(&to);
        moved.map_err(|error| failure(error.to_string()))
    }

    /// Creates the root and its subdirectories. Repeatable.
    pub fn ensure_dirs(&self) -> Result<(), ConfigError> {
        for dir in [
            self.root.clone(),
            self.repos_dir(),
            self.locks_dir(),
            self.tmp_dir(),
        ] {
            std::fs::create_dir_all(&dir).map_err(|error| ConfigError::CreateFailed {
                path: dir.display().to_string(),
                message: error.to_string(),
            })?;
        }
        Ok(())
    }
}
