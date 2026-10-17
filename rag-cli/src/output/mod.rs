//! Output layer: pure renderers from application responses and errors to strings.
//! Nothing here prints or exits; the caller writes the strings and uses the exit
//! code. Chunk text is untrusted data: JSON escapes it, and human output strips
//! control characters.

pub mod error;
pub mod human;
pub mod list;
pub mod search;
pub mod snippet;

#[cfg(test)]
mod test_support;
