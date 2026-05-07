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

At spawn time the daemon augments the role's tool_packs with this
section's tool_packs (deduplicated, profile additions appended).
Augmentation applies based on role kind: profile.coder augments coder
roles, profile.reviewer augments reviewer roles. Other role kinds
(shipper, ci-watcher, ac-verifier) are not augmented in slice I/2c.

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

## ProfileFetchError

Typed error returned by the fetcher (slice I/2b) that pulls
`.agentry/profile.toml` from the target_repo via the forge contents API.
Variants distinguish the layers a fetch can fail at: a malformed
target_repo input, network/transport, non-success HTTP status, base64
decode of the content field, and TOML parse. The 404 path is NOT an
error — fetcher returns `Ok(None)` so the daemon proceeds with role
defaults.

- depends on: Error

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
- Profile fetched at brief dispatch via forge contents API. The daemon
  issues `GET https://<forge>/api/v1/repos/<owner>/<repo>/contents/.agentry/profile.toml?ref=<base_branch>`.
  404 means "no profile, use defaults"; 200 + valid TOML produces a
  Profile; other errors log a WARN and the brief proceeds with defaults.
  Slice I/2c will compose `Profile.{coder,reviewer}.tool_packs` with the
  role's tool_packs at spawn time.
- Profile augments, never replaces. The role's hardcoded tool_packs
  always run; the profile's packs are appended after. A target_repo
  profile cannot strip a pack the role declared; it can only add. This
  preserves operator-set guarantees: a coder role declaring
  `tool_packs = ["quality-fast"]` always gets quality-fast's bits, even
  if the project profile forgets to mention it.
