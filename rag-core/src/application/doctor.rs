//! Doctor: a read-only health report of the data directory, the database, `git`, and
//! every source. It changes nothing and deletes nothing: leftovers are counted, a
//! problem is a warning with what to do (addressed to the user when the fix changes
//! their index or their machine).

use serde::Serialize;

use crate::application::dto::{SourceSummary, SCHEMA_VERSION};
use crate::application::error::{shell_word, AppError, StorageError};
use crate::application::ports::{
    ChunkStore, Clock, Diagnostics, GitFacts, LockState, SourceRepository,
};
use crate::domain::index_versions::IndexVersions;
use crate::domain::source::Source;

/// The oldest `git` rag supports: reusing a clone asks `rev-parse --show-object-format`.
pub const MINIMUM_GIT_VERSION: (u32, u32) = (2, 38);

/// Input of the doctor use case.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct DoctorRequest {
    /// `--integrity`: also run SQLite's integrity check (slow on a large index).
    pub integrity: bool,
}

/// The catalog and the chunk store, when the database could be opened.
#[derive(Clone, Copy)]
pub struct Catalog<'a> {
    pub sources: &'a dyn SourceRepository,
    pub chunks: &'a dyn ChunkStore,
}

/// The database file.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct DatabaseReport {
    pub exists: bool,
    pub size_bytes: u64,
    pub wal_bytes: u64,
    /// `ok` or the problems found; present only with `--integrity`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub integrity: Option<String>,
    /// Why the database could not be read, when it exists but cannot be opened.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// `git` on this machine.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct GitReport {
    pub on_path: bool,
    /// The version number as git prints it (`2.55.0.windows.5`).
    pub version: Option<String>,
    /// The version is known and at least `minimum`.
    pub supported: bool,
    pub minimum: String,
}

/// One source in the report: what `rag list` shows, plus the state of its files.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct DoctorSource {
    #[serde(flatten)]
    pub summary: SourceSummary,
    /// `present` or `missing`: the local copy that `show` and `update` need.
    pub checkout: &'static str,
    /// The index was made by other tokenizer, chunker, or policy versions.
    pub stale_versions: bool,
    /// `free` or `held`.
    pub operation_lock: &'static str,
    /// What the holder of the operation lock runs, when it said.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub lock_operation: Option<String>,
}

/// What was found lying around; counted, never deleted by `rag doctor`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct LeftoversReport {
    pub tmp_entries: usize,
    pub unregistered_checkouts: usize,
}

/// Output of the doctor use case.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct DoctorResponse {
    pub schema_version: u32,
    /// No warnings.
    pub ok: bool,
    pub data_dir: String,
    pub database: DatabaseReport,
    pub git: GitReport,
    pub sources: Vec<DoctorSource>,
    pub leftovers: LeftoversReport,
    pub warnings: Vec<String>,
}

/// `(major, minor)` of a `git --version` line such as `git version 2.55.0.windows.5`.
pub fn parse_git_version(line: &str) -> Option<(u32, u32)> {
    let number = line
        .trim()
        .strip_prefix("git version")?
        .split_whitespace()
        .next()?;
    let mut parts = number.split('.');
    let major = parts.next()?.parse().ok()?;
    let minor = parts.next()?.parse().ok()?;
    Some((major, minor))
}

/// The version number of a `git --version` line, as printed.
fn version_number(line: &str) -> Option<String> {
    let number = line
        .trim()
        .strip_prefix("git version")?
        .split_whitespace()
        .next()?;
    Some(number.to_owned())
}

/// `rag doctor`.
pub struct Doctor<'a> {
    clock: &'a dyn Clock,
    diagnostics: &'a dyn Diagnostics,
    catalog: Result<Catalog<'a>, StorageError>,
}

impl<'a> Doctor<'a> {
    /// `catalog` is what opening the database gave: `IndexMissing` means nothing was
    /// added yet (not a problem); any other error is reported as a warning.
    pub fn new(
        clock: &'a dyn Clock,
        diagnostics: &'a dyn Diagnostics,
        catalog: Result<Catalog<'a>, StorageError>,
    ) -> Self {
        Self {
            clock,
            diagnostics,
            catalog,
        }
    }

