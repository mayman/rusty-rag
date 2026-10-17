//! GitLocation: where a repository is cloned from, validated for use as a `git`
//! argument, plus `scrub_credentials` for messages.
//!
//! Allowed forms: `https://`, `ssh://`, scp-like (`[user@]host:path`), `file://`,
//! and a local path (absolute or drive-qualified; a relative path is rejected, since
//! its meaning would depend on the working directory). Everything else
//! (`http://`, `git://`, `ext::...`, `fd::...`, any other scheme) is rejected, as is
//! anything starting with `-` (it would be read as a `git` option), a host or ssh
//! user name starting with `-` (`ssh://-oProxyCommand=x@host/`), and control
//! characters.
//!
//! Credentials never survive construction: userinfo is removed from `https://`
//! URLs, and `ssh://` keeps only its user name (never a password) because the user
//! name (`git@`) is not a secret and is needed to authenticate. Query strings and
//! fragments (`?token=...`) are removed from `https://` and `ssh://` URLs. An
//! scp-like address with a password (`user:pw@host:path`) is rejected, because
//! there is no safe way to guess which part is the host. Authentication itself is
//! left to the user's own `git` configuration.
//!
//! `restore` rebuilds a location from storage without re-judging its shape (a
//! relative path stored by an older version stays readable), so a later tightening
//! of these rules never makes a stored source unreadable.

use std::fmt;

/// A validated repository location.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct GitLocation(String);

/// Why a string is not a valid `GitLocation`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GitLocationError {
    Empty,
    LeadingDash,
    UnsupportedTransport,
    ControlCharacter,
    Malformed,
    /// A password inside an scp-like address (`user:pw@host:path`).
    EmbeddedCredentials,
    /// A local path that is not absolute. Its meaning would depend on the working
    /// directory, so the caller resolves it first.
    RelativePath,
}

impl GitLocation {
    pub fn parse(text: &str) -> Result<Self, GitLocationError> {
        let text = Self::check_hard_rules(text)?;
        if let Some((scheme, rest)) = text.split_once("://") {
            if is_scheme(scheme) {
                return Self::from_url(scheme, rest);
            }
        }
        if is_drive_path(text) {
            return Ok(Self(text.to_owned()));
        }
        if let Some((address, path)) = text.split_once(':') {
            let is_scp_like = address.len() >= 2 && !address.contains(['/', '\\']);
            if is_scp_like {
                return Self::from_scp_like(text, address, path);
            }
        }
        if is_absolute_path(text) {
            Ok(Self(text.to_owned()))
        } else {
            Err(GitLocationError::RelativePath)
        }
    }

    /// Rebuilds a location read from storage. Only the checks that keep a value
    /// safe to hand to `git` apply (non-empty, no leading `-`, no control
    /// characters); the transport and shape rules of `parse` are not re-applied.
    pub fn restore(text: &str) -> Result<Self, GitLocationError> {
        Self::check_hard_rules(text).map(|text| Self(text.to_owned()))
    }

    /// The trimmed text, when it is non-empty, does not start with `-`, and has no
    /// control characters.
    fn check_hard_rules(text: &str) -> Result<&str, GitLocationError> {
        let text = text.trim();
        if text.is_empty() {
            return Err(GitLocationError::Empty);
        }
        if text.starts_with('-') {
            return Err(GitLocationError::LeadingDash);
        }
        if text.chars().any(char::is_control) {
            return Err(GitLocationError::ControlCharacter);
        }
        Ok(text)
    }

    fn from_url(scheme: &str, rest: &str) -> Result<Self, GitLocationError> {
        let kind = scheme.to_ascii_lowercase();
        if kind == "file" {
            return Ok(Self(format!("{scheme}://{rest}")));
        }
        if kind != "https" && kind != "ssh" {
            return Err(GitLocationError::UnsupportedTransport);
        }
        if rest.contains(char::is_whitespace) {
            return Err(GitLocationError::Malformed);
        }
        let (authority, tail) = rest.split_at(rest.find(['/', '?', '#']).unwrap_or(rest.len()));
        let path = &tail[..tail.find(['?', '#']).unwrap_or(tail.len())];
        let (userinfo, host) = match authority.rfind('@') {
            Some(at) => (Some(&authority[..at]), &authority[at + 1..]),
            None => (None, authority),
        };
        if host.is_empty() {
            return Err(GitLocationError::Malformed);
        }
        let user = match (kind.as_str(), userinfo) {
            ("ssh", Some(info)) => info.split(':').next().filter(|u| !u.is_empty()),
            _ => None,
        };
        if host.starts_with('-') || user.is_some_and(|u| u.starts_with('-')) {
            return Err(GitLocationError::LeadingDash);
        }
        Ok(Self(match user {
            Some(user) => format!("{scheme}://{user}@{host}{path}"),
            None => format!("{scheme}://{host}{path}"),
        }))
    }

