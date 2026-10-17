//! Output layer: pure renderers from application responses and errors to strings.
//! Nothing here prints or exits; `main` writes the strings and uses the exit
//! code. The responses (snippets, scores, hints, paging) are built by the use
//! cases in `rag-core`; this layer only renders them. Chunk text is untrusted
//! data: JSON escapes it, and human output strips control characters.

pub mod doctor;
pub mod error;
pub mod human;
pub mod list;
pub mod report;
pub mod search;
pub mod show;

#[cfg(test)]
mod test_support;
