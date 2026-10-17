//! Application errors: one enum per port family, the stable `ErrorCode`, and the
//! `AppError` use cases return. Adapters translate their own errors (SQLite,
//! subprocess, I/O) into these at the boundary. `thiserror` is not available in
//! this crate, so `Display` and `Error` are written by hand.

use std::fmt;

use crate::domain::git_location::scrub_credentials;
use crate::domain::untrusted_text::sanitize_untrusted;

/// Longest message a `Scrubbed` keeps, in characters (an ellipsis included).
pub const MAX_MESSAGE_CHARS: usize = 500;

/// Stable error codes: the JSON `error.code` string and the process exit code.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ErrorCode {
    Generic,
    Usage,
    NotFound,
    NetworkOrAuth,
    IndexMissing,
    IndexLocked,
    ReindexRequired,
    /// The `--deadline` of an indexing command ran out.
    DeadlineExceeded,
}

impl ErrorCode {
    /// Every code, in exit-code order (the deadline shares the generic exit code, so
    /// it comes last).
    pub const ALL: [ErrorCode; 8] = [
        ErrorCode::Generic,
        ErrorCode::Usage,
        ErrorCode::NotFound,
        ErrorCode::NetworkOrAuth,
        ErrorCode::IndexMissing,
        ErrorCode::IndexLocked,
        ErrorCode::ReindexRequired,
        ErrorCode::DeadlineExceeded,
    ];

    /// The process exit code: 1 generic, 2 usage, 3 not found, 4 auth/network,
    /// 5 index missing, 6 index locked, 7 reindex required (0 is success). A deadline
    /// is neither a network problem nor a lock: it exits 1 under its own code string.
    pub fn exit_code(&self) -> u8 {
        match self {
            Self::Generic | Self::DeadlineExceeded => 1,
            Self::Usage => 2,
            Self::NotFound => 3,
            Self::NetworkOrAuth => 4,
            Self::IndexMissing => 5,
            Self::IndexLocked => 6,
            Self::ReindexRequired => 7,
        }
    }

    /// The stable string in `{"error":{"code":...}}`.
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Generic => "error",
            Self::Usage => "usage",
            Self::NotFound => "not_found",
            Self::NetworkOrAuth => "network_or_auth",
            Self::IndexMissing => "index_missing",
            Self::IndexLocked => "index_locked",
            Self::ReindexRequired => "reindex_required",
            Self::DeadlineExceeded => "deadline_exceeded",
        }
    }

    /// The code whose `as_str` is `text`.
    pub fn parse(text: &str) -> Option<ErrorCode> {
        Self::ALL.into_iter().find(|code| code.as_str() == text)
    }
}

/// Message text made safe to show, at construction: credentials removed
/// (`scrub_credentials`), control and invisible characters replaced
/// (`sanitize_untrusted`; line breaks and tabs become spaces, so a message is one
/// line), and the length capped at `MAX_MESSAGE_CHARS`. The only ways to build one
/// are `new` and `From`, so no error can carry a password, token, or query secret
/// that a subprocess or an adapter happened to quote, nor text that writes to a
/// terminal.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Scrubbed(String);

