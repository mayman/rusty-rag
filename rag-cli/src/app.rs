//! The composition root: resolves the data directory, builds the adapters for the
//! command, runs the use case, and renders its response. No business rules live
//! here; the use cases in `rag-core` own them.

use std::time::Duration;

use rag_core::application::add_source::{AddRequest, AddSource};
use rag_core::application::dto::{ListResponse, SearchRequest, SCHEMA_VERSION};
use rag_core::application::error::{AppError, Scrubbed, StorageError};
use rag_core::application::indexer::DEFAULT_LOCK_WAIT;
use rag_core::application::list_sources::ListSources;
use rag_core::application::ports::SourceRepository;
use rag_core::application::progress::Progress;
use rag_core::application::reindex::{Reindex, ReindexRequest};
use rag_core::application::remove_source::{RemoveRequest, RemoveSource};
use rag_core::application::search::Search;
use rag_core::application::show_file::{ShowFile, ShowFileRequest};
use rag_core::application::update_source::{UpdateRequest, UpdateSource};
use rag_infra::clock::SystemClock;
use rag_infra::config::{ConfigError, DataLayout};
use rag_infra::git::SystemGitClient;
use rag_infra::lock::FileOperationLock;
use rag_infra::sqlite::SqliteStore;

use crate::cli::Command;
use crate::location::resolve_target;
use crate::output::error::EXIT_OK;
use crate::output::{list, report, search, show};
use crate::progress::StderrProgress;

/// How a response is written.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Json,
    Human,
}

/// What a finished command wants written to stdout, and its exit code.
#[derive(Debug, PartialEq, Eq)]
pub struct Executed {
    pub stdout: String,
    pub code: u8,
}

impl Executed {
    fn ok(stdout: String) -> Self {
        Self {
            stdout,
            code: EXIT_OK,
        }
    }

    /// The output with the exit code the response decided on (`exit_code`).
    fn with_code(stdout: String, code: u8) -> Self {
        Self { stdout, code }
    }
}

/// What the process was asked beyond the command itself.
pub struct RunOptions<'a> {
    /// Where the progress lines for stderr go; also measures the phases.
    pub progress: &'a StderrProgress,
    /// `--no-wait`: do not wait for another process working on the same source.
    pub no_wait: bool,
}

impl RunOptions<'_> {
    fn lock_wait(&self) -> Duration {
        if self.no_wait {
            Duration::ZERO
        } else {
            DEFAULT_LOCK_WAIT
        }
    }
}

/// Runs `command` against the data directory named by the environment.
pub fn run(command: Command, mode: Mode, options: &RunOptions) -> Result<Executed, AppError> {
    let progress: &dyn Progress = options.progress;
    let layout = DataLayout::from_environment().map_err(config_error)?;
    match command {
        Command::Search {
            query,
            k,
            repos,
            languages,
            path,
            offset,
            max_chars,
        } => {
            let request = SearchRequest {
                query,
                k,
                offset,
                repos,
                languages,
                path,
                max_chars,
            };
            run_search(&layout, &request, mode)
        }
        Command::Show {
            chunk,
            repo,
            path,
            lines,
            around,
            max_chars,
        } => {
            let store = SqliteStore::open_for_read(layout.root())?;
            let git = git_client(&layout);
            let show_file = ShowFile::new(&store, &store, &git);
            let response = match (chunk, repo, path) {
                (Some(id), _, _) => show_file.execute_chunk(&id, max_chars)?,
                (None, Some(repo), Some(path)) => show_file.execute(&ShowFileRequest {
                    repo,
                    path,
                    lines,
                    around,
                    max_chars,
                })?,
                _ => return Err(usage("show needs --chunk <id> or <repo> <path>")),
            };
            Ok(Executed::ok(match mode {
                Mode::Json => show::render_show_json(&response),
                Mode::Human => show::render_show_human(&response),
            }))
        }
        Command::List => run_list(&layout, mode),
        Command::Add { target, name } => {
            let location = resolve_target(&target)?;
            let request = AddRequest { location, name };
            let writer = Writer::open(&layout)?;
            options
                .progress
                .note(&format!("indexing {}", request.location));
            let response = AddSource::new(
                &writer.store,
                &writer.store,
                &writer.git,
                &writer.locks,
                &SystemClock,
            )
            .with_progress(progress)
            .with_lock_wait(options.lock_wait())
            .execute(&request)?;
            Ok(Executed::ok(match mode {
                Mode::Json => report::render_add_json(&response),
                Mode::Human => report::render_add_human(&response),
            }))
        }
        Command::Update { name } => {
            let request = name.map_or_else(UpdateRequest::all, |name| UpdateRequest::named(&name));
            let writer = Writer::open(&layout)?;
            options.progress.note("updating and indexing");
            let response = UpdateSource::new(
                &writer.store,
                &writer.store,
                &writer.git,
                &writer.locks,
                &SystemClock,
            )
            .with_progress(progress)
            .with_lock_wait(options.lock_wait())
            .execute(&request)?;
            Ok(Executed::with_code(
                match mode {
                    Mode::Json => report::render_update_json(&response),
                    Mode::Human => report::render_update_human(&response),
                },
                response.exit_code(),
            ))
        }
        Command::Remove { name } => {
            let writer = Writer::open(&layout)?;
            let response =
                RemoveSource::new(&writer.store, &writer.store, &writer.git, &writer.locks)
                    .execute(&RemoveRequest::new(&name))?;
            Ok(Executed::ok(match mode {
                Mode::Json => report::render_remove_json(&response),
                Mode::Human => report::render_remove_human(&response),
            }))
        }
        Command::Reindex { name, all: _ } => {
            let request =
                name.map_or_else(ReindexRequest::all, |name| ReindexRequest::named(&name));
            let writer = Writer::open(&layout)?;
            options.progress.note("reindexing");
            let response = Reindex::new(
                &writer.store,
                &writer.store,
                &writer.git,
                &writer.locks,
                &SystemClock,
            )
            .with_progress(progress)
            .execute(&request)?;
            Ok(Executed::with_code(
                match mode {
                    Mode::Json => report::render_reindex_json(&response),
                    Mode::Human => report::render_reindex_human(&response),
                },
                response.exit_code(),
            ))
        }
    }
}

