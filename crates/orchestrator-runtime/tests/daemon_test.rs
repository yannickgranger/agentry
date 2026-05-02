//! Integration tests for the daemon.
//!
//! The daemon's pure helpers (DAG walking, rework target resolution,
//! permit minting, chain-trigger path collection, DOL verdict
//! composition) and its top-level `handle_brief` entry point are
//! crate-private. The brief migration recipe forbids both promoting
//! them and rewriting their tests through `pub fn run` (an infinite
//! XREAD loop with no single-iteration entry). Their inline coverage
//! moved with them when they stayed private; this file is intentionally
//! a placeholder so the tests/ layout matches the migrated peers.
