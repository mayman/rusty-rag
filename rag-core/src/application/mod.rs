//! Application layer: use cases, request/response DTOs, and port traits.
//! Depends on the domain layer only.

pub mod add_source;
pub mod dto;
pub mod error;
pub mod index_pipeline;
pub mod indexer;
pub mod list_sources;
pub mod ports;
pub mod reindex;
pub mod remove_source;
pub mod retrieval;
pub mod search;
pub mod show_file;
pub mod update_source;

#[cfg(test)]
mod indexer_test_support;

#[cfg(any(test, feature = "testkit"))]
pub mod testkit;
