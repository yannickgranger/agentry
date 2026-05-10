# Target repo

> Status: **draft**. Council-authored 2026-05-09 to canonicalize the
> routing-key concept currently scattered across `brief.payload.target_repo:
> String`, `Project.repo_url: Option<String>`, and `Project.forges:
> Vec<String>`. Type lands in this brief; migration of the 60+ existing
> call sites proceeds brief-by-brief per the migration plan below; new
> ad-hoc construction is fenced by `.cfdb/queries/arch-ban-target-repo-*.cypher`.

`target_repo` names the repository a brief acts on. It is the routing
key for the daemon, the cfdb keyspace selector, the credential-scope
key, the workspace-mount target, and the forge-URL source. Until this
spec, the concept was invoked in seven other concept docs but DECLARED
in none — `brief.payload.target_repo` was a bare
`serde_json::Value::get("target_repo")` read with `.unwrap_or("_unknown")`
fallback, with three independent parse functions and a one-way slug
derivation that collided on legitimate inputs. This spec declares the
canonical type, its invariants, and the in-flight migration shape.

## TargetRepo

The routing-key value object. A `TargetRepo` carries the resolved
forge identity, owner, and repo name as typed fields (`forge`, `owner`,
`repo` — all string-shaped today; `forge` becomes a typed `ForgeId`
newtype in a follow-up Configuration council).

Construction is monopolistic: `TargetRepo::from_str` is the sole
parser. It accepts both bare `owner/repo` (binding `forge` from
`ForgeConfig.default_host` at parse time and capturing the resolved
value into the type) and forge-qualified `forge:owner/repo` (forward-
compatible with the eventual Shape C migration on `Project.forges[]`).

`TargetRepo::Display` emits the bare `owner/repo` form for wire
compatibility with existing brief payloads and dispatched briefs in
flight. A separate `TargetRepo::display_qualified()` method emits
`forge:owner/repo` for new internal call sites that want the
self-contained form.

Methods on `TargetRepo`:

- `forge(&self) -> &ForgeId` — the resolved forge identifier
- `owner(&self) -> &str` — the owner segment
- `repo(&self) -> &str` — the repo segment
- `slug(&self) -> String` — the filesystem- and keyspace-safe slug
  (collision-resistant — see Operational invariants)
- `clone_url(&self, forge_host: &str) -> String` — the canonical clone
  URL builder; the SOLE legitimate site for composing target_repo into
  a `https://.../<owner>/<repo>.git` string
- `cfdb_keyspace(&self) -> String` — the cfdb keyspace name (delegates
  to `slug()` today; method exists so future extension can change the
  derivation in one place)

`TargetRepo` implements `serde::Serialize` (emits the bare wire form)
and `serde::Deserialize` (delegates to `FromStr`). Round-trip is
preserved: a `TargetRepo` deserialized from the wire and re-serialized
produces the same bytes.

## TargetRepoParseError

Typed parse error for `TargetRepo::from_str`. Variants distinguish
the failure modes a malformed routing key can take: `Empty`,
`MissingOwner`, `MissingRepo`, `OwnerInvalidChars`, `RepoInvalidChars`,
`OwnerStartsWithDotOrDash`, `RepoStartsWithDotOrDash`, `TooLong`,
`UnknownForgePrefix`.

Variant carries no payload — the error is structural, not contextual.
Callers needing context (e.g. `ProfileFetchError::MalformedTargetRepo`)
wrap the variant with their own context layer; the error type itself
contains zero infrastructure types (no `reqwest::Error`, no
`base64::DecodeError`, no `serde_json::Error`). This is a port-purity
fix from the current `redis_io::ProfileFetchError` enum which mixes
parse errors with transport errors.

#### Operational invariants (not enforced by graph-specs)

- **Construction is monopolistic.** Every `TargetRepo` is constructed
  by `TargetRepo::from_str` or its serde `Deserialize` impl (which
  delegates to `from_str`). No alternative parse paths in production
  code.

- **Wire shape is bare `owner/repo`.** `brief.payload.target_repo` is
  a JSON string in `<owner>/<repo>` form. The forge identity is bound
  at parse time from `ForgeConfig.default_host` and captured into the
  typed value; it is not re-resolved per use site. This preserves
  wire compatibility with every dispatched brief in flight while
  giving downstream callers a self-contained typed value.

