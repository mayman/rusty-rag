//! Domain layer: entities, value objects, domain services, and domain errors.
//! No I/O, no serde, no clock. Depends on std and the blake3 hashing crate (for
//! ids only).

pub mod chunk;
pub mod chunk_id;
pub mod chunker;
pub mod git_location;
pub mod git_ref;
pub mod hit_policy;
pub mod identifier_splitter;
pub mod index_policy;
pub mod index_versions;
pub mod indexed_at;
pub mod language;
pub mod line_range;
pub mod manifest;
pub mod oid;
pub mod path_glob;
pub mod query;
pub mod repo_path;
pub mod scored_chunk;
pub mod search_filters;
pub mod source;
pub mod source_id;
pub mod tree_entry;
