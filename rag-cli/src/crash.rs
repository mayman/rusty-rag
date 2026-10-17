//! A bug in `rag` must still end as the documented error shape and exit code 1, not
//! as a Rust backtrace and a process abort that a caller cannot parse.
//!
//! `guarded` runs the whole command under `catch_unwind` and turns a panic into an
//! `AppError`. The panic hook replaces the default one: it prints one short, scrubbed
//! line to stderr (never the payload verbatim, which may quote repository content or
//! a URL) and ignores a closed stderr.

use std::any::Any;
use std::io::Write;
use std::panic::{catch_unwind, AssertUnwindSafe};

use rag_core::application::error::{AppError, Scrubbed, StorageError};

/// The text a caller sees for a panic: what it was, in one scrubbed line.
fn panic_message(payload: &(dyn Any + Send)) -> String {
    let detail = payload
        .downcast_ref::<&str>()
        .map(|text| (*text).to_owned())
        .or_else(|| payload.downcast_ref::<String>().cloned())
        .unwrap_or_default();
    let detail = Scrubbed::new(&detail);
    if detail.as_str().is_empty() {
        "internal error: rag hit a bug; please report it".to_owned()
    } else {
        format!(
            "internal error: rag hit a bug ({}); please report it",
            detail.as_str()
        )
    }
}

/// The error a panic with `payload` is reported as: the generic code, exit 1.
pub fn error_for_panic(payload: &(dyn Any + Send)) -> AppError {
    AppError::Storage(StorageError::Failed(Scrubbed::new(&panic_message(payload))))
}

/// Runs `work`; a panic inside it becomes an error.
pub fn guarded<T>(work: impl FnOnce() -> T) -> Result<T, AppError> {
    catch_unwind(AssertUnwindSafe(work)).map_err(|payload| error_for_panic(payload.as_ref()))
}

/// Replaces the default panic hook (which prints the message and a backtrace hint,
/// unscrubbed) with one scrubbed line on stderr.
pub fn install_hook() {
    std::panic::set_hook(Box::new(|info| {
        let place = info
            .location()
            .map(|location| format!(" at {}:{}", location.file(), location.line()))
            .unwrap_or_default();
        let line = Scrubbed::new(&format!("internal error: panic{place}"));
        let _ = writeln!(std::io::stderr(), "rag: {}", line.as_str());
    }));
}

#[cfg(test)]
mod tests {
    use rag_core::application::error::ErrorCode;

    use super::{error_for_panic, guarded};
    use crate::output::error::{exit_code, render_error_json};

    #[test]
    fn a_panic_becomes_the_generic_error_with_exit_one() {
        let result = guarded(|| -> u8 { panic!("the index is on fire") });
        let error = result.unwrap_err();
        assert_eq!(error.code(), ErrorCode::Generic);
        assert_eq!(exit_code(&error), 1);
        assert!(
            error.to_string().contains("the index is on fire"),
            "{error}"
        );
        assert!(error.to_string().contains("report it"), "{error}");
    }

    #[test]
    fn the_panic_error_renders_as_the_json_error_shape() {
        let error = guarded(|| -> u8 { panic!("boom") }).unwrap_err();
        let parsed: serde_json::Value =
            serde_json::from_str(&render_error_json(&error)).expect("one JSON object");
        assert_eq!(parsed["error"]["code"], "error");
        assert!(parsed["error"]["message"]
            .as_str()
            .is_some_and(|message| message.contains("boom")));
    }

    #[test]
    fn a_formatted_panic_payload_is_reported_too() {
        let number = 7;
        let error = guarded(|| -> u8 { panic!("failed at step {number}") }).unwrap_err();
        assert!(error.to_string().contains("failed at step 7"), "{error}");
    }

    #[test]
    fn a_panic_never_leaks_a_credential_or_a_control_character() {
        let error = guarded(|| -> u8 {
            panic!("cannot reach https://user:tok3n@example.com/o/r.git\u{1b}[2J")
        })
        .unwrap_err();
        let shown = error.to_string();
        assert!(!shown.contains("tok3n"), "{shown}");
        assert!(!shown.contains('\u{1b}'), "{shown}");
        assert!(!render_error_json(&error).contains("tok3n"));
    }

    #[test]
    fn a_payload_that_is_not_text_still_gives_a_message() {
        let error = error_for_panic(&42_u32);
        assert!(error.to_string().contains("report it"), "{error}");
    }

    #[test]
    fn work_that_does_not_panic_passes_through() {
        assert_eq!(guarded(|| 5).unwrap(), 5);
    }
}
