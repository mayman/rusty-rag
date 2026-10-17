//! Application layer: use cases, request/response DTOs, and port traits.
//! Depends on the domain layer only.

pub mod dto;
pub mod error;
pub mod ports;
pub mod retrieval;

#[cfg(any(test, feature = "testkit"))]
pub mod testkit;
