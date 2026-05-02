//! Public-surface tests for the seed module.
//!
//! `seed.rs` is dominated by private bash-script string constants and
//! private `build_*_role` helpers. The migration recipe forbids promoting
//! their visibility, so the inline assertions over `BASH_PRELUDE`,
//! `*_AGENTRY_SCRIPT`, and `build_*_role` outputs are dropped — those
//! invariants are exercised end-to-end by `orchestrator seed` against a
//! live Redis. What survives here is the public on-disk role-JSON contract
//! that the role_dir_loader exercises at every seed, plus the
//! `agentry-self-host-v0` topology shape (constructed inline via
//! `orchestrator_types` only — keep in sync with `seed::seed_m0`).

use orchestrator_types::{AgentRole, MessageEdge, RoleName, RoleRef, TeamName, TeamTopology};
use std::path::PathBuf;

/// Issue #175: roles whose entrypoint exec's a host-built runner binary
/// must run on an image whose glibc is at least the host build's
/// compile-target. `debian:trixie-slim` (glibc 2.41) satisfies a Fedora 43
/// (glibc 2.42) host build; the prior `bookworm-slim` (glibc 2.36) failed
/// dynamic-linker resolution before `main()` and silently exited.
#[test]
fn runner_host_roles_use_glibc_compatible_image() {
    const RUNNER_HOST_IMAGE: &str = "docker.io/library/debian:trixie-slim";
    let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let seed_roles = manifest
        .parent()
        .and_then(|p| p.parent())
        .expect("workspace root from CARGO_MANIFEST_DIR")
        .join("seed/roles");
    let files = [
        "reviewer-claude-agentry-v1.json",
        "ac-verifier-claude-agentry-v1.json",
        "ac-verifier-gemini-agentry-v1.json",
        "ac-verifier-grok-agentry-v1.json",
    ];
    for file in files {
        let path = seed_roles.join(file);
        let text = std::fs::read_to_string(&path)
            .unwrap_or_else(|e| panic!("read {}: {e}", path.display()));
        let role: AgentRole =
            serde_json::from_str(&text).unwrap_or_else(|e| panic!("parse {}: {e}", path.display()));
        assert_eq!(
            role.image, RUNNER_HOST_IMAGE,
            "role '{}' (from {file}) must use {RUNNER_HOST_IMAGE} — see #175",
            role.name
        );
        assert!(
            role.entrypoint_script.contains("exec /usr/local/bin/")
                && role.entrypoint_script.contains("-runner"),
            "role '{}' (from {file}) is expected to exec a host-built runner binary",
            role.name
        );
    }
}