    /// Fails only when the data directory cannot be read; everything else is a
    /// finding in the response.
    pub fn execute(&self, request: &DoctorRequest) -> Result<DoctorResponse, AppError> {
        self.diagnostics.check_data_dir()?;
        let mut warnings = Vec::new();

        let facts = self.diagnostics.database(request.integrity);
        let mut database = DatabaseReport {
            exists: facts.exists,
            size_bytes: facts.size_bytes,
            wal_bytes: facts.wal_bytes,
            integrity: facts.integrity,
            error: None,
        };
        if let Some(integrity) = database.integrity.as_deref().filter(|text| *text != "ok") {
            warnings.push(format!(
                "the database failed its integrity check ({integrity}); ask the user to restore it or to remove the data directory and add the sources again"
            ));
        }

        let sources = match &self.catalog {
            Ok(catalog) => self.source_reports(catalog, &mut warnings)?,
            Err(StorageError::IndexMissing) => Vec::new(),
            Err(error) => {
                database.error = Some(error.to_string());
                warnings.push(format!("the database cannot be read: {error}"));
                Vec::new()
            }
        };

        let registered: Vec<String> = match &self.catalog {
            Ok(catalog) => catalog
                .sources
                .list()?
                .iter()
                .map(|source| source.id().checkout_dir_name().to_owned())
                .collect(),
            Err(_) => Vec::new(),
        };
        let found = self.diagnostics.leftovers(&registered);
        if found.tmp_entries > 0 || found.unregistered_checkouts > 0 {
            warnings.push(format!(
                "{} leftover entries in tmp/ and {} clone directories no source owns; the next add or update cleans them up",
                found.tmp_entries, found.unregistered_checkouts
            ));
        }

        let git = git_report(&self.diagnostics.git(), &mut warnings);
        Ok(DoctorResponse {
            schema_version: SCHEMA_VERSION,
            ok: warnings.is_empty(),
            data_dir: self.diagnostics.data_dir(),
            database,
            git,
            sources,
            leftovers: LeftoversReport {
                tmp_entries: found.tmp_entries,
                unregistered_checkouts: found.unregistered_checkouts,
            },
            warnings,
        })
    }

    fn source_reports(
        &self,
        catalog: &Catalog<'_>,
        warnings: &mut Vec<String>,
    ) -> Result<Vec<DoctorSource>, AppError> {
        let now = self.clock.now();
        let mut reports = Vec::new();
        for source in catalog.sources.list()? {
            let chunks = catalog.chunks.chunk_count(source.id())?;
            let stale = catalog
                .chunks
                .stored_versions(source.id())?
                .is_some_and(|stored| stored != IndexVersions::current());
            let checkout = if self.diagnostics.checkout_present(source.id()) {
                "present"
            } else {
                "missing"
            };
            let (operation_lock, lock_operation) =
                match self.diagnostics.operation_lock(source.id()) {
                    LockState::Free => ("free", None),
                    LockState::Held(operation) => ("held", operation),
                };
            let summary = SourceSummary::new(&source, chunks, now);
            warn_about_source(&source, &summary, checkout, stale, warnings);
            reports.push(DoctorSource {
                summary,
                checkout,
                stale_versions: stale,
                operation_lock,
                lock_operation,
            });
        }
        Ok(reports)
    }
}

fn warn_about_source(
    source: &Source,
    summary: &SourceSummary,
    checkout: &str,
    stale: bool,
    warnings: &mut Vec<String>,
) {
    let name = shell_word(source.name());
    if checkout == "missing" {
        warnings.push(format!(
            "source {name}: its local copy is missing (searching still works; show and update do not); ask the user to run `rag remove {name}` and `rag add` for it again"
        ));
    }
    if stale {
        warnings.push(format!(
            "source {name}: its index was made by an older version; ask the user to run `rag reindex {name}`"
        ));
    }
    if let Some(error) = &summary.last_error {
        warnings.push(format!(
            "source {name}: the last operation failed ({}): {}",
            error.code, error.message
        ));
    }
}

