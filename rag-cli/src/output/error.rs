//! Error output: the JSON error object and the process exit code for each error.

use rag_core::application::dto::ErrorResponse;
use rag_core::application::error::AppError;

/// Exit code of a successful command, including a search with no results.
pub const EXIT_OK: u8 = 0;

/// The JSON printed on stdout in JSON mode: `{"error":{"code","message","hint"}}`.
pub fn render_error_json(error: &AppError) -> String {
    serde_json::to_string(&ErrorResponse::from_error(error))
        .expect("error responses hold only strings")
}

/// The process exit code for `error`: 1 generic, 2 usage, 3 not found, 4 auth or
/// network, 5 index missing, 6 index locked, 7 reindex required.
pub fn exit_code(error: &AppError) -> u8 {
    ErrorResponse::exit_code(error)
}

#[cfg(test)]
mod tests {
    use super::{exit_code, render_error_json, EXIT_OK};
    use rag_core::application::error::{AppError, GitError, Scrubbed, StorageError, UnknownSource};

    const GOLDEN_INDEX_MISSING: &str =
        include_str!("../../tests/golden/error_index_missing.json.golden");

    const GOLDEN_UNKNOWN_SOURCE: &str =
        include_str!("../../tests/golden/error_unknown_source.json.golden");

    #[test]
    fn index_missing_matches_the_golden_file() {
        let error = AppError::Storage(StorageError::IndexMissing);
        assert_eq!(render_error_json(&error), GOLDEN_INDEX_MISSING.trim_end());
    }

    #[test]
    fn an_unknown_source_matches_the_golden_file_with_its_valid_names() {
        let error = AppError::UnknownSource(UnknownSource::new(
            "nope",
            ["docs".to_owned(), "other".to_owned()],
        ));
        assert_eq!(render_error_json(&error), GOLDEN_UNKNOWN_SOURCE.trim_end());
    }

    #[test]
    fn reindex_required_names_the_source_in_details() {
        let error = AppError::StaleIndex {
            source: "docs".into(),
            also_stale: 0,
        };
        let parsed: serde_json::Value = serde_json::from_str(&render_error_json(&error)).unwrap();
        assert_eq!(parsed["error"]["code"], "reindex_required");
        assert_eq!(parsed["error"]["details"]["source"], "docs");
        assert_eq!(
            parsed["error"]["hint"],
            "ask the user to run `rag reindex docs`"
        );
    }

    #[test]
    fn a_usage_error_has_a_null_hint() {
        assert_eq!(
            render_error_json(&AppError::Usage(Scrubbed::from("bad flag"))),
            r#"{"error":{"code":"usage","message":"bad flag","hint":null}}"#
        );
    }

    #[test]
    fn every_error_renders_as_one_json_object_with_a_stable_code() {
        let cases = [
            (AppError::Usage("x".into()), "usage"),
            (AppError::NotFound("x".into()), "not_found"),
            (
                AppError::Git(GitError::NetworkOrAuth("x".into())),
                "network_or_auth",
            ),
            (
                AppError::Storage(StorageError::IndexMissing),
                "index_missing",
            ),
            (AppError::Storage(StorageError::Busy), "index_locked"),
            (
                AppError::Storage(StorageError::ReindexRequired),
                "reindex_required",
            ),
            (AppError::Storage(StorageError::Failed("x".into())), "error"),
        ];
        for (error, code) in cases {
            let parsed: serde_json::Value =
                serde_json::from_str(&render_error_json(&error)).expect("valid JSON");
            assert_eq!(parsed["error"]["code"], code);
            assert!(parsed["error"]["message"].is_string());
        }
    }

    #[test]
    fn the_exit_codes_follow_the_error_code_table() {
        assert_eq!(
            exit_code(&AppError::Storage(StorageError::Failed("x".into()))),
            1
        );
        assert_eq!(exit_code(&AppError::Usage("x".into())), 2);
        assert_eq!(exit_code(&AppError::NotFound("x".into())), 3);
        assert_eq!(exit_code(&AppError::Git(GitError::Timeout)), 4);
        assert_eq!(exit_code(&AppError::Storage(StorageError::IndexMissing)), 5);
        assert_eq!(exit_code(&AppError::Storage(StorageError::Busy)), 6);
        assert_eq!(
            exit_code(&AppError::Storage(StorageError::ReindexRequired)),
            7
        );
    }

    #[test]
    fn a_conflict_exits_six_because_it_is_a_lost_race() {
        assert_eq!(exit_code(&AppError::Storage(StorageError::Conflict)), 6);
    }

    #[test]
    fn success_is_exit_zero() {
        assert_eq!(EXIT_OK, 0);
    }

    #[test]
    fn credentials_in_a_message_never_reach_the_json() {
        let error = AppError::Git(GitError::Failed(
            "fatal: https://user:tok3n@github.com/o/r?token=abc123".into(),
        ));
        let json = render_error_json(&error);
        assert!(
            !json.contains("tok3n") && !json.contains("abc123"),
            "{json}"
        );
        assert!(json.contains("github.com/o/r"), "{json}");
    }
}
