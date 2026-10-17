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
}

impl ErrorCode {
    /// Every code, in exit-code order.
    pub const ALL: [ErrorCode; 7] = [
        ErrorCode::Generic,
        ErrorCode::Usage,
        ErrorCode::NotFound,
        ErrorCode::NetworkOrAuth,
        ErrorCode::IndexMissing,
        ErrorCode::IndexLocked,
        ErrorCode::ReindexRequired,
    ];

    /// The process exit code: 1 generic, 2 usage, 3 not found, 4 auth/network,
    /// 5 index missing, 6 index locked, 7 reindex required (0 is success).
    pub fn exit_code(&self) -> u8 {
        match self {
            Self::Generic => 1,
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
        }
    }

    /// The code whose `as_str` is `text`.
    pub fn parse(text: &str) -> Option<ErrorCode> {
        Self::ALL.into_iter().find(|code| code.as_str() == text)
    }

    /// What the caller can do about it, when that is the same every time.
    pub fn default_hint(&self) -> Option<&'static str> {
        match self {
            Self::IndexMissing => Some("run `rag add <url|path>` to index a repository"),
            Self::IndexLocked => Some("another rag process is writing; wait a moment and retry"),
            Self::ReindexRequired => Some("run `rag reindex` to rebuild the index"),
            Self::Generic | Self::Usage | Self::NotFound | Self::NetworkOrAuth => None,
        }
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
    /// The repository is in use by another operation; retry later.
    Busy,
    /// A blob, or a batch of blobs, is larger than the adapter will read into
    /// memory (see `ports::MAX_BLOB_BYTES`). Retrying cannot help.
    TooLarge(Scrubbed),
    /// Anything else.
    Failed(Scrubbed),
}

/// What a use case returns on failure.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AppError {
    Storage(StorageError),
    Git(GitError),
    /// The named source, file, or lines do not exist.
    NotFound(Scrubbed),
    /// The request itself is invalid.
    Usage(Scrubbed),
}

impl AppError {
    pub fn code(&self) -> ErrorCode {
        match self {
            Self::Storage(StorageError::IndexMissing) => ErrorCode::IndexMissing,
            Self::Storage(
                StorageError::Busy | StorageError::LockedBy(_) | StorageError::Conflict,
            )
            | Self::Git(GitError::Busy) => ErrorCode::IndexLocked,
            Self::Storage(StorageError::ReindexRequired) => ErrorCode::ReindexRequired,
            Self::Git(GitError::NotFound(_)) | Self::NotFound(_) => ErrorCode::NotFound,
            Self::Git(GitError::NetworkOrAuth(_) | GitError::Timeout) => ErrorCode::NetworkOrAuth,
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
            Self::Usage(message) => message.to_string(),
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
    use super::{AppError, ErrorCode, GitError, Scrubbed, StorageError};

    #[test]
    fn exit_codes_are_one_to_seven_in_order() {
        let codes: Vec<u8> = ErrorCode::ALL.iter().map(ErrorCode::exit_code).collect();
        assert_eq!(codes, [1, 2, 3, 4, 5, 6, 7]);
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
                "reindex_required"
            ]
        );
    }

    #[test]
    fn index_errors_carry_a_hint_naming_the_command_to_run() {
        assert!(ErrorCode::IndexMissing
            .default_hint()
            .unwrap()
            .contains("rag add"));
        assert!(ErrorCode::ReindexRequired
            .default_hint()
            .unwrap()
            .contains("rag reindex"));
        assert!(ErrorCode::IndexLocked
            .default_hint()
            .unwrap()
            .contains("retry"));
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