fn git_report(facts: &GitFacts, warnings: &mut Vec<String>) -> GitReport {
    let (minimum_major, minimum_minor) = MINIMUM_GIT_VERSION;
    let minimum = format!("{minimum_major}.{minimum_minor}");
    let line = facts.version_text.as_deref();
    let parsed = line.and_then(parse_git_version);
    let supported = parsed.is_some_and(|version| version >= MINIMUM_GIT_VERSION);
    if !facts.on_path {
        warnings
            .push("git was not found on PATH; rag needs git to add and update sources".to_owned());
    } else if parsed.is_none() {
        warnings.push("the version of git could not be read".to_owned());
    } else if !supported {
        warnings.push(format!(
            "git {} is older than {minimum}; update git, because reusing a clone needs it",
            line.and_then(version_number).unwrap_or_default()
        ));
    }
    GitReport {
        on_path: facts.on_path,
        version: line.and_then(version_number),
        supported,
        minimum,
    }
}

#[cfg(test)]
mod tests {
    use super::{parse_git_version, Catalog, Doctor, DoctorRequest, DoctorResponse};
    use crate::application::error::{AppError, ErrorCode, StorageError};
    use crate::application::search::fixtures::{index, INDEXED_AT};
    use crate::application::testkit::{
        FakeDiagnostics, FixedClock, InMemoryStore, StampStoredVersions,
    };
    use crate::domain::index_versions::IndexVersions;
    use crate::domain::indexed_at::IndexedAt;
    use crate::domain::source_id::SourceId;

    fn clock() -> FixedClock {
        FixedClock::new(IndexedAt::from_epoch_seconds(INDEXED_AT + 600))
    }

    fn indexed(store: &InMemoryStore) -> SourceId {
        index(
            store,
            "https://github.com/o/r",
            "docs",
            &[("a.rs", "fn a() {}"), ("b.rs", "fn b() {}")],
        )
    }

    fn run(
        store: &InMemoryStore,
        diagnostics: &FakeDiagnostics,
        request: DoctorRequest,
    ) -> Result<DoctorResponse, AppError> {
        let clock = clock();
        Doctor::new(
            &clock,
            diagnostics,
            Ok(Catalog {
                sources: store,
                chunks: store,
            }),
        )
        .execute(&request)
    }

    fn healthy() -> (InMemoryStore, SourceId, FakeDiagnostics) {
        let store = InMemoryStore::new();
        let id = indexed(&store);
        let diagnostics = FakeDiagnostics::new().with_checkout(&id);
        (store, id, diagnostics)
    }

    #[test]
    fn a_healthy_install_reports_its_sources_with_no_warnings() {
        let (store, _, diagnostics) = healthy();
        let diagnostics = diagnostics.with_database_sizes(4096, 1024);

        let report = run(&store, &diagnostics, DoctorRequest::default()).unwrap();

        assert!(report.ok, "{:?}", report.warnings);
        assert_eq!(report.data_dir, "/data/rag");
        assert_eq!(
            (report.database.size_bytes, report.database.wal_bytes),
            (4096, 1024)
        );
        assert_eq!(report.database.integrity, None, "only on request");
        let [source] = &report.sources[..] else {
            panic!("one source")
        };
        assert_eq!(source.summary.name, "docs");
        assert_eq!(source.summary.chunks, 2);
        assert_eq!(source.summary.age_seconds, Some(600));
        assert_eq!(source.summary.status, "ready");
        assert_eq!(
            (
                source.checkout,
                source.stale_versions,
                source.operation_lock
            ),
            ("present", false, "free")
        );
        assert_eq!(source.lock_operation, None);
    }

    #[test]
    fn a_data_directory_without_a_database_is_fine() {
        let diagnostics = FakeDiagnostics::new().without_database();
        let clock = clock();
        let report = Doctor::new(&clock, &diagnostics, Err(StorageError::IndexMissing))
            .execute(&DoctorRequest::default())
            .unwrap();
        assert!(report.ok, "{:?}", report.warnings);
        assert!(!report.database.exists);
        assert!(report.sources.is_empty());
    }