    fn from_scp_like(text: &str, address: &str, path: &str) -> Result<Self, GitLocationError> {
        if path.starts_with(':') {
            return Err(GitLocationError::UnsupportedTransport);
        }
        if text.contains(char::is_whitespace) {
            return Err(GitLocationError::Malformed);
        }
        // `user:pw@host:path` splits at the first colon into host `user` and a path
        // whose first segment holds the `@`: a password, not a path.
        if path
            .split('/')
            .next()
            .is_some_and(|first| first.contains('@'))
        {
            return Err(GitLocationError::EmbeddedCredentials);
        }
        let host = address.rsplit('@').next().unwrap_or(address);
        if host.is_empty() {
            return Err(GitLocationError::Malformed);
        }
        if host.starts_with('-') {
            return Err(GitLocationError::LeadingDash);
        }
        Ok(Self(text.to_owned()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// A rooted path: `/dir` or `//server/share` (also `\\server\share`). A drive path
/// is checked separately (`is_drive_path`).
fn is_absolute_path(text: &str) -> bool {
    text.starts_with('/') || text.starts_with("\\\\")
}

/// `https`, `ssh`, `git+ssh`: letters, digits, `+`, `-`, `.`.
fn is_scheme(text: &str) -> bool {
    !text.is_empty()
        && text
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'+' | b'-' | b'.'))
}

/// `C:\dir`, `C:/dir`, or a bare `C:`.
fn is_drive_path(text: &str) -> bool {
    let bytes = text.as_bytes();
    bytes.len() >= 2
        && bytes[0].is_ascii_alphabetic()
        && bytes[1] == b':'
        && bytes.get(2).is_none_or(|b| matches!(b, b'/' | b'\\'))
}

/// Removes secrets from text that may quote what a subprocess printed: userinfo
/// (`user:password@`, `token@`) and query strings or fragments (`?token=...`) of
/// every `scheme://` URL, and the `user:password@` of scp-like addresses.
pub fn scrub_credentials(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(position) = rest.find("://") {
        let (before, after) = rest.split_at(position + 3);
        out.push_str(before);
        let url_end = after.find(is_url_delimiter).unwrap_or(after.len());
        let (url, remainder) = after.split_at(url_end);
        let (authority_and_path, _dropped_query) =
            url.split_at(url.find(['?', '#']).unwrap_or(url.len()));
        let (authority, path) = authority_and_path.split_at(
            authority_and_path
                .find('/')
                .unwrap_or(authority_and_path.len()),
        );
        out.push_str(authority.rsplit('@').next().unwrap_or(authority));
        out.push_str(path);
        rest = remainder;
    }
    out.push_str(rest);
    scrub_scp_passwords(&out)
}

fn is_url_delimiter(c: char) -> bool {
    c.is_whitespace() || matches!(c, '\'' | '"')
}

/// Drops `user:password@` from words such as `user:pw@host:path`. Words holding a
/// URL, and addresses without a colon before the `@` (`git@host:path`), are kept.
fn scrub_scp_passwords(text: &str) -> String {
    fn push_word(word: &str, out: &mut String) {
        let credentials_end = word.find('@').filter(|at| {
            let before = &word[..*at];
            !word.contains("://") && before.contains(':') && !before.contains('/')
        });
        match credentials_end {
            Some(at) => out.push_str(&word[at + 1..]),
            None => out.push_str(word),
        }
    }

    let mut out = String::with_capacity(text.len());
    let mut word_start = 0;
    for (index, c) in text.char_indices() {
        if is_url_delimiter(c) {
            push_word(&text[word_start..index], &mut out);
            out.push(c);
            word_start = index + c.len_utf8();
        }
    }
    push_word(&text[word_start..], &mut out);
    out
}

impl fmt::Display for GitLocation {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl fmt::Display for GitLocationError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let text = match self {
            Self::Empty => "repository location is empty",
            Self::LeadingDash => "repository location must not start with '-'",
            Self::UnsupportedTransport => {
                "only https, ssh, scp-like, file, and local path locations are supported"
            }
            Self::ControlCharacter => "repository location contains a control character",
            Self::Malformed => "repository location is not a valid address",
            Self::EmbeddedCredentials => {
                "repository location must not contain a password; use ssh keys or a git credential helper"
            }
            Self::RelativePath => {
                "a local repository path must be absolute (resolve it before adding)"
            }
        };
        f.write_str(text)
    }
}

impl std::error::Error for GitLocationError {}

#[cfg(test)]
mod tests {
    use super::{scrub_credentials, GitLocation, GitLocationError};

