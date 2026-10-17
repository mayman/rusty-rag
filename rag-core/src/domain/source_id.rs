//! SourceId: the identity of a registered source (repository).
//!
//! The id is a readable slug plus a short hash of the canonical location, written
//! `host/owner/name-1a2b3c4d5e6f`. The slug is what people see (`repo` in search
//! output). Ids are host aware (`github.com/o/r` and `gitlab.com/o/r` differ) and
//! the transport is irrelevant (`https://host/o/r.git` and `git@host:o/r.git` give
//! the same id). Only the host is lowercased: owner and name keep their case, since
//! most hosts treat them as case-sensitive. Credentials in a URL never reach the id.
//! A local path becomes `local/<dirname>-<6 hex>` (the slug carries the start of the
//! hash of the normalised path, so two directories with one name stay distinct).
//!
//! Every slug segment is validated (`parse` re-checks stored ids): no empty, dot-only
//! or `.git` segments. The checkout directory of a source is named by the hash alone
//! (`checkout_dir_name`), never by the slug, so slugs never touch the file system.

use std::fmt;

use crate::domain::git_location::GitLocation;

/// Length of the hash suffix, in hex characters.
const HASH_CHARS: usize = 12;

/// Characters of the hash that end the slug of a local directory.
const LOCAL_SLUG_HASH_CHARS: usize = 6;

/// Slug plus short hash. Always valid; construct with `from_location` or `parse`.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct SourceId {
    /// `slug-hash`.
    full: String,
    /// Byte length of the slug at the start of `full`.
    slug_len: usize,
}

/// Why a location or stored id is not a valid `SourceId`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SourceIdError {
    MissingRepositoryPath,
    InvalidHost,
    InvalidSegment,
    Malformed,
}

impl SourceId {
    /// Derives the id from a validated location: a git URL (https, ssh, scp-like,
    /// `file://`) or a local filesystem path. Credentials, queries and the like were
    /// already removed by `GitLocation`, so they cannot reach the id.
    pub fn from_location(location: &GitLocation) -> Result<Self, SourceIdError> {
        let location = location.as_str();
        match split_remote(location) {
            Some(remote) => Self::remote(remote),
            None => Self::local(location),
        }
    }