impl Scrubbed {
    pub fn new(text: &str) -> Self {
        // Credentials go first: a cut made by the cap could otherwise split a URL and
        // leave a secret that no scrubber recognises.
        let scrubbed = scrub_credentials(text).replace(['\n', '\r', '\t'], " ");
        let clean = sanitize_untrusted(&scrubbed);
        Self(cap(clean))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

fn cap(text: String) -> String {
    if text.chars().count() <= MAX_MESSAGE_CHARS {
        return text;
    }
    let mut capped: String = text.chars().take(MAX_MESSAGE_CHARS - 3).collect();
    capped.push_str("...");
    capped
}

impl From<&str> for Scrubbed {
    fn from(text: &str) -> Self {
        Self::new(text)
    }
}

impl From<String> for Scrubbed {
    fn from(text: String) -> Self {
        Self::new(&text)
    }
}

impl fmt::Display for Scrubbed {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// Failures of the stores: sources, chunks, and search.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StorageError {
    /// No index exists yet.
    IndexMissing,
    /// Another writer holds the index or the source's operation lock.
    Busy,
    /// The index was built with other tokenizer or chunker versions.
    ReindexRequired,
    /// The stored index is not at the commit the writer started from
    /// (`commit_index` with a stale `expected_base`).
    Conflict,
    /// The operation lock of the source stayed held; the text names the operation in
    /// progress. Like `Busy`, a retry later can succeed.
    LockedBy(Scrubbed),
    /// Another source already has this name (compared case-insensitively).
    NameTaken(Scrubbed),
    /// Anything else; the message is safe to show.
    Failed(Scrubbed),
}

impl StorageError {
    /// The lock of a source is held by `operation` (`add`, `update`).
    pub fn locked_by(operation: &str) -> Self {
        Self::LockedBy(
            format!("another rag process is running `{operation}` on this source").into(),
        )
    }
}

/// Failures of the git client.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GitError {
    /// Repository, ref, commit, or blob does not exist.
    NotFound(Scrubbed),
    /// Authentication or network failure.
    NetworkOrAuth(Scrubbed),
    /// A git operation exceeded its time limit.
    Timeout,
    /// The overall `--deadline` of the command ran out while git was working.
    DeadlineExceeded,
    /// The remote refused the credentials, or asked for some that git cannot give
    /// non-interactively. A retry cannot help until the user signs in.
    AuthRequired(Scrubbed),
    /// The repository is in use by another operation; retry later.
    Busy,
    /// A blob, or a batch of blobs, is larger than the adapter will read into
    /// memory (see `ports::MAX_BLOB_BYTES`). Retrying cannot help.
    TooLarge(Scrubbed),
    /// Anything else.
    Failed(Scrubbed),
}

/// Most names `UnknownSource` lists.
pub const MAX_VALID_NAMES: usize = 20;

/// A name or slug that matches no registered source, with the names that would.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnknownSource {
    wanted: Scrubbed,
    valid: Vec<Scrubbed>,
    /// Valid names beyond the `MAX_VALID_NAMES` kept.
    more: usize,
}

impl UnknownSource {
    /// `wanted` matched nothing; `valid` are the names of the registered sources (the
    /// first `MAX_VALID_NAMES` are kept).
    pub fn new(wanted: &str, valid: impl IntoIterator<Item = String>) -> Self {
        let all: Vec<String> = valid.into_iter().collect();
        let kept = all.len().min(MAX_VALID_NAMES);
        Self {
            wanted: Scrubbed::new(wanted),
            valid: all[..kept].iter().map(|name| Scrubbed::new(name)).collect(),
            more: all.len() - kept,
        }
    }
}

impl fmt::Display for UnknownSource {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "no source named {:?}; run `rag list` to see the registered ones",
            self.wanted.as_str()
        )?;
        if self.valid.is_empty() {
            return f.write_str(" (no sources are registered)");
        }
        let names: Vec<&str> = self.valid.iter().map(Scrubbed::as_str).collect();
        write!(f, " (valid: {}", names.join(", "))?;
        if self.more > 0 {
            write!(f, ", and {} more", self.more)?;
        }
        f.write_str(")")
    }
}

/// What a use case returns on failure.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AppError {
    Storage(StorageError),
    Git(GitError),
    /// The named source, file, or lines do not exist.
    NotFound(Scrubbed),
    /// The name or slug given matches no registered source.
    UnknownSource(UnknownSource),
    /// A source the command touches was indexed by other rules and needs
    /// `rag reindex` first.
    StaleIndex {
        /// The (first) stale source, by name.
        source: Scrubbed,
        /// How many more stale sources the command touches.
        also_stale: usize,
    },
    /// The request itself is invalid.
    Usage(Scrubbed),
    /// The overall `--deadline` ran out between two steps of an indexing command.
    /// It is not recorded as a failure of a source: the old index is intact.
    DeadlineExceeded,
    /// The chunk asked for by id is not stored (an update replaced it, or its source
    /// was removed).
    ChunkGone,
}