#[test]
fn agentry_self_host_v0_topology_has_ac_verifier_with_correct_edges() {
    // Mirror of the agentry-self-host-v0 topology block in seed_m0 — built
    // here so the dual-inbound ordering invariant is covered without
    // touching Redis. Keep in sync with seed_m0. Roles are referenced by
    // (name, version); the six Rust-runner roles are now JSON-defined and
    // registered by the role_dir_loader.
    let coder_ref = RoleRef {
        name: RoleName("coder-claude-agentry".into()),
        version: 1,
    };
    let ac_verifier_ref = RoleRef {
        name: RoleName("ac-verifier-claude-agentry".into()),
        version: 1,
    };
    let ac_verifier_gemini_ref = RoleRef {
        name: RoleName("ac-verifier-gemini-agentry".into()),
        version: 1,
    };
    let ac_verifier_grok_ref = RoleRef {
        name: RoleName("ac-verifier-grok-agentry".into()),
        version: 1,
    };
    let reviewer_claude_ref = RoleRef {
        name: RoleName("reviewer-claude-agentry".into()),
        version: 1,
    };
    let reviewer_mechanical_ref = RoleRef {
        name: RoleName("reviewer-mechanical-agentry".into()),
        version: 1,
    };
    let shipper_ref = RoleRef {
        name: RoleName("shipper-agentry".into()),
        version: 1,
    };
    let ci_watcher_ref = RoleRef {
        name: RoleName("ci-watcher-agentry".into()),
        version: 1,
    };

    let topology = TeamTopology {
        name: TeamName("agentry-self-host-v0".into()),
        version: 1,
        roles: vec![
            coder_ref.clone(),
            ac_verifier_ref.clone(),
            ac_verifier_gemini_ref.clone(),
            ac_verifier_grok_ref.clone(),
            reviewer_mechanical_ref.clone(),
            reviewer_claude_ref.clone(),
            shipper_ref.clone(),
            ci_watcher_ref.clone(),
        ],
        message_graph: vec![
            MessageEdge {
                from: coder_ref.clone(),
                to: reviewer_mechanical_ref.clone(),
                permit_overrides_from: None,
                rework_target: None,
            },
            MessageEdge {
                from: coder_ref.clone(),
                to: reviewer_claude_ref.clone(),
                permit_overrides_from: None,
                rework_target: None,
            },
            MessageEdge {
                from: coder_ref.clone(),
                to: ac_verifier_ref.clone(),
                permit_overrides_from: None,
                rework_target: None,
            },
            MessageEdge {
                from: ac_verifier_ref.clone(),
                to: reviewer_mechanical_ref.clone(),
                permit_overrides_from: None,
                rework_target: None,
            },
            MessageEdge {
                from: ac_verifier_ref.clone(),
                to: reviewer_claude_ref.clone(),
                permit_overrides_from: None,
                rework_target: None,
            },
            MessageEdge {
                from: coder_ref.clone(),
                to: ac_verifier_gemini_ref.clone(),
                permit_overrides_from: None,
                rework_target: None,
            },
            MessageEdge {
                from: coder_ref.clone(),
                to: ac_verifier_grok_ref.clone(),
                permit_overrides_from: None,
                rework_target: None,
            },
            MessageEdge {
                from: ac_verifier_gemini_ref.clone(),
                to: reviewer_mechanical_ref.clone(),
                permit_overrides_from: None,
                rework_target: None,
            },
            MessageEdge {
                from: ac_verifier_gemini_ref.clone(),
                to: reviewer_claude_ref.clone(),
                permit_overrides_from: None,
                rework_target: None,
            },
            MessageEdge {
                from: ac_verifier_grok_ref.clone(),
                to: reviewer_mechanical_ref.clone(),
                permit_overrides_from: None,
                rework_target: None,
            },
            MessageEdge {
                from: ac_verifier_grok_ref.clone(),
                to: reviewer_claude_ref.clone(),
                permit_overrides_from: None,
                rework_target: None,
            },
            MessageEdge {
                from: reviewer_mechanical_ref.clone(),
                to: shipper_ref.clone(),
                permit_overrides_from: None,
                rework_target: None,
            },
            MessageEdge {
                from: reviewer_claude_ref.clone(),
                to: shipper_ref.clone(),
                permit_overrides_from: None,
                rework_target: None,
            },
            MessageEdge {
                from: shipper_ref.clone(),
                to: ci_watcher_ref.clone(),
                permit_overrides_from: None,
                rework_target: None,
            },
        ],
        terminal_role: ci_watcher_ref.clone(),
        max_retries: 2,
    };

    assert!(
        topology.roles.contains(&ac_verifier_ref),
        "ac-verifier-claude-agentry must be in roles"
    );

    let edge_idx = |from: &RoleRef, to: &RoleRef| -> Option<usize> {
        topology
            .message_graph
            .iter()
            .position(|e| e.from == *from && e.to == *to)
    };

    let coder_to_acv = edge_idx(&coder_ref, &ac_verifier_ref);
    let acv_to_rev_mech = edge_idx(&ac_verifier_ref, &reviewer_mechanical_ref);
    let acv_to_rev_claude = edge_idx(&ac_verifier_ref, &reviewer_claude_ref);
    let coder_to_rev_mech = edge_idx(&coder_ref, &reviewer_mechanical_ref);
    let coder_to_rev_claude = edge_idx(&coder_ref, &reviewer_claude_ref);

    assert!(coder_to_acv.is_some(), "coder→ac-verifier edge must exist");
    assert!(
        acv_to_rev_mech.is_some(),
        "ac-verifier→reviewer-mechanical edge must exist"
    );
    assert!(
        acv_to_rev_claude.is_some(),
        "ac-verifier→reviewer-claude edge must exist"
    );
    assert!(
        coder_to_rev_mech.is_some(),
        "coder→reviewer-mechanical edge must exist (rework target)"
    );

    // Brief 5 of #134: gemini + grok wired as parallel siblings.
    assert!(
        topology.roles.contains(&ac_verifier_gemini_ref),
        "ac-verifier-gemini-agentry must be in roles"
    );
    assert!(
        topology.roles.contains(&ac_verifier_grok_ref),
        "ac-verifier-grok-agentry must be in roles"
    );

    let coder_to_acv_gemini = edge_idx(&coder_ref, &ac_verifier_gemini_ref);
    let coder_to_acv_grok = edge_idx(&coder_ref, &ac_verifier_grok_ref);
    let acv_gemini_to_rev_mech = edge_idx(&ac_verifier_gemini_ref, &reviewer_mechanical_ref);
    let acv_gemini_to_rev_claude = edge_idx(&ac_verifier_gemini_ref, &reviewer_claude_ref);
    let acv_grok_to_rev_mech = edge_idx(&ac_verifier_grok_ref, &reviewer_mechanical_ref);
    let acv_grok_to_rev_claude = edge_idx(&ac_verifier_grok_ref, &reviewer_claude_ref);

    assert!(
        coder_to_acv_gemini.is_some(),
        "coder→ac-verifier-gemini edge must exist"
    );
    assert!(
        coder_to_acv_grok.is_some(),
        "coder→ac-verifier-grok edge must exist"
    );
    assert!(
        acv_gemini_to_rev_mech.is_some(),
        "ac-verifier-gemini→reviewer-mechanical edge must exist"
    );
    assert!(
        acv_gemini_to_rev_claude.is_some(),
        "ac-verifier-gemini→reviewer-claude edge must exist"
    );
    assert!(
        acv_grok_to_rev_mech.is_some(),
        "ac-verifier-grok→reviewer-mechanical edge must exist"
    );
    assert!(
        acv_grok_to_rev_claude.is_some(),
        "ac-verifier-grok→reviewer-claude edge must exist"
    );

    // Dual-inbound ordering invariant: coder→reviewer-mechanical MUST
    // appear BEFORE ac-verifier→reviewer-mechanical so the daemon's
    // `team.incoming(reviewer).first()` rework lookup rewinds to the
    // coder, not the (non-corrective) ac-verifier.
    let coder_pos =
        coder_to_rev_mech.expect("coder→reviewer-mechanical edge already asserted present");
    let acv_pos =
        acv_to_rev_mech.expect("ac-verifier→reviewer-mechanical edge already asserted present");
    assert!(
        coder_pos < acv_pos,
        "coder→reviewer-mechanical must appear before ac-verifier→reviewer-mechanical (rework rewinds to coder, not ac-verifier)"
    );

    // Extended ordering invariant across all three verifier variants:
    // every coder→reviewer edge index must be less than every
    // ac-verifier-*→reviewer edge index.
    let coder_to_reviewer_indices: Vec<usize> = [coder_to_rev_mech, coder_to_rev_claude]
        .iter()
        .map(|opt| opt.expect("coder→reviewer edge already asserted present"))
        .collect();
    let acv_to_reviewer_indices: Vec<usize> = [
        acv_to_rev_mech,
        acv_to_rev_claude,
        acv_gemini_to_rev_mech,
        acv_gemini_to_rev_claude,
        acv_grok_to_rev_mech,
        acv_grok_to_rev_claude,
    ]
    .iter()
    .map(|opt| opt.expect("ac-verifier→reviewer edge already asserted present"))
    .collect();
    for c_idx in &coder_to_reviewer_indices {
        for a_idx in &acv_to_reviewer_indices {
            assert!(
                c_idx < a_idx,
                "coder→reviewer edge at {c_idx} must precede ac-verifier→reviewer edge at {a_idx}"
            );
        }
    }
}

