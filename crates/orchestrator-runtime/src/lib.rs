//! agentry orchestrator runtime.
//!
//! The daemon reads briefs from Redis, spawns ephemeral containers per the
//! team's roles, routes messages between them per the team's graph, and
//! records verdicts. The orchestrator does not know what the roles *mean*;
//! it only runs the graph.

#![forbid(unsafe_code)]

pub mod daemon;
pub mod errors;
pub mod redis_io;
pub mod seed;
pub mod spawner;

pub use errors::{Error, Result};