/// A word safe to paste into a shell as one argument: quoted unless it is plain.
pub(crate) fn shell_word(text: &str) -> String {
    let plain = !text.is_empty()
        && text
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "._/-:".contains(c));
    if plain {
        text.to_owned()
    } else {
        format!("{text:?}")
    }
}

/// Shown for the errors that mean "someone else is working": the command did not do
/// its work.
const LOCKED_HINT: &str =
    "your command was not done and nothing was changed by it; wait about 5 seconds, then retry";
/// Shown when git's own lock files are taken, possibly by a git that was killed.
const GIT_LOCK_HINT: &str = "another git process or a killed one holds a lock; retry in a minute";
/// Shown when the time limit of the command ran out.
const DEADLINE_HINT: &str = "re-run the same command to resume; the fetched data is kept";
/// What a deadline error says.
const DEADLINE_MESSAGE: &str =
    "the time limit of this command (--deadline) ran out before it finished";
/// Shown for an authentication failure. Addressed to the user: signing in changes
/// their credentials, which an agent must not do.
const AUTH_HINT: &str = "authenticate once in a terminal (for example `git ls-remote <url>` or `gh auth login`), then retry; never put tokens in a URL";
/// Shown for network and authentication failures.
const NETWORK_HINT: &str = "check the network and the credentials; git must authenticate non-interactively (a configured credential helper or ssh key); if it persists, ask the user";

impl AppError {
    /// What to do about the error, when something general can be said: always a
    /// string or none, and never an instruction the agent would run to change the
    /// index of the user (those say "ask the user to run ...").
    pub fn hint(&self) -> Option<String> {
        match self {
            Self::Storage(StorageError::IndexMissing) => {
                Some("ask the user to run `rag add <url|path>` to index a repository".to_owned())
            }
            Self::Storage(StorageError::ReindexRequired) => {
                Some("ask the user to run `rag reindex` to rebuild the index".to_owned())
            }
            Self::StaleIndex {
                source,
                also_stale: 0,
            } => Some(format!(
                "ask the user to run `rag reindex {}`",
                shell_word(source.as_str())
            )),
            Self::StaleIndex { .. } => {
                Some("ask the user to run `rag reindex` to rebuild every stale source".to_owned())
            }
            Self::Storage(
                StorageError::Busy | StorageError::LockedBy(_) | StorageError::Conflict,
            ) => Some(LOCKED_HINT.to_owned()),
            Self::Git(GitError::Busy) => Some(GIT_LOCK_HINT.to_owned()),
            Self::Git(GitError::DeadlineExceeded) | Self::DeadlineExceeded => {
                Some(DEADLINE_HINT.to_owned())
            }
            Self::Git(GitError::AuthRequired(_)) => Some(AUTH_HINT.to_owned()),
            Self::ChunkGone => Some("run `rag search` again".to_owned()),
            Self::Git(GitError::NetworkOrAuth(_) | GitError::Timeout) => {
                Some(NETWORK_HINT.to_owned())
            }
            Self::UnknownSource(_) => {
                Some("use one of the names in details.valid_names, or run `rag list`".to_owned())
            }
            Self::Storage(StorageError::NameTaken(_) | StorageError::Failed(_))
            | Self::Git(GitError::NotFound(_) | GitError::TooLarge(_) | GitError::Failed(_))
            | Self::NotFound(_)
            | Self::Usage(_) => None,
        }
    }

    /// The names of the valid sources, for an unknown-source error.
    pub fn valid_names(&self) -> Option<Vec<&str>> {
        match self {
            Self::UnknownSource(unknown) => {
                Some(unknown.valid.iter().map(Scrubbed::as_str).collect())
            }
            _ => None,
        }
    }

    /// The stale source, for a reindex-required error that names one.
    pub fn stale_source(&self) -> Option<&str> {
        match self {
            Self::StaleIndex { source, .. } => Some(source.as_str()),
            _ => None,
        }
    }

    /// Whether the overall `--deadline` ran out. Not a failure of any source: the
    /// stored index and the source row are as they were, and a re-run resumes.
    pub fn is_deadline(&self) -> bool {
        matches!(
            self,
            Self::DeadlineExceeded | Self::Git(GitError::DeadlineExceeded)
        )
    }