    fn remote(remote: Remote<'_>) -> Result<Self, SourceIdError> {
        let Remote { host, port, path } = remote;
        let host = host.to_ascii_lowercase();
        if host.is_empty()
            || !host
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'.' || b == b'-')
        {
            return Err(SourceIdError::InvalidHost);
        }
        let path = path.replace('\\', "/");
        let path = path.trim_matches('/');
        let path = path.strip_suffix(".git").unwrap_or(path);
        let segments: Vec<&str> = path.split('/').filter(|s| !s.is_empty()).collect();
        if segments.is_empty() {
            return Err(SourceIdError::MissingRepositoryPath);
        }
        let mut slug = host.clone();
        for segment in &segments {
            slug.push('/');
            slug.push_str(&sanitise_segment(segment)?);
        }
        // The port is part of the identity (another port is another server) but not
        // of the slug, which is a path-like name. Default ports add nothing, so the
        // transport stays irrelevant.
        let authority = match port {
            Some(port) => format!("{host}:{port}"),
            None => host,
        };
        let canonical = format!("https://{authority}/{}", segments.join("/"));
        Ok(Self::from_parts(&slug, &short_hash(&canonical)))
    }

    fn local(path: &str) -> Result<Self, SourceIdError> {
        let path = path.strip_prefix("file://").unwrap_or(path);
        let normalised = path.replace('\\', "/");
        let normalised = normalised.trim_end_matches('/');
        let name = normalised
            .rsplit('/')
            .find(|s| !s.is_empty())
            .ok_or(SourceIdError::MissingRepositoryPath)?;
        let name = sanitise_segment(name)?;
        let hash = short_hash(&format!("local:{normalised}"));
        // Two directories called `proj` must not share a slug, or `--repo` could not
        // tell them apart: the slug carries the start of the hash.
        Ok(Self::from_parts(
            &format!("local/{name}-{}", &hash[..LOCAL_SLUG_HASH_CHARS]),
            &hash,
        ))
    }

    /// Reads back an id produced by `as_str` (for example from storage).
    pub fn parse(text: &str) -> Result<Self, SourceIdError> {
        let (slug, hash) = text.rsplit_once('-').ok_or(SourceIdError::Malformed)?;
        let hash_ok = hash.len() == HASH_CHARS
            && hash
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b));
        let slug_ok = slug.split('/').all(|segment| {
            segment_is_valid(segment)
                && segment
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
        });
        if hash_ok && slug_ok {
            Ok(Self::from_parts(slug, hash))
        } else {
            Err(SourceIdError::Malformed)
        }
    }

    /// The readable part, `host/owner/name`.
    pub fn slug(&self) -> &str {
        &self.full[..self.slug_len]
    }

    /// The full id, `slug-hash`.
    pub fn as_str(&self) -> &str {
        &self.full
    }

    /// The names a new source takes by default, most wanted first: the last path
    /// segment of the location, then the last two (`owner/name`; for a local path,
    /// `parent/directory`), then the full slug. `self` must come from `location`. A
    /// local path is named from its own segments, since its slug carries a hash.
    pub fn default_names(&self, location: &GitLocation) -> Vec<String> {
        let segments: Vec<String> = match split_remote(location.as_str()) {
            Some(_) => self.slug().split('/').skip(1).map(str::to_owned).collect(),
            None => local_segments(location.as_str()),
        };
        let mut names = Vec::new();
        if let Some(last) = segments.last() {
            names.push(last.clone());
        }
        if segments.len() >= 2 {
            names.push(segments[segments.len() - 2..].join("/"));
        }
        names.push(self.slug().to_owned());
        names
    }

    /// The name of this source's checkout directory: the 12-character hash alone,
    /// so no part of the slug (or of a hostile URL) reaches the file system.
    pub fn checkout_dir_name(&self) -> &str {
        &self.full[self.slug_len + 1..]
    }

    fn from_parts(slug: &str, hash: &str) -> Self {
        Self {
            full: format!("{slug}-{hash}"),
            slug_len: slug.len(),
        }
    }
}

/// The parts of a remote location that make up its identity.
struct Remote<'a> {
    host: &'a str,
    /// The port, only when it is not the default of the transport (443 for https,
    /// 22 for ssh and scp-like addresses).
    port: Option<&'a str>,
    path: &'a str,
}

/// The remote parts of a URL or scp-like address, with credentials removed. `None`
/// for local paths (including `file://` URLs and drive letters).
fn split_remote(location: &str) -> Option<Remote<'_>> {
    if let Some((scheme, rest)) = location.split_once("://") {
        if scheme.eq_ignore_ascii_case("file") {
            return None;
        }
        let (authority, path) = rest.split_once('/').unwrap_or((rest, ""));
        let host_port = authority.rsplit('@').next().unwrap_or(authority);
        let (host, port) = match host_port.split_once(':') {
            Some((host, port)) => (host, Some(port)),
            None => (host_port, None),
        };
        let default = if scheme.eq_ignore_ascii_case("ssh") {
            "22"
        } else {
            "443"
        };
        return Some(Remote {
            host,
            port: port.filter(|port| !port.is_empty() && *port != default),
            path,
        });
    }
    let (address, path) = location.split_once(':')?;
    let looks_like_path = address.contains('/') || address.contains('\\');
    // A single letter before ':' is a Windows drive, not a host.
    if looks_like_path || address.len() < 2 {
        return None;
    }
    Some(Remote {
        host: address.rsplit('@').next().unwrap_or(address),
        port: None,
        path,
    })
}

