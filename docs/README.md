# docs/

This index lists every doc under `docs/` with a one-or-two-sentence purpose statement. New docs MUST add an entry here in the same PR they are added — orphan docs are a recurring failure mode and the index is the structural fence against it.

## Top-level

- `PROPOSAL.md` — Original 2026-04-23 archaeology-synthesis proposal that seeded agentry. Read for the motivation behind the orchestrator-v2 shape; not an authoritative roadmap (the M0–M9 framing is retired).
- `dogfood-protocol.md` — How to plan, dispatch, observe, and respond to briefs in `agentry-self-host-v0`. Read before authoring any brief on `yg/agentry`.
- `captain-doctrine.md` — Operator protocols beyond brief dispatch — currently the redeploy protocol after merging briefs that touch the daemon, captain CLI, orchestrator CLI, or role-runner binaries. Read before merging such briefs.
- `architectural-control-loop.md` — How non-trivial features traverse the council → spec → brief → ship pipeline so architectural correctness becomes a CI fence. Read before opening a council or scoping a non-trivial feature.
- `substrate-isolation-and-profile.md` — How a brief's container is assembled and how each target_repo declares its competence via `.agentry/profile.toml`. Read when changing role JSONs, packs, or profile contracts.
- `public_api_allowlist.toml` — Allowlist of pub items exempt from the role-runtime precommit gate. Edited by briefs that intentionally expand the public surface; consumed at runtime by the coder role.

## Subdirectories

- `forensics/orphan_pattern.md` — Catalogued cases of substrate orphans and verdict divergence collected for the lifecycle FSM EPIC (#246). Cited in code (daemon.rs, reaper_ports.rs) as rationale for the resume-on-restart and reaper paths.
- `conventions/split_brain_caller_check.md` — Convention forbidding new pub items that have zero callers. Enforced by `FenceKind::CallersZero` in `agentry-role-runtime`; this doc is the human-readable explanation.
- `conventions/test_separation.md` — Convention forbidding `#[cfg(test)]` blocks inline in `src/`. Enforced by `.cfdb/queries/arch-ban-inline-cfg-test-in-src.cypher`; this doc is the human-readable explanation.

## Out of scope

`docs/archaeology/` is preserved untouched for a separate decision — per operator direction, future history/archaeology content moves into Meilisearch indexes rather than into the repo. Existing archaeology files are not indexed here pending that migration.
