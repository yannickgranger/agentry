//! agentry orchestrator runtime.
//!
//! The daemon reads briefs from Redis, spawns ephemeral containers per the
//! team's roles, routes messages between them per the team's graph, and
//! records verdicts. The orchestrator does not know what the roles *mean*;
//! it only runs the graph.

#![forbid(unsafe_code)]

pub mod anchor_resolver;
pub mod captain_dispatch_env;
pub mod captain_freshness;
pub mod captain_ground;
pub mod captain_ground_cache;
pub mod captain_new_spec;
pub mod cli_abort;
pub mod cli_agents;
pub mod cli_decide;
pub mod cli_roles;
pub mod cli_teams;
pub mod daemon;
pub mod daemon_resume;
pub mod delivery;
pub mod intake_validation;
pub mod lifecycle;
pub mod lifecycle_driver;
pub mod lifecycle_ports;
pub mod lifecycle_redis;
pub mod permit;
pub mod projector;
pub mod reaper;
pub mod reaper_ports;
pub mod reaper_redis;
pub mod role_dir_loader;
pub mod seed;
pub mod spawner;
pub mod state;
pub mod submit_shape_check;
pub mod team_validator;
pub mod watchdog;
pub mod workspace;

pub use orchestrator_infra::{config, errors, redis_io, transcript};
pub use orchestrator_infra::{Config, Error, Result};