- **Charset and shape (parse-time validation).** Owner and repo each
  match `^[A-Za-z0-9._-]{1,64}$`; neither starts with `.` or `-`.
  Total length ≤ 200 bytes. Inputs failing this rule are rejected at
  intake; they never reach slug derivation, clone-URL construction,
  or permit-mint.

- **No `_unknown` fallback.** A brief whose payload lacks
  `target_repo` is rejected at intake with
  `IntakeError::MissingTargetRepo`. There is no shared keyspace, no
  fallback slug, no `_unknown` literal anywhere in source. The
  `unwrap_or("_unknown")` patterns at `intake_validation.rs:55` and
  `daemon.rs:188` are removed in this brief.

- **Owner-allowlist intersection at intake.** The brief's
  `target_repo.owner()` MUST be present in
  `cfg.forge.allowed_owners` before intake admits the brief.
  Out-of-allowlist owners are rejected before clone, extract, or
  spawn — defense in depth ahead of the permit-broker
  `forge:write:<owner>/*` gate. This closes the
  permit-scope-decoupled gap where a brief targeting an
  out-of-allowlist owner currently reaches filesystem extraction,
  clone (with token), and container spawn before the eventual
  `forge:write` is blocked.

- **Slug derivation is collision-resistant.** `TargetRepo::slug()`
  first encodes `_` → `__` in the input, then applies the byte map
  (replace non-alphanumeric/`_` with `_`). This makes derivation
  injective over `[A-Za-z0-9._-]/[A-Za-z0-9._-]`: distinct
  `(owner, repo)` tuples produce distinct slugs. The cache marker
  (`<slug>.head_sha`) and cfdb keyspace are now collision-free,
  closing the cross-target data-leak path where `yg/foo` and
  `yg_foo` collapsed to the same slug.

- **No raw `target_repo` interpolation into URLs.** Clone-URL
  construction goes through `TargetRepo::clone_url(&forge_host)`. The
  pattern `format!("https://...{target_repo}.git")` is forbidden in
  production code. This closes the token-exfiltration vector at
  `daemon.rs:1470-1475` where a `target_repo` like
  `yg/agentry@evil.example.com/foo` could redirect the
  token-bearing clone to an attacker-controlled host.

- **`Brief::target_repo()` is the sole accessor.** Direct
  `brief.payload.get("target_repo")` outside
  `orchestrator-types::brief` is a cfdb ban-rule violation after the
  role-runner sweep completes (brief 3 of the migration). Until
  then it remains an operational invariant for new code.

- **`Project.repo_url` is a derived field.** Computed from the
  primary `TargetRepo` + forge host at workspace-allocation time.
  Not stored independently going forward. Slated for removal in a
  follow-up Project/Registry council.

- **`Project.forges[]` is a different concept.** A 1:N enumeration of
  forge identities the project participates in, NOT a list of
  routing keys. NOT a synonym for `target_repo`. The forge-prefixed
  shorthand `agency:yg/qbot-core` lives there as a project-level
  enumeration; the per-brief routing key is always a single
  `TargetRepo`.

#### Context mapping

`TargetRepo` is the **Published Language** of the Briefing context.
Every other context is a consumer; none should redefine the concept.

- **Briefing → Intake validation:** Conformist. Intake validation
  reads `Brief::target_repo()` without translation. The structural
  validation (charset, length, non-empty segments) is enforced by
  `TargetRepo::from_str` at brief construction, not by the intake
  adapter.

- **Briefing → Profile:** Conformist. The profile fetcher calls
  `target_repo.owner()` and `target_repo.repo()` to construct the
  forge API URL. No translation layer needed — the Profile adapter
  reads the Published Language directly.

- **Briefing → Anchor resolver:** Conformist. The anchor resolver
  calls `target_repo.slug()` to derive the cfdb keyspace name. One
  call, no re-parsing.

- **Briefing → Permits (shared kernel on owner):** Permit-mint code
  in Permits depends on `target_repo.owner()` to derive
  `forge:write:<owner>/*` scope. The shared-kernel relationship
  means a change to owner-component shape (e.g. introducing
  org/sub-org hierarchy) requires coordinated changes in both
  contexts.

