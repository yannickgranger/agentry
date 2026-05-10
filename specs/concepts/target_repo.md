# Target repo

> Status: **draft**. Council-authored 2026-05-09 to canonicalize the
> routing-key concept currently scattered across `brief.payload.target_repo:
> String`, `Project.repo_url: Option<String>`, and `Project.forges:
> Vec<String>`. The typed surface (`TargetRepo`, `TargetRepoParseError`,
> `Brief::target_repo()`) lands in brief 1a; the security gates
> (intake REJECT for missing `target_repo`, owner-allowlist intersection,
> collision-resistant slug, typed clone-URL builder migration, removal of
> `unwrap_or("_unknown")` fallbacks) ship in brief 1b on top of this
> scaffold. Migration of the 60+ existing call sites proceeds brief-by-brief
> per the migration plan below; new ad-hoc construction is fenced by
> `.cfdb/queries/arch-ban-target-repo-*.cypher`.

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
parser. It accepts both bare `owner/repo` (binding `forge` to a
placeholder constant in brief 1a — brief 1b / a follow-up Configuration
council swaps in real default-forge resolution from
`ForgeConfig.default_host`) and forge-qualified `forge:owner/repo`
(forward-compatible with the eventual Shape C migration on
`Project.forges[]`).

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

Each invariant is annotated with the brief that lands it. Brief 1a lands
the typed surface, the lazy `Brief::target_repo()` accessor, and the two
cfdb fence rules listed under the migration plan. Brief 1b lands the
security gates (intake REJECT, owner-allowlist intersection, collision-
resistant slug, typed clone-URL builder migration, removal of legacy
parse fallbacks at the production call sites).

- **Construction is monopolistic.** *(Brief 1a — typed surface.)* Every
  `TargetRepo` in production code is constructed by `TargetRepo::from_str`
  or its serde `Deserialize` impl (which delegates to `from_str`). No
  alternative parse paths in production code; legacy free-fn parsers
  (`parse_target_repo`, `sanitize_target_repo_slug`) are fenced by the
  cfdb rules below — the slug free-fn remains as a transitional bridge
  with a single migration-window caller (`for_target_repo`) that
  delegates to `TargetRepo::slug()` for valid inputs.

- **Wire shape is bare `owner/repo`.** *(Brief 1a — typed surface.)*
  `brief.payload.target_repo` is a JSON string in `<owner>/<repo>` form.
  In brief 1a the forge identity is bound to a placeholder constant
  (`agency-default`) at parse time and captured into the typed value;
  brief 1b (or a follow-up Configuration council) replaces the
  placeholder with real default-forge resolution from
  `ForgeConfig.default_host`. The wire shape itself is preserved across
  every dispatched brief in flight.

- **Charset and shape (parse-time validation).** *(Brief 1a — typed
  surface.)* Owner and repo each match `^[A-Za-z0-9._-]{1,64}$`; neither
  starts with `.` or `-`. Total length ≤ 200 bytes. `TargetRepo::from_str`
  enforces the rule today. Brief 1b adds intake REJECT so failing inputs
  never reach slug derivation, clone-URL construction, or permit-mint;
  in 1a the validator is reachable but not yet hooked to intake.

- **No `_unknown` fallback.** *(Brief 1b — security gate.)* A brief
  whose payload lacks `target_repo` will be rejected at intake with
  `IntakeError::MissingTargetRepo`; the shared `_unknown` keyspace and
  fallback slug will be removed. The current `unwrap_or("_unknown")`
  patterns at `intake_validation.rs:55` and `daemon.rs:188` are NOT
  touched by brief 1a — they are scoped to brief 1b alongside the
  intake-REJECT path. Brief 1a's `Brief::target_repo()` accessor
  returns `Option<TargetRepo>` precisely because the upstream `_unknown`
  fallback still exists.

- **Owner-allowlist intersection at intake.** *(Brief 1b — security
  gate.)* The brief's `target_repo.owner()` will be required to be
  present in `cfg.forge.allowed_owners` before intake admits the brief.
  Out-of-allowlist owners will be rejected before clone, extract, or
  spawn — defense in depth ahead of the permit-broker
  `forge:write:<owner>/*` gate. Brief 1a does NOT add this check; the
  permit-scope-decoupled gap where a brief targeting an out-of-allowlist
  owner reaches filesystem extraction, clone (with token), and container
  spawn before the eventual `forge:write` is blocked remains open until
  brief 1b lands.