#[test]
fn agentry_self_host_v0_topology_has_all_three_ac_verifiers_wired_in_parallel() {
    // Mirror of the agentry-self-host-v0 topology block in seed_m0 — built
    // here so the parallel-verifier wiring is covered without touching
    // Redis. Keep in sync with seed_m0.
    let coder_ref = RoleRef {
        name: RoleName("coder-claude-agentry".into()),
        version: 1,
    };
    let ac_verifier_claude_ref = RoleRef {
        name: RoleName("ac-verifier-claude-agentry".into()),
        version: 1,
    };
    let ac_verifier_gemini_ref = RoleRef {
        name: RoleName("ac-verifier-gemini-agentry".into()),
        version: 1,
    };
    let ac_verifier_grok_ref = RoleRef {
        name: RoleName("ac-verifier-grok-agentry".into()),
        version: 1,
    };
    let reviewer_claude_ref = RoleRef {
        name: RoleName("reviewer-claude-agentry".into()),
        version: 1,
    };
    let reviewer_mechanical_ref = RoleRef {
        name: RoleName("reviewer-mechanical-agentry".into()),
        version: 1,
    };
    let shipper_ref = RoleRef {
        name: RoleName("shipper-agentry".into()),
        version: 1,
    };
    let ci_watcher_ref = RoleRef {
        name: RoleName("ci-watcher-agentry".into()),
        version: 1,
    };

    let topology = TeamTopology {
        name: TeamName("agentry-self-host-v0".into()),
        version: 1,
        roles: vec![
            coder_ref.clone(),
            ac_verifier_claude_ref.clone(),
            ac_verifier_gemini_ref.clone(),
            ac_verifier_grok_ref.clone(),
            reviewer_mechanical_ref.clone(),
            reviewer_claude_ref.clone(),
            shipper_ref.clone(),
            ci_watcher_ref.clone(),
        ],
        message_graph: vec![
            MessageEdge {
                from: coder_ref.clone(),
                to: reviewer_mechanical_ref.clone(),
                permit_overrides_from: None,
                rework_target: None,
            },
            MessageEdge {
                from: coder_ref.clone(),
                to: reviewer_claude_ref.clone(),
                permit_overrides_from: None,
                rework_target: None,
            },
            MessageEdge {
                from: coder_ref.clone(),
                to: ac_verifier_claude_ref.clone(),
                permit_overrides_from: None,
                rework_target: None,
            },
            MessageEdge {
                from: ac_verifier_claude_ref.clone(),
                to: reviewer_mechanical_ref.clone(),
                permit_overrides_from: None,
                rework_target: None,
            },
            MessageEdge {
                from: ac_verifier_claude_ref.clone(),
                to: reviewer_claude_ref.clone(),
                permit_overrides_from: None,
                rework_target: None,
            },
            MessageEdge {
                from: coder_ref.clone(),
                to: ac_verifier_gemini_ref.clone(),
                permit_overrides_from: None,
                rework_target: None,
            },
            MessageEdge {
                from: coder_ref.clone(),
                to: ac_verifier_grok_ref.clone(),
                permit_overrides_from: None,
                rework_target: None,
            },
            MessageEdge {
                from: ac_verifier_gemini_ref.clone(),
                to: reviewer_mechanical_ref.clone(),
                permit_overrides_from: None,
                rework_target: None,
            },
            MessageEdge {
                from: ac_verifier_gemini_ref.clone(),
                to: reviewer_claude_ref.clone(),
                permit_overrides_from: None,
                rework_target: None,
            },
            MessageEdge {
                from: ac_verifier_grok_ref.clone(),
                to: reviewer_mechanical_ref.clone(),
                permit_overrides_from: None,
                rework_target: None,
            },
            MessageEdge {
                from: ac_verifier_grok_ref.clone(),
                to: reviewer_claude_ref.clone(),
                permit_overrides_from: None,
                rework_target: None,
            },
            MessageEdge {
                from: reviewer_mechanical_ref.clone(),
                to: shipper_ref.clone(),
                permit_overrides_from: None,
                rework_target: None,
            },
            MessageEdge {
                from: reviewer_claude_ref.clone(),
                to: shipper_ref.clone(),
                permit_overrides_from: None,
                rework_target: None,
            },
            MessageEdge {
                from: shipper_ref.clone(),
                to: ci_watcher_ref.clone(),
                permit_overrides_from: None,
                rework_target: None,
            },
        ],
        terminal_role: ci_watcher_ref.clone(),
        max_retries: 2,
    };

    // (a) all three verifier role refs present.
    assert!(
        topology.roles.contains(&ac_verifier_claude_ref),
        "ac-verifier-claude-agentry must be in roles"
    );
    assert!(
        topology.roles.contains(&ac_verifier_gemini_ref),
        "ac-verifier-gemini-agentry must be in roles"
    );
    assert!(
        topology.roles.contains(&ac_verifier_grok_ref),
        "ac-verifier-grok-agentry must be in roles"
    );

    // (b) coder fans out to all three verifiers.
    let coder_to_verifier_count = topology
        .message_graph
        .iter()
        .filter(|e| {
            e.from == coder_ref
                && (e.to == ac_verifier_claude_ref
                    || e.to == ac_verifier_gemini_ref
                    || e.to == ac_verifier_grok_ref)
        })
        .count();
    assert_eq!(
        coder_to_verifier_count, 3,
        "coder must fan out to all three ac-verifier variants (claude, gemini, grok)"
    );

    // (c) each verifier signals both reviewers (six edges total).
    let verifier_to_reviewer_count = topology
        .message_graph
        .iter()
        .filter(|e| {
            (e.from == ac_verifier_claude_ref
                || e.from == ac_verifier_gemini_ref
                || e.from == ac_verifier_grok_ref)
                && (e.to == reviewer_mechanical_ref || e.to == reviewer_claude_ref)
        })
        .count();
    assert_eq!(
        verifier_to_reviewer_count, 6,
        "each ac-verifier variant must signal both reviewers (3 verifiers × 2 reviewers = 6)"
    );
}