/// ASCII letters (case kept), digits, `.`, `_`, `-`; anything else becomes `-`.
/// Rejects segments that are empty, only dots (`.`, `..`), or `.git`.
fn sanitise_segment(segment: &str) -> Result<String, SourceIdError> {
    let cleaned: String = segment
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-') {
                c
            } else {
                '-'
            }
        })
        .collect();
    if segment_is_valid(&cleaned) {
        Ok(cleaned)
    } else {
        Err(SourceIdError::InvalidSegment)
    }
}

/// The sanitised directory segments of a local path, drive letters left out.
fn local_segments(location: &str) -> Vec<String> {
    let path = location.strip_prefix("file://").unwrap_or(location);
    path.replace('\\', "/")
        .split('/')
        .filter(|segment| !segment.is_empty() && !segment.contains(':'))
        .filter_map(|segment| sanitise_segment(segment).ok())
        .collect()
}

/// A slug segment is non-empty, not made of dots only, and not `.git`.
fn segment_is_valid(segment: &str) -> bool {
    !segment.is_empty()
        && !segment.chars().all(|c| c == '.')
        && !segment.eq_ignore_ascii_case(".git")
}

fn short_hash(canonical: &str) -> String {
    blake3::hash(canonical.as_bytes()).to_hex()[..HASH_CHARS].to_owned()
}

impl fmt::Display for SourceId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.full)
    }
}

impl fmt::Display for SourceIdError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let text = match self {
            Self::MissingRepositoryPath => "repository location has no owner/name path",
            Self::InvalidHost => "repository host is not valid",
            Self::InvalidSegment => "repository path has an unusable segment",
            Self::Malformed => "not a source id",
        };
        f.write_str(text)
    }
}

impl std::error::Error for SourceIdError {}

#[cfg(test)]
mod tests {
    use super::{SourceId, SourceIdError};
    use crate::domain::git_location::GitLocation;

    fn from(location: &str) -> Result<SourceId, SourceIdError> {
        SourceId::from_location(&GitLocation::parse(location).expect(location))
    }

    fn id(location: &str) -> SourceId {
        from(location).expect(location)
    }

    #[test]
    fn https_url_gives_a_host_owner_name_slug_lowercasing_only_the_host() {
        assert_eq!(
            id("https://GitHub.com/Owner/Repo.git").slug(),
            "github.com/Owner/Repo"
        );
    }

    #[test]
    fn a_non_default_port_is_part_of_the_identity_but_not_of_the_slug() {
        let plain = id("https://git.example.com/o/r");
        let on_port = id("https://git.example.com:8443/o/r");
        assert_ne!(plain, on_port, "another server, another source");
        assert_eq!(plain.slug(), on_port.slug());
        assert_ne!(
            id("https://git.example.com:8443/o/r"),
            id("https://git.example.com:9443/o/r")
        );
        assert!(SourceId::parse(on_port.as_str()).is_ok());
    }

    #[test]
    fn default_ports_do_not_change_the_identity() {
        assert_eq!(
            id("https://git.example.com:443/o/r"),
            id("https://git.example.com/o/r")
        );
        assert_eq!(
            id("ssh://git@git.example.com:22/o/r"),
            id("git@git.example.com:o/r")
        );
        assert_eq!(
            id("ssh://git@git.example.com:22/o/r"),
            id("https://git.example.com/o/r")
        );
        assert_ne!(
            id("ssh://git@git.example.com:2222/o/r"),
            id("git@git.example.com:o/r"),
            "ssh on another port is another server"
        );
    }

    #[test]
    fn owner_and_name_are_case_sensitive() {
        assert_ne!(id("https://github.com/O/R"), id("https://github.com/o/r"));
    }

    #[test]
    fn checkout_dir_name_is_the_hash_alone() {
        let one = id("https://github.com/o/r");
        let name = one.checkout_dir_name();
        assert_eq!(name.len(), 12);
        assert!(one.as_str().ends_with(&format!("-{name}")));
        assert!(!name.contains('/') && !name.contains('.'));
        assert_ne!(name, id("https://github.com/o/other").checkout_dir_name());
    }