    #[test]
    fn a_database_that_cannot_be_opened_is_a_finding_not_a_failure() {
        let diagnostics = FakeDiagnostics::new();
        let clock = clock();
        let report = Doctor::new(
            &clock,
            &diagnostics,
            Err(StorageError::Failed("unsupported schema".into())),
        )
        .execute(&DoctorRequest::default())
        .unwrap();
        assert!(!report.ok);
        assert!(report
            .database
            .error
            .unwrap()
            .contains("unsupported schema"));
        assert!(
            report.warnings[0].contains("cannot be read"),
            "{:?}",
            report.warnings
        );
    }

    #[test]
    fn an_unreadable_data_directory_is_the_one_failure() {
        let (store, _, diagnostics) = healthy();
        let error = run(
            &store,
            &diagnostics.unreadable("access denied"),
            DoctorRequest::default(),
        )
        .unwrap_err();
        assert_eq!(error.code(), ErrorCode::Generic);
        assert!(error.to_string().contains("access denied"), "{error}");
    }

    #[test]
    fn a_missing_clone_is_reported_with_what_to_do() {
        let store = InMemoryStore::new();
        indexed(&store);

        let report = run(&store, &FakeDiagnostics::new(), DoctorRequest::default()).unwrap();

        assert_eq!(report.sources[0].checkout, "missing");
        assert!(!report.ok);
        let warning = &report.warnings[0];
        assert!(
            warning.contains("docs") && warning.contains("local copy is missing"),
            "{warning}"
        );
        assert!(warning.contains("ask the user"), "{warning}");
    }

    #[test]
    fn a_stale_index_is_reported_with_the_reindex_command_for_the_user() {
        let (store, id, diagnostics) = healthy();
        let mut old = IndexVersions::current();
        old.chunker = 0;
        store.stamp_stored_versions(&id, old);

        let report = run(&store, &diagnostics, DoctorRequest::default()).unwrap();

        assert!(report.sources[0].stale_versions);
        assert_eq!(
            report.warnings,
            ["source docs: its index was made by an older version; ask the user to run `rag reindex docs`"]
        );
    }

    #[test]
    fn a_held_operation_lock_is_shown_with_its_operation() {
        let (store, id, diagnostics) = healthy();
        let diagnostics = diagnostics.with_lock_held(&id, Some("update"));

        let report = run(&store, &diagnostics, DoctorRequest::default()).unwrap();

        assert_eq!(report.sources[0].operation_lock, "held");
        assert_eq!(report.sources[0].lock_operation.as_deref(), Some("update"));
        assert!(
            report.ok,
            "a lock being held is normal while a command runs"
        );
    }

    #[test]
    fn a_failed_source_shows_its_last_error_as_a_warning() {
        use crate::application::ports::SourceRepository;
        let (store, id, diagnostics) = healthy();
        let mut source = store.get(&id).unwrap().unwrap();
        source.record_failure(
            "network_or_auth",
            "git: operation timed out",
            IndexedAt::from_epoch_seconds(INDEXED_AT + 60),
        );
        store.save(&source).unwrap();

        let report = run(&store, &diagnostics, DoctorRequest::default()).unwrap();

        let error = report.sources[0].summary.last_error.as_ref().unwrap();
        assert_eq!(error.code, "network_or_auth");
        assert!(
            report.warnings[0].contains("git: operation timed out"),
            "{:?}",
            report.warnings
        );
    }

    #[test]
    fn leftovers_are_counted_and_the_unregistered_clones_are_the_ones_no_source_owns() {
        let (store, _, diagnostics) = healthy();
        let diagnostics = diagnostics
            .with_tmp_entries(3)
            .with_repos_dir("0123456789abcdef");

        let report = run(&store, &diagnostics, DoctorRequest::default()).unwrap();

        assert_eq!(
            (
                report.leftovers.tmp_entries,
                report.leftovers.unregistered_checkouts
            ),
            (3, 1)
        );
        assert!(
            report.warnings[0].contains("3 leftover"),
            "{:?}",
            report.warnings
        );
    }