    pub fn code(&self) -> ErrorCode {
        match self {
            Self::Storage(StorageError::IndexMissing) => ErrorCode::IndexMissing,
            Self::Storage(
                StorageError::Busy | StorageError::LockedBy(_) | StorageError::Conflict,
            )
            | Self::Git(GitError::Busy) => ErrorCode::IndexLocked,
            Self::Storage(StorageError::ReindexRequired) | Self::StaleIndex { .. } => {
                ErrorCode::ReindexRequired
            }
            Self::Git(GitError::NotFound(_))
            | Self::NotFound(_)
            | Self::UnknownSource(_)
            | Self::ChunkGone => ErrorCode::NotFound,
            Self::Git(
                GitError::NetworkOrAuth(_) | GitError::Timeout | GitError::AuthRequired(_),
            ) => ErrorCode::NetworkOrAuth,
            Self::DeadlineExceeded | Self::Git(GitError::DeadlineExceeded) => {
                ErrorCode::DeadlineExceeded
            }
            Self::Usage(_) | Self::Storage(StorageError::NameTaken(_)) => ErrorCode::Usage,
            Self::Storage(StorageError::Failed(_))
            | Self::Git(GitError::Failed(_) | GitError::TooLarge(_)) => ErrorCode::Generic,
        }
    }
}

impl fmt::Display for StorageError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::IndexMissing => f.write_str("no index exists yet"),
            Self::Busy => f.write_str("the index is locked by another process"),
            Self::LockedBy(operation) => write!(f, "{operation}; wait for it to finish and retry"),
            Self::ReindexRequired => f.write_str("the index was built by an older version"),
            Self::Conflict => {
                f.write_str("the index changed while this command ran; retry the command")
            }
            Self::NameTaken(name) => {
                write!(
                    f,
                    "the name {:?} is already used by a source",
                    name.as_str()
                )
            }
            Self::Failed(message) => write!(f, "storage failure: {message}"),
        }
    }
}

impl fmt::Display for GitError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotFound(what) => write!(f, "git: not found: {what}"),
            Self::NetworkOrAuth(message) => {
                write!(f, "git: network or authentication failure: {message}")
            }
            Self::Timeout => f.write_str("git: operation timed out"),
            Self::DeadlineExceeded => f.write_str(DEADLINE_MESSAGE),
            Self::AuthRequired(message) => {
                write!(f, "git: authentication failure: {message}")
            }
            Self::Busy => f.write_str("git: the repository is busy; retry shortly"),
            Self::TooLarge(what) => write!(f, "git: too large to read: {what}"),
            Self::Failed(message) => write!(f, "git failure: {message}"),
        }
    }
}

impl fmt::Display for AppError {
    /// Whatever the variant carries, the shown text goes through
    /// `scrub_credentials` once more, so no future variant can leak.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let text = match self {
            Self::Storage(error) => error.to_string(),
            Self::Git(error) => error.to_string(),
            Self::NotFound(what) => format!("not found: {what}"),
            Self::UnknownSource(unknown) => unknown.to_string(),
            Self::StaleIndex { source, also_stale } => {
                let others = match also_stale {
                    0 => String::new(),
                    n => format!(" ({n} other stale sources)"),
                };
                format!(
                    "the index of source {:?} was built by an older version{others}",
                    source.as_str()
                )
            }
            Self::Usage(message) => message.to_string(),
            Self::DeadlineExceeded => DEADLINE_MESSAGE.to_owned(),
            Self::ChunkGone => "that chunk is not in the index; an update may have replaced it, so run `rag search` again".to_owned(),
        };
        f.write_str(&scrub_credentials(&text))
    }
}

impl std::error::Error for StorageError {}
impl std::error::Error for GitError {}
impl std::error::Error for AppError {}

impl From<StorageError> for AppError {
    fn from(error: StorageError) -> Self {
        Self::Storage(error)
    }
}

