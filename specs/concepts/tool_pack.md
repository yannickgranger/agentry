# ToolPack

> Status: **draft**. Concept landed in I/1a. Loader and role integration in
> subsequent slices.

A composable bundle of role configuration. A `ToolPack` contributes
binaries, allowed-tools allowlist additions, a system prompt fragment, and
optional bootstrap script fragments to a consuming `AgentRole` at spawn
time. Multiple roles can reference the same pack, and a role can reference
multiple packs. This is the building block for profile-driven role
configuration: the long-term shape is a generic coder-claude role that
consumes a list of tool packs declared per-project, replacing the current
N-roles-per-project anti-pattern.

## ToolPack

The pack itself. Persisted as JSON; seeded into Redis under
`agentry:tool_pack:<name>:<version>` (slice I/1b). Merged into a role's
effective config at spawn time (slice I/1c). The `version` field is a
monotonic `u32`, mirroring `AgentRole`'s versioning semantics.

Slice I/1c added a `tool_packs: Vec<String>` field on `AgentRole`. The
spawner's `spawner::resolve_role_with_packs` resolves each pack name into a
`ToolPack` body fetched from Redis and applies the pure
`role::merge_role_with_packs` to produce the role's effective config before
the container starts. Today the daemon picks the latest seeded version per
name; this is a known transient (see invariants below) until profile-driven
roles in slice I/2 pin `(name, version)` pairs explicitly.

#### Operational invariants (not enforced by graph-specs)

- Additive merge. Pack contributions append to the role's existing fields;
  they never replace. A role declaring `binaries=[git]` plus a pack
  declaring `binaries=[cargo]` produces an effective `[git, cargo]`.
  Deduplication is the merge logic's responsibility.
- No pack-of-packs. Tool packs do not reference other tool packs. The
  composition layer is the consuming role; flattening packs would invite
  cycle detection and ambiguous merge order.
- Versioned. Packs version like roles. A role references a pack by
  `(name, version)` — never `latest` — so a pack update doesn't silently
  change role behavior; bumping the consumed version is an explicit role
  edit.
- Latest-version transient. Slice I/1c resolves pack references by name
  only; the daemon fetches the highest-version seeded for each name. This
  is a known transient — slice I/2's profile-driven roles will pin
  `(name, version)` pairs in `profile.toml` so a pack update doesn't
  silently change role behavior.