- **Briefing → Configuration (anti-corruption layer):** The
  `ForgeId` component of `TargetRepo` is resolved at intake from
  `cfg.forge.default_host` (Shape A bare input) or parsed inline
  (Shape C `forge:owner/repo` input). The Configuration context's
  `ForgeConfig` is a Configuration concept; the resolution happens
  at the Briefing boundary so downstream consumers see only the
  resolved typed value.

- **Briefing → Registry/Project:** Conformist (light ACL).
  `Project.forges[]` entries will be typed as `Vec<TargetRepo>` after
  a follow-up migration. Until then, the Registry context holds a
  transitional string-form that the adapter parses into `TargetRepo`
  at resolution time. The single-method ACL (`TargetRepo::from_str`)
  is the same parser used everywhere else.

#### Aliases

The following names appear in the codebase, specs, or operator docs
and refer either to the same routing-key concept (deprecated) or to
related-but-distinct concepts (kept). Enumerated exhaustively per
council survey.

**Same concept, different encodings (deprecated):**

- `repo_url` — full clone URL (`https://forge.example/owner/repo.git`).
  Lives on `Project` as a typed `Option<String>`. Same concept,
  encoded as a URL rather than a slug. Slated for removal in a
  follow-up Project council; rename candidate `repo_clone_url` to
  disambiguate from the routing-key vocabulary in the interim.

- `clone_url` — derived value computed at the dispatch boundary from
  `(target_repo, forge_host)`. Adapter-layer artifact, never the
  canonical name. Constructed at the boundary, not propagated.

- `repo_name` — the `name` half of `org/name`. Accessor on
  `TargetRepo` (`repo()`), not a separate field.

- `repo` standalone — forbidden in new code (too ambiguous; every
  code site has *a* repo). Always qualify as `target_repo`.

**Considered and rejected:**

- `routing_target`, `routing_key`, `repo_slug`, `target_slug`,
  `workspace_repo`, `project_repo`, `forge_repo`, `repo_id`,
  `target_owner` — all unattested in the codebase (zero hits in
  `crates/`, `specs/`, `docs/`). Picking any of them would be a
  top-down rename against zero adoption, sacrificing the
  ubiquitous-language property that took 50+ briefs to establish.

- `tenant`, `tenant_id` — appear only in narrative aspiration docs
  (`docs/PROPOSAL.md:36`); never reached code or live specs.
  Forbidden in new code.

**Related-but-distinct concepts (kept, NOT aliases):**

- `Project` / `ProjectSlug` — a higher-level grouping. A `Project`
  carries a routing key (its primary `TargetRepo`) plus standing
  orders, budget, default topology. SUPERSET of the routing key,
  never a synonym. Briefs may carry `project: Option<ProjectSlug>`
  OR `target_repo` in the payload; when both are present, the
  project resolves to its `TargetRepo` and that wins.

- `Project.forges` — a project's 1:N enumeration of forge
  identities (`agency:yg/qbot-core`-style shorthand). Different
  shape, different concept. NOT a list of routing keys.

#### Migration plan

This brief (brief 1 of the canonical sequence) lands the spec, the
typed surface, the security gates, and two cfdb rules. The remaining
two briefs follow:

- **Brief 2** — sweep role-runner direct payload reads. Adds
  `agentry-role-runtime::ParsedBundle::target_repo()` typed bundle
  helper. Migrates 7+ runners (planner, pr_rebaser, shipper,
  ci_watcher, preflight_criterion, auditor, coder-precommit). No
  new ban rule.

- **Brief 3** — land the broad `payload.get` ban
  (`arch-ban-target-repo-payload-get-outside-accessor.cypher`).
  Single-file brief; lands at zero violations once brief 2
  completes the sweep.

The dogfood team's own use of `target_repo: "yg/agentry"` MUST remain
wire-compatible across all three briefs. The JSON payload shape is
`{"target_repo": "<owner>/<repo>", ...}` and remains valid input to
the daemon throughout. Renaming the JSON key, adding a required
top-level `target_repo` field on `Brief`, or removing the
`payload.target_repo` key are all FORBIDDEN until a separate council
ratifies the wire-shape change with a documented compatibility
window.