- **Slug derivation is collision-resistant.** *(Brief 1b — security
  gate.)* `TargetRepo::slug()` will first encode `_` → `__` in the
  input, then apply the byte map (replace non-alphanumeric/`_` with
  `_`), making derivation injective over `[A-Za-z0-9._-]/[A-Za-z0-9._-]`.
  Brief 1a deliberately does NOT add the `_` → `__` pre-encoding: the
  1a `slug()` body produces byte-identical output to the legacy
  `sanitize_target_repo_slug` to preserve cache markers
  (`<slug>.head_sha`) and cfdb keyspace names for the single in-flight
  production routing key (`yg/agentry`). The cross-target collision
  path where `yg/foo` and `yg_foo` collapse to the same slug remains
  open until brief 1b ships.

- **No raw `target_repo` interpolation into URLs.** *(Brief 1b —
  security gate.)* Clone-URL construction will go through
  `TargetRepo::clone_url(&forge_host)`; the pattern
  `format!("https://...{target_repo}.git")` will be forbidden in
  production code. Brief 1a defines the `clone_url` method on the typed
  value but does NOT migrate the existing daemon and shipper call sites
  to use it. The token-exfiltration vector at `daemon.rs:1470-1475`
  where a `target_repo` like `yg/agentry@evil.example.com/foo` could
  redirect the token-bearing clone to an attacker-controlled host
  remains open until brief 1b lands.

- **`Brief::target_repo()` is the sole accessor.** *(Brief 1a —
  accessor; brief 3 — broad ban rule.)* Brief 1a adds the lazy
  `Brief::target_repo()` accessor on `orchestrator-types::brief`.
  Direct `brief.payload.get("target_repo")` outside that module
  becomes a cfdb ban-rule violation after the role-runner sweep
  completes (brief 3 of the migration). Until then it is an
  operational invariant for new code only.

- **`Project.repo_url` is a derived field.** *(Out of scope for the
  brief 1 sequence.)* Will be computed from the primary `TargetRepo`
  + forge host at workspace-allocation time, not stored independently.
  Slated for removal in a follow-up Project/Registry council.

- **`Project.forges[]` is a different concept.** *(Documentation only —
  no behavior change in any brief.)* A 1:N enumeration of forge
  identities the project participates in, NOT a list of routing keys.
  NOT a synonym for `target_repo`. The forge-prefixed shorthand
  `agency:yg/qbot-core` lives there as a project-level enumeration;
  the per-brief routing key is always a single `TargetRepo`.

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

The canonical migration sequence is brief 1a → 1b → 2 → 3. Brief 1a is
the substrate; the remaining briefs build on it incrementally.

- **Brief 1a (this brief)** — lands the spec, the canonical
  `TargetRepo` and `TargetRepoParseError` types in
  `orchestrator-types`, the lazy `Brief::target_repo()` accessor, and
  two cfdb fence rules
  (`arch-ban-target-repo-slug-free-fn.cypher`,
  `arch-ban-parse-target-repo-free-fn.cypher`). Migrates the existing
  `parse_target_repo` callsite in `redis_io::fetch_profile` to delegate
  through `TargetRepo::from_str`, deletes `parse_target_repo`, and
  bridges `sanitize_target_repo_slug` through `TargetRepo::slug()` for
  valid inputs while preserving the legacy byte-map for inputs the
  strict validator rejects (transitional fallback, removed in brief
  1b). ZERO behavior change is visible to dispatched briefs after this
  lands: same wire shape, same slug bytes for the in-flight production
  routing key (`yg/agentry`), same forge API URL composition.

- **Brief 1b** — lands the security gates on top of the 1a substrate:
  intake REJECT for missing/malformed `target_repo` (removing the
  `unwrap_or("_unknown")` fallbacks at `intake_validation.rs:55` and
  `daemon.rs:188`), owner-allowlist intersection at intake,
  collision-resistant slug (`_` → `__` pre-encoding in
  `TargetRepo::slug()`), migration of the daemon and shipper clone
  call sites to `TargetRepo::clone_url(&forge_host)`, and removal of
  the transitional fallback in `sanitize_target_repo_slug`. May
  introduce additional cfdb fence rules covering the
  `format!("https://...{target_repo}.git")` interpolation pattern.

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
wire-compatible across all four briefs. The JSON payload shape is
`{"target_repo": "<owner>/<repo>", ...}` and remains valid input to
the daemon throughout. Renaming the JSON key, adding a required
top-level `target_repo` field on `Brief`, or removing the
`payload.target_repo` key are all FORBIDDEN until a separate council
ratifies the wire-shape change with a documented compatibility
window.
