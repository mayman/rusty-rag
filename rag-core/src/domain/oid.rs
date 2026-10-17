//! Git object ids: `CommitId` and `BlobOid`, both lowercase hexadecimal.

use std::fmt;

/// A git object id was not 40 (SHA-1) or 64 (SHA-256) hexadecimal characters.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OidError;

impl fmt::Display for OidError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("object id must be 40 or 64 hexadecimal characters")
    }
}

impl std::error::Error for OidError {}

fn normalise(text: &str) -> Result<String, OidError> {
    let valid_length = text.len() == 40 || text.len() == 64;
    if valid_length && text.bytes().all(|b| b.is_ascii_hexdigit()) {
        Ok(text.to_ascii_lowercase())
    } else {
        Err(OidError)
    }
}

/// The id of a commit.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct CommitId(String);

impl CommitId {
    /// Accepts 40 or 64 hex characters, in either case; stored lowercase.
    pub fn parse(text: &str) -> Result<Self, OidError> {
        normalise(text).map(Self)
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// The first 7 characters, for display.
    pub fn short(&self) -> &str {
        &self.0[..7]
    }
}

impl fmt::Display for CommitId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// The id of a git blob (the content identity of a file at a commit).
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct BlobOid(String);

impl BlobOid {
    /// Accepts 40 or 64 hex characters, in either case; stored lowercase.
    pub fn parse(text: &str) -> Result<Self, OidError> {
        normalise(text).map(Self)
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for BlobOid {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

#[cfg(test)]
mod tests {
    use super::{BlobOid, CommitId, OidError};

    const SHA1: &str = "0123456789abcdef0123456789abcdef01234567";

    #[test]
    fn commit_id_accepts_sha1_and_sha256_lengths() {
        assert!(CommitId::parse(SHA1).is_ok());
        assert!(CommitId::parse(&"a".repeat(64)).is_ok());
    }

    #[test]
    fn rejects_other_lengths_non_hex_and_empty() {
        for bad in [
            "",
            "abc",
            &SHA1[1..],
            &format!("{SHA1}0"),
            &SHA1.replace('0', "g"),
        ] {
            assert_eq!(CommitId::parse(bad), Err(OidError), "{bad}");
            assert_eq!(BlobOid::parse(bad), Err(OidError), "{bad}");
        }
    }

    #[test]
    fn uppercase_input_is_stored_lowercase() {
        let id = BlobOid::parse(&SHA1.to_uppercase()).unwrap();
        assert_eq!(id.as_str(), SHA1);
    }

    #[test]
    fn short_is_the_first_seven_characters() {
        assert_eq!(CommitId::parse(SHA1).unwrap().short(), "0123456");
    }

    #[test]
    fn display_is_the_full_id() {
        assert_eq!(CommitId::parse(SHA1).unwrap().to_string(), SHA1);
    }
}