fn run_search(
    layout: &DataLayout,
    request: &SearchRequest,
    mode: Mode,
) -> Result<Executed, AppError> {
    // A read never creates the database: a missing file is `index_missing`.
    let store = SqliteStore::open_for_read(layout.root())?;
    let response = Search::new(&store, &store, &SystemClock).execute(request)?;
    Ok(Executed::ok(match mode {
        Mode::Json => search::render_search_json(response, request.max_chars),
        Mode::Human => {
            let response = search::fit_to_budget(response, request.max_chars);
            crate::output::human::render_search_human(&response)
        }
    }))
}

fn run_list(layout: &DataLayout, mode: Mode) -> Result<Executed, AppError> {
    let response = match SqliteStore::open_for_read(layout.root()) {
        Ok(store) => ListSources::new(&store, &store, &SystemClock).execute()?,
        // Nothing added yet is an empty list, not a failure.
        Err(StorageError::IndexMissing) => ListResponse {
            schema_version: SCHEMA_VERSION,
            sources: Vec::new(),
        },
        Err(error) => return Err(error.into()),
    };
    Ok(Executed::ok(match mode {
        Mode::Json => list::render_list_json(&response),
        Mode::Human => crate::output::human::render_list_human(&response),
    }))
}

/// The adapters of a command that changes the index.
struct Writer {
    store: SqliteStore,
    git: SystemGitClient,
    locks: FileOperationLock,
}

impl Writer {
    /// Creates the data directory if needed, opens the index for writing, and
    /// collects leftovers of crashed or half-finished runs: scratch space, and
    /// clones that no registered source owns.
    fn open(layout: &DataLayout) -> Result<Self, AppError> {
        layout.ensure_dirs().map_err(config_error)?;
        layout.verify_same_volume().map_err(config_error)?;
        let git = git_client(layout);
        let locks = FileOperationLock::new(layout.locks_dir());
        let store = SqliteStore::open_for_write(layout.root())?;
        git.collect_garbage(&locks);
        git.collect_unregistered_checkouts(&locks, &|hash| owns_checkout(&store, hash));
        Ok(Self { store, git, locks })
    }
}

/// Whether a registered source has the clone directory `hash`. When the catalog
/// cannot be read the answer is yes: nothing is deleted on a guess.
fn owns_checkout(sources: &dyn SourceRepository, hash: &str) -> bool {
    sources.list().map_or(true, |all| {
        all.iter()
            .any(|source| source.id().checkout_dir_name() == hash)
    })
}

fn git_client(layout: &DataLayout) -> SystemGitClient {
    SystemGitClient::new(layout.repos_dir(), layout.tmp_dir())
}

fn usage(message: &str) -> AppError {
    AppError::Usage(Scrubbed::from(message))
}

fn config_error(error: ConfigError) -> AppError {
    match error {
        ConfigError::NoDataDirectory | ConfigError::RelativeHome => {
            AppError::Usage(Scrubbed::from(error.to_string()))
        }
        other => AppError::Storage(StorageError::Failed(Scrubbed::from(other.to_string()))),
    }
}
