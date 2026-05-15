//! Infrastructure crate: configuration loading, Redis I/O, shared error type,
//! transcript parsing, and the running-container registry.
//! Extracted from orchestrator-runtime so dashboard and other read-side
//! consumers do not need to depend on the daemon monolith.
//! See PR for #457 (CA2 of clean-architecture audit).

pub mod config;
pub mod errors;
pub mod redis_io;
pub mod runtime_registry;
pub mod transcript;

pub use config::Config;
pub use errors::{Error, Result};