impl From<GitError> for AppError {
    fn from(error: GitError) -> Self {
        Self::Git(error)
    }
}
#[cfg(test)]
mod tests {
    use super::{AppError, ErrorCode, GitError, Scrubbed, StorageError, UnknownSource};

    #[test]
    fn exit_codes_are_one_to_seven_in_order_then_the_deadline_shares_the_generic_one() {
        let codes: Vec<u8> = ErrorCode::ALL.iter().map(ErrorCode::exit_code).collect();
        assert_eq!(codes, [1, 2, 3, 4, 5, 6, 7, 1]);
    }

    #[test]
    fn a_deadline_is_its_own_code_with_exit_one_and_a_resume_hint() {
        for error in [
            AppError::DeadlineExceeded,
            AppError::Git(GitError::DeadlineExceeded),
        ] {
            assert_eq!(error.code(), ErrorCode::DeadlineExceeded);
            assert_eq!(error.code().as_str(), "deadline_exceeded");
            assert_eq!(error.code().exit_code(), 1);
            assert_eq!(
                error.hint().unwrap(),
                "re-run the same command to resume; the fetched data is kept"
            );
            assert!(error.to_string().contains("--deadline"), "{error}");
        }
    }

    #[test]
    fn a_git_lock_says_another_git_process_or_a_killed_one_holds_it() {
        let error = AppError::Git(GitError::Busy);
        assert_eq!(error.code().exit_code(), 6);
        assert_eq!(
            error.hint().unwrap(),
            "another git process or a killed one holds a lock; retry in a minute"
        );
    }

    #[test]
    fn an_authentication_failure_tells_the_user_to_sign_in_once_and_never_to_use_a_token_url() {
        let error = AppError::Git(GitError::AuthRequired("denied".into()));
        assert_eq!(error.code(), ErrorCode::NetworkOrAuth);
        assert_eq!(error.code().exit_code(), 4);
        let hint = error.hint().unwrap();
        assert_eq!(
            hint,
            "authenticate once in a terminal (for example `git ls-remote <url>` or `gh auth login`), then retry; never put tokens in a URL"
        );
        assert!(error.to_string().contains("denied"), "{error}");
    }

    #[test]
    fn a_vanished_chunk_hints_to_search_again() {
        let error = AppError::ChunkGone;
        assert_eq!(error.code(), ErrorCode::NotFound);
        assert_eq!(error.hint().unwrap(), "run `rag search` again");
        assert!(error.to_string().contains("rag search"), "{error}");
    }

    #[test]
    fn code_strings_parse_back_to_their_codes() {
        for code in ErrorCode::ALL {
            assert_eq!(ErrorCode::parse(code.as_str()), Some(code));
        }
        assert_eq!(ErrorCode::parse("skipped_busy"), None);
    }

    #[test]
    fn code_strings_are_stable() {
        let strings: Vec<&str> = ErrorCode::ALL.iter().map(ErrorCode::as_str).collect();
        assert_eq!(
            strings,
            [
                "error",
                "usage",
                "not_found",
                "network_or_auth",
                "index_missing",
                "index_locked",
                "reindex_required",
                "deadline_exceeded"
            ]
        );
    }

    #[test]
    fn missing_and_stale_index_hints_address_the_user_not_the_agent() {
        let missing = AppError::from(StorageError::IndexMissing).hint().unwrap();
        assert_eq!(
            missing,
            "ask the user to run `rag add <url|path>` to index a repository"
        );
        let stale = AppError::StaleIndex {
            source: "docs".into(),
            also_stale: 0,
        };
        assert_eq!(
            stale.hint().unwrap(),
            "ask the user to run `rag reindex docs`"
        );
        for hint in [missing, stale.hint().unwrap()] {
            assert!(hint.starts_with("ask the user"), "{hint}");
        }
    }

