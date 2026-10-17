//! Domain layer: entities, value objects, domain services, and domain errors.
//! No I/O, no serde, no clock. Depends on std and the blake3 hashing crate (for
//! chunk ids only).

pub mod chunk_id;
pub mod chunker;
pub mod identifier_splitter;
pub mod line_range;