    fn location(text: &str) -> GitLocation {
        GitLocation::parse(text).expect(text)
    }

    #[test]
    fn accepts_the_allowed_transports() {
        for text in [
            "https://github.com/o/r.git",
            "ssh://git@github.com/o/r.git",
            "git@github.com:o/r.git",
            "github.com:o/r.git",
            "file:///home/me/proj",
            "/home/me/proj",
            "C:\\Users\\me\\proj",
            "C:/Users/me/proj",
            "\\\\server\\share\\proj",
            "C:\\My Projects\\proj",
        ] {
            assert_eq!(location(text).as_str(), text, "{text}");
        }
    }

    #[test]
    fn relative_local_paths_are_rejected_because_the_caller_must_resolve_them() {
        for text in [
            "./rel/path",
            "../up",
            "relative-dir",
            "dir/sub",
            ".",
            "..",
            "~/proj",
            "C:proj",
        ] {
            assert_eq!(
                GitLocation::parse(text),
                Err(GitLocationError::RelativePath),
                "{text}"
            );
        }
    }

    #[test]
    fn restore_stays_lenient_about_relative_paths() {
        for text in ["./rel/path", "relative-dir"] {
            assert_eq!(GitLocation::restore(text).unwrap().as_str(), text);
        }
    }

    #[test]
    fn surrounding_whitespace_is_trimmed() {
        assert_eq!(
            location("  https://h.com/o/r \n").as_str(),
            "https://h.com/o/r"
        );
    }

    #[test]
    fn userinfo_is_stripped_from_https_urls() {
        for (given, stored) in [
            (
                "https://user:s3cret@github.com/o/r",
                "https://github.com/o/r",
            ),
            (
                "https://ghp_token@github.com/o/r.git",
                "https://github.com/o/r.git",
            ),
            ("https://u:p@ss@github.com/o/r", "https://github.com/o/r"),
        ] {
            let parsed = location(given);
            assert_eq!(parsed.as_str(), stored);
            assert!(!parsed.to_string().contains("s3cret"));
        }
    }

    #[test]
    fn ssh_keeps_its_user_name_but_never_a_password() {
        assert_eq!(
            location("ssh://git:pw@github.com/o/r").as_str(),
            "ssh://git@github.com/o/r"
        );
        assert_eq!(
            location("ssh://git@github.com/o/r").as_str(),
            "ssh://git@github.com/o/r"
        );
    }

    #[test]
    fn query_strings_and_fragments_are_stripped_from_remote_urls() {
        for (given, stored) in [
            (
                "https://github.com/o/r.git?token=abc123",
                "https://github.com/o/r.git",
            ),
            (
                "https://github.com/o/r#access_token=abc",
                "https://github.com/o/r",
            ),
            (
                "https://u:p@github.com/o/r?a=b&c=d#frag",
                "https://github.com/o/r",
            ),
            ("https://github.com?token=abc", "https://github.com"),
            (
                "ssh://git@github.com/o/r?key=abc",
                "ssh://git@github.com/o/r",
            ),
        ] {
            let parsed = location(given);
            assert_eq!(parsed.as_str(), stored, "{given}");
            assert!(!parsed.to_string().contains("abc"));
        }
    }

    #[test]
    fn question_marks_in_local_paths_and_file_urls_are_kept() {
        for text in ["/tmp/what?/repo", "file:///tmp/a?b#c", "C:\\dir\\a#b"] {
            assert_eq!(location(text).as_str(), text);
        }
    }

    #[test]
    fn scp_like_addresses_with_a_password_are_rejected() {
        for text in [
            "user:pw@host.com:o/r",
            "user:pw@host.com/o/r",
            "git:s3cret@github.com:o/r.git",
        ] {
            assert_eq!(
                GitLocation::parse(text),
                Err(GitLocationError::EmbeddedCredentials),
                "{text}"
            );
        }
        assert!(GitLocation::parse("git@github.com:o/r@v2.git").is_ok());
        assert!(GitLocation::parse("host:dir/a@b").is_ok());
    }

    #[test]
    fn a_host_or_user_starting_with_a_dash_cannot_become_an_ssh_option() {
        for text in [
            "ssh://-oProxyCommand=evil@host.com/o/r",
            "ssh://git@-host.com/o/r",
            "https://-host.com/o/r",
            "git@-oProxyCommand=evil:o/r",
            "-oProxyCommand=evil@host.com:o/r",
        ] {
            assert_eq!(
                GitLocation::parse(text),
                Err(GitLocationError::LeadingDash),
                "{text}"
            );
        }
    }