    #[test]
    fn a_stale_index_names_its_source_and_several_stale_sources_say_so() {
        let one = AppError::StaleIndex {
            source: "my docs".into(),
            also_stale: 0,
        };
        assert_eq!(one.code(), ErrorCode::ReindexRequired);
        assert_eq!(one.stale_source(), Some("my docs"));
        assert_eq!(
            one.hint().unwrap(),
            "ask the user to run `rag reindex \"my docs\"`"
        );
        assert!(one.to_string().contains("\"my docs\""), "{one}");
        let many = AppError::StaleIndex {
            source: "a".into(),
            also_stale: 2,
        };
        assert!(many.to_string().contains("2 other"), "{many}");
        assert_eq!(
            many.hint().unwrap(),
            "ask the user to run `rag reindex` to rebuild every stale source"
        );
    }

    #[test]
    fn a_raw_stale_storage_error_still_has_a_user_addressed_hint() {
        let hint = AppError::from(StorageError::ReindexRequired)
            .hint()
            .unwrap();
        assert!(
            hint.starts_with("ask the user to run `rag reindex`"),
            "{hint}"
        );
    }

    #[test]
    fn network_failures_hint_at_credentials_and_the_user() {
        for error in [
            AppError::Git(GitError::NetworkOrAuth("x".into())),
            AppError::Git(GitError::Timeout),
        ] {
            let hint = error.hint().unwrap();
            assert!(
                hint.contains("network") && hint.contains("credentials"),
                "{hint}"
            );
            assert!(hint.contains("non-interactively"), "{hint}");
            assert!(hint.contains("ask the user"), "{hint}");
        }
    }

    #[test]
    fn a_locked_index_says_the_work_was_not_done_and_to_wait_about_five_seconds() {
        for error in [
            AppError::Storage(StorageError::Busy),
            AppError::Storage(StorageError::Conflict),
            AppError::Storage(StorageError::locked_by("update")),
        ] {
            let hint = error.hint().unwrap();
            assert!(hint.contains("was not done"), "{hint}");
            assert!(hint.contains("about 5 seconds"), "{hint}");
        }
    }

    #[test]
    fn errors_with_nothing_general_to_suggest_have_no_hint() {
        for error in [
            AppError::Usage("x".into()),
            AppError::NotFound("x".into()),
            AppError::Storage(StorageError::Failed("x".into())),
            AppError::Git(GitError::Failed("x".into())),
        ] {
            assert_eq!(error.hint(), None, "{error}");
        }
    }

    #[test]
    fn an_unknown_source_says_run_rag_list_and_names_the_valid_ones() {
        let error = AppError::UnknownSource(UnknownSource::new(
            "nope",
            ["docs".to_owned(), "other".to_owned()],
        ));
        assert_eq!(error.code(), ErrorCode::NotFound);
        assert_eq!(
            error.to_string(),
            "no source named \"nope\"; run `rag list` to see the registered ones (valid: docs, other)"
        );
        assert_eq!(error.valid_names(), Some(vec!["docs", "other"]));
        assert_eq!(
            error.hint().unwrap(),
            "use one of the names in details.valid_names, or run `rag list`"
        );
    }

    #[test]
    fn at_most_twenty_valid_names_are_listed() {
        let names: Vec<String> = (0..30).map(|i| format!("s{i:02}")).collect();
        let error = AppError::UnknownSource(UnknownSource::new("nope", names));
        let listed = error.valid_names().unwrap();
        assert_eq!(listed.len(), 20);
        assert_eq!(listed[19], "s19");
        assert!(error.to_string().contains("and 10 more"), "{error}");
        assert!(!error.to_string().contains("s20"), "{error}");
    }

    #[test]
    fn an_unknown_source_with_nothing_registered_says_so() {
        let error = AppError::UnknownSource(UnknownSource::new("nope", Vec::new()));
        assert!(
            error.to_string().contains("no sources are registered"),
            "{error}"
        );
        assert_eq!(error.valid_names(), Some(Vec::new()));
    }

    #[test]
    fn unknown_source_text_is_scrubbed_like_every_message() {
        let error = AppError::UnknownSource(UnknownSource::new(
            "https://user:tok3n@github.com/o/r\u{1b}[2J",
            ["a\u{202e}b".to_owned()],
        ));
        let shown = error.to_string();
        assert!(
            !shown.contains("tok3n") && !shown.contains('\u{1b}'),
            "{shown}"
        );
        assert!(!shown.contains('\u{202e}'), "{shown}");
        assert!(!error.valid_names().unwrap()[0].contains('\u{202e}'));
    }

