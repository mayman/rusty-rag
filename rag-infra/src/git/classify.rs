//! Turning a failed `git` run (stderr under `LC_ALL=C`) into a `GitError`.
//!
//! Messages are scrubbed of credentials by `Scrubbed` at construction; only the
//! first lines of stderr are kept.

use rag_core::application::error::GitError;
use rag_core::domain::git_location::scrub_credentials;

/// How much of stderr is quoted in an error message.
const MAX_MESSAGE_CHARS: usize = 400;

/// Classifies `stderr` of a failed command.
pub fn classify(stderr: &str) -> GitError {
    let lower = stderr.to_ascii_lowercase();
    let has = |needles: &[&str]| needles.iter().any(|needle| lower.contains(needle));
    // Scrub the whole text first: a cut made by the length limit could otherwise
    // split a URL and leave a credential that no scrubber recognises.
    let message = summary(&scrub_credentials(stderr));

    if has(&["index.lock", "shallow.lock", "another git process"]) {
        GitError::Busy
    } else if has(&[
        "couldn't find remote ref",
        "does not appear to be a git repository",
        "repository not found",
        "not found",
        "returned error: 404",
        "not a valid object name",
        "not a tree object",
        "bad object",
        "invalid object name",
        "unknown revision",
        "no such file or directory",
        "does not exist",
    ]) {
        GitError::NotFound(message.into())
    } else if has(&[
        "could not resolve host",
        "could not read username",
        "could not read password",
        "authentication failed",
        "terminal prompts disabled",
        "permission denied",
        "host key verification failed",
        "unable to access",
        "connection refused",
        "connection timed out",
        "connection reset",
        "network is unreachable",
        "operation timed out",
        "ssl",
        "tls",
        "returned error: 401",
        "returned error: 403",
        "returned error: 5",
        "could not read from remote repository",
        "the remote end hung up",
        "early eof",
    ]) {
        GitError::NetworkOrAuth(message.into())
    } else if has(&["mismatched algorithms", "object format", "sha256"]) {
        GitError::Failed(
            "the repository uses SHA-256 object ids, which rag does not support".into(),
        )
    } else {
        GitError::Failed(message.into())
    }
}

fn summary(stderr: &str) -> String {
    let text: String = stderr
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .take(3)
        .collect::<Vec<_>>()
        .join(" | ");
    let mut text: String = text.chars().take(MAX_MESSAGE_CHARS).collect();
    if text.is_empty() {
        text.push_str("git exited with an error");
    }
    text
}

#[cfg(test)]
mod tests {
    use super::*;

    fn kind(stderr: &str) -> &'static str {
        match classify(stderr) {
            GitError::NotFound(_) => "not_found",
            GitError::NetworkOrAuth(_) => "network",
            GitError::Timeout => "timeout",
            GitError::Busy => "busy",
            GitError::TooLarge(_) => "too_large",
            GitError::Failed(_) => "failed",
        }
    }

    #[test]
    fn missing_repositories_and_refs_are_not_found() {
        for stderr in [
            "fatal: couldn't find remote ref nope",
            "fatal: 'C:/x' does not appear to be a git repository\nfatal: Could not read from remote repository.",
            "remote: Repository not found.\nfatal: repository 'https://github.com/o/r/' not found",
            "fatal: not a tree object",
            "fatal: Not a valid object name abc",
            "error: RPC failed; HTTP 404 curl 22 The requested URL returned error: 404",
        ] {
            assert_eq!(kind(stderr), "not_found", "{stderr}");
        }
    }

    #[test]
    fn auth_and_network_failures_are_network_or_auth() {
        for stderr in [
            "fatal: unable to access 'https://h/r/': Could not resolve host: h",
            "fatal: could not read Username for 'https://h': terminal prompts disabled",
            "git@h: Permission denied (publickey).\nfatal: Could not read from remote repository.",
            "fatal: Authentication failed for 'https://h/r/'",
            "fatal: unable to access 'https://h/r/': Failed to connect: Connection refused",
            "ssh: connect to host h port 22: Connection timed out",
            "Host key verification failed.",
            "The requested URL returned error: 403",
        ] {
            assert_eq!(kind(stderr), "network", "{stderr}");
        }
    }

    #[test]
    fn lock_contention_is_busy() {
        for stderr in [
            "fatal: Unable to create '/r/.git/index.lock': File exists.\n\nAnother git process seems to be running",
            "fatal: Unable to create '/r/shallow.lock': File exists.",
            "fatal: another git process seems to be running in this repository",
        ] {
            assert_eq!(kind(stderr), "busy", "{stderr}");
        }
    }

    #[test]
    fn only_real_lock_contention_is_busy() {
        for stderr in [
            "fatal: unable to create temporary file '/r/objects/tmp_obj_x': No space left on device",
            "fatal: Unable to create '/r/objects/pack/tmp_pack': Permission denied",
            "error: cannot lock ref 'refs/rag/head': is at abc but expected def",
            "fatal: unable to lock config file",
            "error: could not write 'x.lock': file exists elsewhere",
        ] {
            assert_ne!(kind(stderr), "busy", "{stderr}");
        }
    }

    #[test]
    fn anything_else_is_failed_and_sha256_is_explained() {
        assert_eq!(kind("fatal: something odd"), "failed");
        assert_eq!(kind(""), "failed");
        let GitError::Failed(message) =
            classify("fatal: mismatched algorithms: client sha1; server sha256")
        else {
            panic!("expected Failed");
        };
        assert!(message.as_str().contains("SHA-256"), "{message}");
    }

    #[test]
    fn credentials_never_survive_in_the_message() {
        let error = classify(
            "fatal: unable to access 'https://user:tok3n@github.com/o/r/?token=abc123': Could not resolve host",
        );
        let shown = error.to_string();
        assert!(
            !shown.contains("tok3n") && !shown.contains("abc123"),
            "{shown}"
        );
        assert!(shown.contains("github.com/o/r"), "{shown}");
    }

    #[test]
    fn a_credential_cut_by_the_length_limit_still_does_not_leak() {
        // The URL straddles the 400-character limit: truncating first would leave
        // `https://user:s3cr` in the message, which no scrubber recognises.
        let stderr = format!(
            "fatal: {} https://user:s3cretpassword@github.com/o/r",
            "x".repeat(MAX_MESSAGE_CHARS - 40)
        );
        for error in [
            classify(&stderr),
            classify(&format!("{stderr} Repository not found")),
        ] {
            let shown = error.to_string();
            assert!(
                !shown.contains("s3cr") && !shown.contains("user:"),
                "{shown}"
            );
        }
        let GitError::Failed(message) = classify(&stderr) else {
            panic!("expected Failed");
        };
        assert!(message.as_str().len() <= MAX_MESSAGE_CHARS);
    }

    #[test]
    fn long_output_is_truncated() {
        let GitError::Failed(message) = classify(&"x".repeat(5000)) else {
            panic!("expected Failed");
        };
        assert!(message.as_str().len() <= MAX_MESSAGE_CHARS);
    }
}
