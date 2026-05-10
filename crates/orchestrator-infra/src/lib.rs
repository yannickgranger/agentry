//! Infrastructure crate: configuration loading, Redis I/O, shared error type.
//! Extracted from orchestrator-runtime so dashboard and other read-side
//! consumers do not need to depend on the daemon monolith.
//! See PR for #457 (CA2 of clean-architecture audit).

pub mod config;
pub mod errors;
pub mod redis_io;

pub use config::Config;
pub use errors::{Error, Result};
