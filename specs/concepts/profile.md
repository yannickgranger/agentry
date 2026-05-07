# Profile

> Status: **draft**. Schema landed in I/2a. Daemon consumption in I/2b/I/2c.

A Profile is a per-project configuration manifest read from
`.agentry/profile.toml` in each target_repo. Profiles declare which tool
packs the project's coder and reviewer consume, the canonical brief
acceptance command, and which methodology gates apply to dispatched
briefs. Profiles are checked into the target_repo so they're versioned
with the project's code — diffable, reviewable, owned by the project.

This is the configuration source for profile-driven roles: a generic
coder-claude role consumes whatever pack list the target_repo's profile
declares, replacing the N-projects-equals-N-role-files anti-pattern.
agentry-side roles become project-agnostic.

## Profile

Top-level container. Aggregates coder, reviewer, acceptance, and
methodology sub-sections. Each sub-section is optional; profiles can
ship partial coverage.

- depends on: ProfileRoleSection
- depends on: ProfileAcceptanceSection
- depends on: ProfileMethodologySection

## ProfileRoleSection

Per-role config. Used for both `[coder]` and `[reviewer]` sections in
`profile.toml`. Currently carries one field: `tool_packs`. Future fields
(model override, system prompt prefix, custom permits) will follow the
same pattern — opt-in additions, defaulting to no-op.

## ProfileAcceptanceSection

The brief acceptance command default. When a brief is dispatched against
this target_repo without an explicit `payload.acceptance`, the daemon
fills in the profile's default. The brief author can still override
per-brief.

## ProfileMethodologySection

Methodology gates to invoke. Maps to skill names like `"discover"`,
`"prescribe"`, `"prepare-issue"`, `"verify-issue"`. The methodology
runner (slice I/5) will sequence these as pre-conditions before the
coder accepts the brief.

## ProfileParseError

Typed error returned by the pure profile parser. Wraps `toml::de::Error`
plus any additional validation errors layered on later. The parser does
no I/O; fetching the profile content from disk or forge is a downstream
concern (slice I/2b).

#### Operational invariants (not enforced by graph-specs)

- Profile is checked-in to target_repo. The substrate fetches it via
  forge API at brief dispatch (slice I/2b); profiles are not transmitted
  via the brief itself. This enforces "project owns its config" —
  captain dispatches a brief, daemon discovers project requirements
  from the project's own checked-in manifest.
- `deny_unknown_fields` on every level. Typos in section names or field
  names error at parse time. No silent best-effort.
- Optional everywhere. A profile with only `[coder]` tool_packs is
  valid; missing sections default to empty. Avoids forcing every
  project to specify every dimension.