    #[test]
    fn storage_errors_map_to_their_codes() {
        let code = |e: StorageError| AppError::from(e).code();
        assert_eq!(code(StorageError::IndexMissing), ErrorCode::IndexMissing);
        assert_eq!(code(StorageError::Busy), ErrorCode::IndexLocked);
        assert_eq!(
            code(StorageError::ReindexRequired),
            ErrorCode::ReindexRequired
        );
        assert_eq!(
            code(StorageError::Conflict),
            ErrorCode::IndexLocked,
            "a stale base is a lost race: retry, like a busy lock"
        );
        assert_eq!(code(StorageError::NameTaken("x".into())), ErrorCode::Usage);
        assert_eq!(code(StorageError::Failed("x".into())), ErrorCode::Generic);
    }

    #[test]
    fn git_errors_map_to_their_codes() {
        let code = |e: GitError| AppError::from(e).code();
        assert_eq!(code(GitError::NotFound("x".into())), ErrorCode::NotFound);
        assert_eq!(
            code(GitError::NetworkOrAuth("x".into())),
            ErrorCode::NetworkOrAuth
        );
        assert_eq!(code(GitError::Timeout), ErrorCode::NetworkOrAuth);
        assert_eq!(code(GitError::Busy), ErrorCode::IndexLocked);
        assert_eq!(code(GitError::Failed("x".into())), ErrorCode::Generic);
        assert_eq!(code(GitError::TooLarge("x".into())), ErrorCode::Generic);
    }

    #[test]
    fn a_too_large_blob_is_reported_with_its_own_message() {
        let shown = GitError::TooLarge("blob abc is 3 MiB".into()).to_string();
        assert!(
            shown.contains("too large") && shown.contains("blob abc"),
            "{shown}"
        );
    }

    #[test]
    fn other_errors_map_to_their_codes() {
        assert_eq!(AppError::NotFound("x".into()).code(), ErrorCode::NotFound);
        assert_eq!(AppError::Usage("x".into()).code(), ErrorCode::Usage);
    }

    #[test]
    fn messages_include_the_cause() {
        assert!(AppError::from(GitError::Failed("boom".into()))
            .to_string()
            .contains("boom"));
        assert!(AppError::NotFound("repo x".into())
            .to_string()
            .contains("repo x"));
        assert!(StorageError::NameTaken("docs".into())
            .to_string()
            .contains("docs"));
    }

    #[test]
    fn conflict_and_busy_messages_tell_the_caller_to_retry() {
        assert!(StorageError::Conflict.to_string().contains("retry"));
        assert!(GitError::Busy.to_string().contains("retry"));
    }

    #[test]
    fn a_conflict_exits_with_six_and_the_locked_code() {
        let error = AppError::from(StorageError::Conflict);
        assert_eq!(error.code().exit_code(), 6);
        assert_eq!(error.code().as_str(), "index_locked");
    }

    #[test]
    fn scrubbed_text_loses_control_and_invisible_characters() {
        let hostile = "a\u{1b}[2Jb\u{7}c\u{202e}d\u{e0041}e\u{9b}f";
        let shown = Scrubbed::from(hostile);
        assert_eq!(shown.as_str(), "a?[2Jb?c?d?e?f");
    }

    #[test]
    fn scrubbed_text_is_one_line() {
        assert_eq!(
            Scrubbed::from("one\ntwo\r\nthree\tfour").as_str(),
            "one two  three four"
        );
    }

    #[test]
    fn scrubbed_text_is_capped_near_500_characters() {
        let long = "x".repeat(2000);
        let shown = Scrubbed::from(long.as_str());
        assert_eq!(shown.as_str().chars().count(), 500);
        assert!(shown.as_str().ends_with("..."), "{}", shown.as_str());
        let short = "y".repeat(500);
        assert_eq!(Scrubbed::from(short.as_str()).as_str(), short);
    }

