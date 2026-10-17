//! IndexVersions: the tokenizer, chunker, and indexing-policy versions an index was
//! built with.
//!
//! All three come from the domain (`TOKENIZER_VERSION`, `CHUNKER_VERSION`,
//! `POLICY_VERSION`). They are stored in each source's indexed revision, so a
//! source whose stored versions differ from `current()` needs `rag reindex` before
//! it can be searched. A tokenizer change only needs the full-text fields
//! rewritten; a chunker or policy change needs the files re-read and re-chunked.

use crate::domain::chunker::CHUNKER_VERSION;
use crate::domain::identifier_splitter::TOKENIZER_VERSION;
use crate::domain::index_policy::POLICY_VERSION;

/// The versions of the rules that produced an index.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IndexVersions {
    pub tokenizer: u32,
    pub chunker: u32,
    /// Version of `IndexPolicy`: which files and bytes are accepted.
    pub policy: u32,
}

impl IndexVersions {
    /// The versions this build of the domain produces.
    pub fn current() -> Self {
        Self {
            tokenizer: TOKENIZER_VERSION,
            chunker: CHUNKER_VERSION,
            policy: POLICY_VERSION,
        }
    }

    /// Whether `other` selects and splits files the same way (same chunker and
    /// policy), so the stored chunks stay valid and only the tokenizer may differ.
    pub fn same_chunks_as(&self, other: &Self) -> bool {
        self.chunker == other.chunker && self.policy == other.policy
    }

    /// Whether an index stamped with these versions can be searched as is.
    pub fn is_current(&self) -> bool {
        *self == Self::current()
    }
}

#[cfg(test)]
mod tests {
    use super::IndexVersions;
    use crate::domain::chunker::CHUNKER_VERSION;
    use crate::domain::identifier_splitter::TOKENIZER_VERSION;
    use crate::domain::index_policy::POLICY_VERSION;

    #[test]
    fn current_versions_are_the_domain_constants() {
        assert_eq!(
            IndexVersions::current(),
            IndexVersions {
                tokenizer: TOKENIZER_VERSION,
                chunker: CHUNKER_VERSION,
                policy: POLICY_VERSION
            }
        );
    }

    #[test]
    fn only_the_current_versions_are_current() {
        let current = IndexVersions::current();
        assert!(current.is_current());
        assert!(!IndexVersions {
            tokenizer: current.tokenizer + 1,
            ..current
        }
        .is_current());
        assert!(!IndexVersions {
            chunker: current.chunker + 1,
            ..current
        }
        .is_current());
        assert!(!IndexVersions {
            policy: current.policy + 1,
            ..current
        }
        .is_current());
    }

    #[test]
    fn only_a_tokenizer_difference_leaves_the_chunks_valid() {
        let current = IndexVersions::current();
        assert!(current.same_chunks_as(&IndexVersions {
            tokenizer: current.tokenizer + 1,
            ..current
        }));
        assert!(!current.same_chunks_as(&IndexVersions {
            chunker: current.chunker + 1,
            ..current
        }));
        assert!(!current.same_chunks_as(&IndexVersions {
            policy: current.policy + 1,
            ..current
        }));
    }
}
