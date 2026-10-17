//! ChunkId: the stable identity of a chunk.
//!
//! A chunk id is the lowercase hex of a blake3 hash (64 characters) over the
//! repository, path, locator (heading path and occurrence, or ordinal), and the
//! chunk text. `CHUNKER_VERSION` is deliberately not part of the id: a chunker
//! change that keeps the same text and boundaries keeps the same ids, and a
//! change that alters boundaries or text changes the ids through the text and
//! locator. The version gate decides whether a stored index must be rebuilt.

use std::fmt;

/// Stable identity of a chunk: 64 lowercase hex characters.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ChunkId(String);

impl ChunkId {
    /// Accepts exactly 64 lowercase hexadecimal characters; anything else is `None`.
    pub fn parse(text: &str) -> Option<Self> {
        let valid = text.len() == 64
            && text
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b));
        valid.then(|| Self(text.to_owned()))
    }

    /// Wraps a blake3 digest. Only the chunker creates ids this way.
    pub(crate) fn from_hash(hash: blake3::Hash) -> Self {
        Self(hash.to_hex().to_string())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for ChunkId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

#[cfg(test)]
mod tests {
    use super::ChunkId;

    const VALID: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

    #[test]
    fn parse_accepts_64_lowercase_hex_characters() {
        let id = ChunkId::parse(VALID).expect("valid id");
        assert_eq!(id.as_str(), VALID);
    }

    #[test]
    fn parse_rejects_uppercase_hex() {
        assert_eq!(ChunkId::parse(&VALID.to_uppercase()), None);
    }

    #[test]
    fn parse_rejects_a_wrong_length() {
        assert_eq!(ChunkId::parse(&VALID[1..]), None);
        assert_eq!(ChunkId::parse(&format!("{VALID}0")), None);
    }

    #[test]
    fn parse_rejects_non_hex_characters() {
        let bad = format!("{}g", &VALID[1..]);
        assert_eq!(ChunkId::parse(&bad), None);
    }

    #[test]
    fn parse_rejects_the_empty_string() {
        assert_eq!(ChunkId::parse(""), None);
    }

    #[test]
    fn display_writes_the_same_text_as_as_str() {
        let id = ChunkId::parse(VALID).expect("valid id");
        assert_eq!(id.to_string(), id.as_str());
    }

    #[test]
    fn ids_order_by_their_text() {
        let low = ChunkId::parse(VALID).expect("valid id");
        let high = ChunkId::parse(&"f".repeat(64)).expect("valid id");
        assert!(low < high);
    }
}
