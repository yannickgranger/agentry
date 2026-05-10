//! Round-trip parity check for the agentry-null-v0 topology.
//!
//! Brief #329 (slice 1 of N) extracts agentry-null-v0 from a Rust literal
//! in `seed_m0` to `seed/topologies/agentry-null-v0.json`. The proof of
//! pattern is that the JSON file deserializes to a `TeamTopology` with
//! values byte-for-byte identical to the prior Rust literal — same name,
//! version, role refs, empty message graph, terminal role, and zero
//! retries. If a future brief widens the literal in one place, this test
//! fails until the JSON is widened in the other.

use orchestrator_types::{MessageEdge, RoleName, RoleRef, TeamName, TeamTopology};
use std::collections::HashMap;
use std::path::PathBuf;

fn seed_topologies_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(|p| p.parent())
        .expect("workspace root from CARGO_MANIFEST_DIR")
        .join("seed/topologies")
}

#[test]
fn agentry_null_v0_json_round_trips_to_prior_rust_literal() {
    let path = seed_topologies_dir().join("agentry-null-v0.json");
    let text =
        std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()));
    let parsed: TeamTopology =
        serde_json::from_str(&text).unwrap_or_else(|e| panic!("parse {}: {e}", path.display()));

    let null_agent_ref = RoleRef {
        name: RoleName("null-agent-agentry".into()),
        version: 1,
    };
    let expected = TeamTopology {
        name: TeamName("agentry-null-v0".into()),
        version: 1,
        roles: vec![null_agent_ref.clone()],
        message_graph: Vec::<MessageEdge>::new(),
        terminal_role: null_agent_ref,
        max_retries: 0,
        node_classes: HashMap::new(),
    };

    assert_eq!(
        parsed, expected,
        "agentry-null-v0 JSON drifted from prior Rust literal"
    );
}