    #[test]
    fn parse_validates_every_slug_segment() {
        for bad in [
            "github.com/../r-1a2b3c4d5e6f",
            "/github.com/o/r-1a2b3c4d5e6f",
            "github.com//r-1a2b3c4d5e6f",
            "github.com/o/./r-1a2b3c4d5e6f",
            "github.com/o/r/-1a2b3c4d5e6f",
            "github.com/o/.git-1a2b3c4d5e6f",
            "github.com/o/...-1a2b3c4d5e6f",
            "github.com/o/r s-1a2b3c4d5e6f",
            "github.com/o\\r-1a2b3c4d5e6f",
            "github.com/o/r-1a2b3c4d5e6f/",
        ] {
            assert_eq!(SourceId::parse(bad), Err(SourceIdError::Malformed), "{bad}");
        }
    }

    #[test]
    fn parse_accepts_mixed_case_slugs_it_produced() {
        let original = id("https://github.com/Owner/Repo");
        assert_eq!(SourceId::parse(original.as_str()), Ok(original));
    }

    #[test]
    fn dot_git_segments_inside_a_url_are_rejected() {
        assert_eq!(
            from("https://github.com/o/.git/x"),
            Err(SourceIdError::InvalidSegment)
        );
    }

    #[test]
    fn https_and_ssh_forms_of_one_repository_share_an_id() {
        let https = id("https://github.com/o/r.git");
        for other in [
            "https://github.com/o/r",
            "https://github.com/o/r/",
            "git@github.com:o/r.git",
            "ssh://git@github.com/o/r.git",
            "ssh://git@github.com:22/o/r",
            "HTTPS://GitHub.com/o/r.git",
        ] {
            assert_eq!(id(other), https, "{other}");
        }
    }

    #[test]
    fn same_owner_and_name_on_different_hosts_differ() {
        assert_ne!(id("https://github.com/o/r"), id("https://gitlab.com/o/r"));
    }

    #[test]
    fn credentials_never_reach_the_id() {
        let with = id("https://user:s3cret-token@github.com/o/r.git");
        assert_eq!(with, id("https://github.com/o/r.git"));
        assert!(!with.to_string().contains("s3cret"));
    }

    #[test]
    fn nested_group_paths_are_kept() {
        assert_eq!(
            id("https://gitlab.com/g/sub/proj.git").slug(),
            "gitlab.com/g/sub/proj"
        );
    }