    #[test]
    fn git_that_is_missing_or_too_old_or_unreadable_is_a_warning() {
        let (store, _, healthy_diagnostics) = healthy();
        let cases = [
            (
                healthy_diagnostics.clone().without_git(),
                "not found on PATH",
                false,
                None,
            ),
            (
                healthy_diagnostics
                    .clone()
                    .with_git_version_text("git version 2.30.2"),
                "older than 2.38",
                false,
                Some("2.30.2"),
            ),
            (
                healthy_diagnostics.clone().with_git_version_text("what"),
                "could not be read",
                false,
                None,
            ),
            (
                healthy_diagnostics
                    .clone()
                    .with_git_version_text("git version 2.38.0"),
                "",
                true,
                Some("2.38.0"),
            ),
        ];
        for (diagnostics, warning, supported, version) in cases {
            let report = run(&store, &diagnostics, DoctorRequest::default()).unwrap();
            assert_eq!(report.git.supported, supported, "{warning}");
            assert_eq!(report.git.version.as_deref(), version, "{warning}");
            assert_eq!(report.git.minimum, "2.38");
            if warning.is_empty() {
                assert!(report.ok, "{:?}", report.warnings);
            } else {
                assert!(
                    report.warnings[0].contains(warning),
                    "{:?}",
                    report.warnings
                );
            }
        }
    }

    #[test]
    fn the_integrity_check_runs_only_on_request_and_a_bad_result_is_a_warning() {
        let (store, _, diagnostics) = healthy();
        let diagnostics = diagnostics.with_integrity_report("row 3 missing from index");

        let quick = run(&store, &diagnostics, DoctorRequest::default()).unwrap();
        assert_eq!(quick.database.integrity, None);
        assert!(quick.ok);

        let checked = run(&store, &diagnostics, DoctorRequest { integrity: true }).unwrap();
        assert_eq!(
            checked.database.integrity.as_deref(),
            Some("row 3 missing from index")
        );
        assert!(
            checked.warnings[0].contains("integrity check"),
            "{:?}",
            checked.warnings
        );

        let good = run(
            &store,
            &FakeDiagnostics::new().with_checkout(&indexed_id()),
            DoctorRequest { integrity: true },
        )
        .unwrap();
        assert_eq!(good.database.integrity.as_deref(), Some("ok"));
        assert!(good.ok);
    }

    fn indexed_id() -> SourceId {
        let store = InMemoryStore::new();
        indexed(&store)
    }

    #[test]
    fn git_versions_parse_from_the_first_two_numbers() {
        assert_eq!(
            parse_git_version("git version 2.55.0.windows.5"),
            Some((2, 55))
        );
        assert_eq!(parse_git_version("git version 2.38.1\n"), Some((2, 38)));
        assert_eq!(parse_git_version("git version 10.2"), Some((10, 2)));
        assert_eq!(parse_git_version("git version"), None);
        assert_eq!(parse_git_version("hg version 1.2"), None);
        assert_eq!(parse_git_version("git version x.y"), None);
    }

    #[test]
    fn the_report_serializes_in_a_stable_order_with_the_source_fields_flattened() {
        let (store, _, diagnostics) = healthy();
        let report = run(&store, &diagnostics, DoctorRequest { integrity: true }).unwrap();
        let json = serde_json::to_string(&report).unwrap();
        assert_eq!(
            json,
            concat!(
                r#"{"schema_version":1,"ok":true,"data_dir":"/data/rag","#,
                r#""database":{"exists":true,"size_bytes":8192,"wal_bytes":0,"integrity":"ok"},"#,
                r#""git":{"on_path":true,"version":"2.55.0","supported":true,"minimum":"2.38"},"#,
                r#""sources":[{"name":"docs","repo":"github.com/o/r","status":"ready","commit":"abcdef0","#,
                r#""indexed_at":"2023-11-14T22:13:20Z","age_seconds":600,"chunks":2,"#,
                r#""checkout":"present","stale_versions":false,"operation_lock":"free"}],"#,
                r#""leftovers":{"tmp_entries":0,"unregistered_checkouts":0},"warnings":[]}"#
            )
        );
    }
}
