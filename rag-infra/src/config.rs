//! Data directory resolution and layout.
//!
//! The root is `RUSTY_RAG_HOME` when set (and non-empty, absolute), otherwise
//! `<OS data dir>/rag` (`dirs::data_dir`). Resolution is a pure function of an
//! environment lookup, so tests never touch the real environment.
//!
//! ```text
//! <root>/index.db        the SQLite index
//! <root>/repos/<hash>/   one bare object database per source
//! <root>/locks/          per-source operation lock files
//! <root>/tmp/            scratch space (clone staging); safe to empty at start
//! ```

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

    /// Resolves from the real process environment and the OS defaults. Used by the
    /// composition root only.
    pub fn from_environment() -> Result<Self, ConfigError> {
        Self::resolve(|name| std::env::var_os(name), dirs::data_dir())
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