    #[test]
    fn as_str_is_slug_dash_twelve_lowercase_hex_characters() {
        let text = id("https://github.com/o/r").to_string();
        let (slug, hash) = text.rsplit_once('-').unwrap();
        assert_eq!(slug, "github.com/o/r");
        assert_eq!(hash.len(), 12);
        assert!(hash
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)));
    }

    #[test]
    fn parse_round_trips_as_str() {
        let original = id("git@github.com:o/r.git");
        assert_eq!(SourceId::parse(original.as_str()), Ok(original.clone()));
    }

    #[test]
    fn parse_rejects_text_that_is_not_an_id() {
        for bad in [
            "",
            "no-hash-here",
            "github.com/o/r",
            "x-1234567",
            "x-1a2b3c4d",
            "x-GGGGGGGGGGGG",
            "-1a2b3c4d5e6f",
        ] {
            assert_eq!(SourceId::parse(bad), Err(SourceIdError::Malformed), "{bad}");
        }
    }

    /// `local/<name>-<6 hex>`: the directory name plus the first six characters of
    /// the id hash.
    fn local_slug_parts(slug: &str) -> (&str, &str) {
        let rest = slug.strip_prefix("local/").expect("a local slug");
        rest.rsplit_once('-').expect("name-hash")
    }

    #[test]
    fn local_paths_become_local_slugs_by_directory_name_and_a_short_hash() {
        for (location, name) in [
            ("/home/me/My Proj/", "My-Proj"),
            ("C:\\Users\\me\\proj", "proj"),
            ("file:///home/me/proj", "proj"),
        ] {
            let id = id(location);
            let (found, hash) = local_slug_parts(id.slug());
            assert_eq!(found, name, "{location}");
            assert_eq!(hash.len(), 6, "{location}: {}", id.slug());
            assert!(
                id.checkout_dir_name().starts_with(hash),
                "the slug hash is the start of the id hash"
            );
        }
    }

    #[test]
    fn local_directories_with_one_name_get_different_slugs() {
        let (a, b) = (id("/a/proj"), id("/b/proj"));
        assert_ne!(a, b);
        assert_ne!(
            a.slug(),
            b.slug(),
            "two proj directories stay distinguishable"
        );
    }

    #[test]
    fn a_local_id_round_trips_through_parse() {
        let original = id("/home/me/proj");
        assert_eq!(SourceId::parse(original.as_str()), Ok(original));
    }

    #[test]
    fn remote_slugs_stay_free_of_the_hash() {
        assert_eq!(id("https://github.com/o/r").slug(), "github.com/o/r");
    }

    #[test]
    fn backslash_and_slash_local_paths_agree() {
        assert_eq!(id("C:\\Users\\me\\proj"), id("C:/Users/me/proj"));
    }

    #[test]
    fn rejects_pathless_locations_and_unusable_hosts() {
        assert_eq!(
            from("https://github.com"),
            Err(SourceIdError::MissingRepositoryPath)
        );
        assert_eq!(
            from("https://under_score.example/o/r"),
            Err(SourceIdError::InvalidHost)
        );
    }

    #[test]
    fn query_tokens_in_a_location_never_reach_the_id() {
        assert_eq!(
            id("https://github.com/o/r?token=abc123"),
            id("https://github.com/o/r")
        );
        assert!(!id("https://github.com/o/r?token=abc123")
            .to_string()
            .contains("abc123"));
    }

    fn default_names(location: &str) -> Vec<String> {
        let location = GitLocation::parse(location).expect(location);
        let id = SourceId::from_location(&location).expect("a valid location");
        id.default_names(&location)
    }

    #[test]
    fn a_remote_is_named_by_last_segment_then_owner_and_name_then_the_slug() {
        assert_eq!(
            default_names("https://github.com/serde-rs/serde.git"),
            ["serde", "serde-rs/serde", "github.com/serde-rs/serde"]
        );
    }

    #[test]
    fn nested_group_paths_use_their_last_two_segments_for_the_second_name() {
        assert_eq!(
            default_names("https://gitlab.com/g/sub/proj.git"),
            ["proj", "sub/proj", "gitlab.com/g/sub/proj"]
        );
    }

    #[test]
    fn a_remote_with_one_path_segment_has_no_owner_and_name_form() {
        assert_eq!(
            default_names("https://example.com/proj"),
            ["proj", "example.com/proj"]
        );
    }

    #[test]
    fn a_local_directory_is_named_by_parent_and_directory_then_the_slug() {
        let slug = id("/srv/repos/proj").slug().to_owned();
        assert_eq!(
            default_names("/srv/repos/proj")[..2],
            ["proj", "repos/proj"]
        );
        assert_eq!(default_names("/srv/repos/proj").last(), Some(&slug));
    }

    #[test]
    fn a_windows_directory_uses_its_real_parent_not_the_drive() {
        let names = default_names("C:\\Users\\me\\proj");
        assert_eq!(names[..2], ["proj", "me/proj"]);
        assert_eq!(names.len(), 3, "{names:?}");
    }

    #[test]
    fn rejects_traversal_segments_in_a_url() {
        assert_eq!(
            from("https://github.com/../r"),
            Err(SourceIdError::InvalidSegment)
        );
    }
}