    #[test]
    fn the_cap_never_splits_a_credential_into_something_unrecognisable() {
        let text = format!(
            "{} https://user:s3cretpassword@github.com/o/r",
            "x".repeat(470)
        );
        let shown = Scrubbed::from(text.as_str());
        assert!(!shown.as_str().contains("s3cr") && !shown.as_str().contains("user:"));
        assert!(shown.as_str().chars().count() <= 500);
    }

    #[test]
    fn locked_by_builds_the_message_for_an_operation() {
        let error = AppError::from(StorageError::locked_by("update"));
        assert_eq!(
            error.to_string(),
            "another rag process is running `update` on this source; wait for it to finish and retry"
        );
        let hostile = AppError::from(StorageError::locked_by("up\u{1b}[2Jdate"));
        assert!(!hostile.to_string().contains('\u{1b}'));
    }

    #[test]
    fn a_held_lock_names_the_operation_and_exits_six() {
        let error = AppError::from(StorageError::LockedBy(
            "another rag process is running `update`".into(),
        ));
        assert_eq!(error.code(), ErrorCode::IndexLocked);
        assert_eq!(error.code().exit_code(), 6);
        assert!(error.to_string().contains("`update`"), "{error}");
        assert!(
            error.to_string().contains("retry") || error.to_string().contains("wait"),
            "{error}"
        );
    }

    const LEAK: &str =
        "failed: https://user:tok3n@github.com/o/r?token=abc123 and ssh://git:pw0rd@h.com/x";

    fn assert_clean(shown: &str) {
        for secret in ["tok3n", "abc123", "pw0rd", "user:"] {
            assert!(!shown.contains(secret), "{secret} leaked in {shown}");
        }
        assert!(shown.contains("github.com/o/r"), "{shown}");
    }

    #[test]
    fn message_payloads_are_scrubbed_when_the_error_is_built() {
        assert_clean(Scrubbed::from(LEAK).as_str());
        assert_clean(Scrubbed::from(LEAK.to_owned()).as_str());
        let GitError::Failed(stored) = GitError::Failed(LEAK.into()) else {
            unreachable!()
        };
        assert_clean(stored.as_str());
        let StorageError::Failed(stored) = StorageError::Failed(LEAK.into()) else {
            unreachable!()
        };
        assert_clean(stored.as_str());
        let AppError::Usage(stored) = AppError::Usage(LEAK.into()) else {
            unreachable!()
        };
        assert_clean(stored.as_str());
    }

    #[test]
    fn app_error_display_scrubs_again_even_if_a_payload_slipped_through() {
        // Only `tests` (inside this module) can build a `Scrubbed` around raw text.
        let raw = || Scrubbed(LEAK.to_owned());
        for error in [
            AppError::Usage(raw()),
            AppError::NotFound(raw()),
            AppError::Storage(StorageError::Failed(raw())),
            AppError::Git(GitError::NetworkOrAuth(raw())),
        ] {
            assert_clean(&error.to_string());
        }
    }

    #[test]
    fn every_app_error_display_is_free_of_credentials() {
        let errors = [
            AppError::Usage(LEAK.into()),
            AppError::NotFound(LEAK.into()),
            AppError::Storage(StorageError::Failed(LEAK.into())),
            AppError::Storage(StorageError::NameTaken(LEAK.into())),
            AppError::Git(GitError::Failed(LEAK.into())),
            AppError::Git(GitError::NotFound(LEAK.into())),
            AppError::Git(GitError::NetworkOrAuth(LEAK.into())),
        ];
        for error in errors {
            assert_clean(&error.to_string());
        }
    }

    #[test]
    fn git_error_messages_never_show_credentials() {
        let leaked = "fatal: unable to access 'https://user:tok3n@github.com/o/r/'";
        for error in [
            GitError::Failed(leaked.into()),
            GitError::NotFound(leaked.into()),
            GitError::NetworkOrAuth(leaked.into()),
        ] {
            let shown = AppError::from(error).to_string();
            assert!(
                !shown.contains("tok3n") && !shown.contains("user:"),
                "{shown}"
            );
            assert!(shown.contains("github.com/o/r"), "{shown}");
        }
    }
}