    #[test]
    fn restore_trusts_stored_text_but_keeps_the_hard_safety_checks() {
        for text in [
            "http://old.example/o/r",
            "git+ssh://old.example/o/r",
            "https://h.com/o /r",
            "user:pw@host.com:o/r",
        ] {
            assert_eq!(GitLocation::restore(text).unwrap().as_str(), text, "{text}");
        }
        assert_eq!(
            GitLocation::restore("  "),
            Err(GitLocationError::Empty),
            "an empty stored location is unusable"
        );
        assert_eq!(
            GitLocation::restore("-oProxyCommand=x"),
            Err(GitLocationError::LeadingDash)
        );
        assert_eq!(
            GitLocation::restore("/tmp/a\nb"),
            Err(GitLocationError::ControlCharacter)
        );
    }

    #[test]
    fn restore_agrees_with_parse_on_valid_locations() {
        for text in [
            "https://github.com/o/r.git",
            "git@github.com:o/r.git",
            "/tmp/r",
        ] {
            assert_eq!(GitLocation::restore(text), GitLocation::parse(text));
        }
    }

    #[test]
    fn rejects_empty() {
        assert_eq!(GitLocation::parse("   "), Err(GitLocationError::Empty));
    }

    #[test]
    fn rejects_a_leading_dash_so_it_cannot_become_an_option() {
        for text in ["-oProxyCommand=evil", "--upload-pack=x", "-"] {
            assert_eq!(
                GitLocation::parse(text),
                Err(GitLocationError::LeadingDash),
                "{text}"
            );
        }
    }

    #[test]
    fn rejects_transports_that_are_not_allowed() {
        for text in [
            "http://github.com/o/r",
            "git://github.com/o/r",
            "ftp://h/x",
            "ext::sh -c evil",
            "fd::3",
            "git+ssh://h/o/r",
            "javascript://x",
        ] {
            assert_eq!(
                GitLocation::parse(text),
                Err(GitLocationError::UnsupportedTransport),
                "{text}"
            );
        }
    }

    #[test]
    fn rejects_control_characters() {
        for text in [
            "https://h.com/o/r\u{0}",
            "https://h.com/o\n/r",
            "/tmp/a\u{7}b",
        ] {
            assert_eq!(
                GitLocation::parse(text),
                Err(GitLocationError::ControlCharacter),
                "{text:?}"
            );
        }
    }

    #[test]
    fn rejects_whitespace_inside_a_remote_address() {
        assert_eq!(
            GitLocation::parse("https://h.com/o /r"),
            Err(GitLocationError::Malformed)
        );
        assert_eq!(
            GitLocation::parse("git@h.com:o /r"),
            Err(GitLocationError::Malformed)
        );
    }

    #[test]
    fn scrubbing_removes_userinfo_from_urls_in_messages() {
        let message = "fatal: unable to access 'https://user:tok3n@github.com/o/r/': 403";
        let scrubbed = scrub_credentials(message);
        assert!(
            !scrubbed.contains("tok3n") && !scrubbed.contains("user"),
            "{scrubbed}"
        );
        assert!(scrubbed.contains("https://github.com/o/r/"), "{scrubbed}");
    }

    #[test]
    fn scrubbing_handles_several_urls_and_ssh_users() {
        assert_eq!(
            scrub_credentials("a https://x:y@h1.com/p b ssh://git@h2.com/q"),
            "a https://h1.com/p b ssh://h2.com/q"
        );
    }

    #[test]
    fn scrubbing_removes_query_strings_and_fragments_from_urls() {
        assert_eq!(
            scrub_credentials("GET 'https://h.com/o/r?token=abc#x' failed, see https://h.com/doc"),
            "GET 'https://h.com/o/r' failed, see https://h.com/doc"
        );
        assert_eq!(
            scrub_credentials("https://u:p@h.com?token=abc"),
            "https://h.com"
        );
        assert_eq!(scrub_credentials("https://h.com/a?b=c"), "https://h.com/a");
    }

    #[test]
    fn scrubbing_removes_a_password_from_scp_like_addresses() {
        assert_eq!(
            scrub_credentials("fatal: could not read from user:pw@host.com:o/r"),
            "fatal: could not read from host.com:o/r"
        );
        assert_eq!(
            scrub_credentials("git@github.com:o/r.git is fine"),
            "git@github.com:o/r.git is fine"
        );
    }

    #[test]
    fn scrubbing_leaves_text_without_urls_alone() {
        for text in ["", "no url here", "git@github.com:o/r.git", "see C:/x"] {
            assert_eq!(scrub_credentials(text), text);
        }
    }

    #[test]
    fn scrubbing_handles_a_url_that_ends_the_message() {
        assert_eq!(scrub_credentials("https://u:p@h.com"), "https://h.com");
    }
}
