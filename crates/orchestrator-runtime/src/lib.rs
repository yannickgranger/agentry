//! agentry orchestrator runtime.
//!
//! The daemon reads briefs from Redis, spawns ephemeral containers per the
//! team's roles, routes messages between them per the team's graph, and
//! records verdicts. The orchestrator does not know what the roles *mean*;
//! it only runs the graph.

#![forbid(unsafe_code)]

pub mod cli_agents;
pub mod cli_roles;
pub mod cli_teams;
pub mod config;
pub mod daemon;
pub mod delivery;
pub mod errors;
pub mod lifecycle;
pub mod lifecycle_driver;
pub mod permit;
pub mod projector;
pub mod reaper;
pub mod redis_io;
pub mod role_dir_loader;
pub mod seed;
pub mod spawner;
pub mod state;
pub mod submit_shape_check;
pub mod team_validator;
pub mod transcript;
pub mod watchdog;
pub mod workspace;

pub use config::Config;

pub use errors::{Error, Result};
