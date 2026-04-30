# Architectural-control loop

How non-trivial features land on agentry. The loop produces specs that are
machine-checkable by `cfdb` + `graph-specs`, so architectural correctness
becomes a CI fence — not a review judgment that depends on a Rust expert
catching architectural drift.

This document is the working agreement. It does not prescribe code; it
prescribes the *process that produces code*. Briefs that skip the loop and
go straight to coder are forbidden for non-trivial work — see the
threshold table at the end.

## Why this loop exists

Process gates (BDD/TDD, scope checks, "vertical slices mandatory"
prose) catch some bugs but not the deeper class. Claude as coder defaults
to:

- horizontal layers (build all the types, then all the wiring)
- parameters silently dropped or misrouted between layers
- split-brain helpers (scaffold a new fn rather than grep for the
  existing one)
- compile-driven discovery (let the compiler tell you the API, instead of
  reading source)
- tests-as-checkbox (write tests AFTER the code, satisfying the gate
  without using tests as a design tool)

A process rule "thou shalt vertical slice" tells the agent to obey the
letter; the spirit (vertical-thinking design) does not transfer. The
fences must be **structural**: encoded in `specs/` (spec ↔ code
equivalence via graph-specs) and `.cfdb/queries/` (call-graph and
signature ban rules via cfdb). What graph-specs and cfdb cannot express
is prose at best — and prose does not block merges.

Therefore: every non-trivial feature passes through a council that
authors a spec **specifically designed to be expressible** as graph-specs
concepts and cfdb queries. If the council produces a spec invariant that
neither tool can encode, the council resolves it (rephrase, narrow,
mechanize) BEFORE the spec is committed.

## The loop

```
            ┌─────────────────────────────────────────────────────────┐
            │                                                         │
   /grill-me ──▶ TeamCreate council ──▶ specs/<context>/<concept>.md ─┤
                       │                          │                   │
                       ▼                          ▼                   │
                  council/                   cfdb queries  ◀──────────┘
                  <scar>/synthesis.md      .cfdb/queries/*.cypher
                                                    │
                                                    ▼
                                          graph-specs check + cfdb violations
                                                    │
                                                    ▼
                                                /to-issues
                                                    │
                                                    ▼
                                          EPIC + sub-issues + deps tree
                                                    │
                                                    ▼
                                          coder-claude per sub-issue
                                                    │
                                                    ▼
                                              CI gate (spec ↔ code)
```

### Step 1 — `/grill-me`

**Purpose.** Produce a shared understanding of the feature between human
and Claude. One question at a time, with Claude proposing the
recommended answer per question. Walks down the decision tree branch by
branch.

**Input.** A loose description: "we want to add X" or an existing issue
body that's still ambiguous on intent.

**Output.** A settled-design conversation in chat. NOT a committed
artifact yet — the artifact gets authored in the next step.

**Failure mode.** Skipping this step means Claude composes the brief
from its own model of what you want, which is the bias-loaded shortcut.

### Step 2 — TeamCreate council

**Purpose.** Convert the settled feature description into an
architectural deliberation across multiple lenses, producing a
`specs/<context>/<concept>.md` that captures the canonical concept,
its types, edges, invariants, and (if cross-context) Context Mapping
pattern.

**Mechanism.** Per global `~/.claude/CLAUDE.md` §2b, councils run as an
agent **team** via the `TeamCreate` tool — NOT as parallel `Agent`
sub-calls. The team boundary provides isolation; merge-rationale and
contested-resolution semantics are part of the team mechanism.

**Lenses (from `/pre-council` skill).** Default 4-7:

| Lens | Focus |
|---|---|
| **ddd-specialist** | Bounded context integrity, ubiquitous language, Context Mapping (ACL / shared-kernel / published-language / conformist), homonym discipline |
| **clean-arch** | Hexagonal layering, dependency direction, port purity, screaming architecture |
| **rust-systems** | Crate granularity, Cargo.toml dep graphs, feature flags, compile cost, trait object safety |
| **solid** | SRP / OCP / LSP / ISP / component cohesion, main-sequence distance |
| **business-domain** | Domain invariants the architecture lenses don't cover (for agentry: Commandant / officer / runner protocol) |
| **llm-behavior** | Prompt drift, citation integrity, calibration hazards (relevant for any role-runtime change) |
| **quant-trader** | Optional in agentry — only relevant if the feature touches money-path code (none today). |

**Lens isolation.** Each lens runs in parallel and MUST NOT read the
other lenses' verdict files during their run. Synthesis happens after all
lenses return.

**Input.** The settled feature description from /grill-me.

**Output.**

- `council/<feature>/target.md` — framing for all lenses (archaeology,
  references to current code at file:line, prior-art links, lens-neutral
  framing).
- `council/<feature>/<lens>.md` — one verdict per lens, three sections:
  archaeology (lens-specific findings), proposed spec contribution
  (markdown fragments), Non-negotiables (rules the lens insists on,
  flagged CONTESTED if disagreement is suspected).
- `council/<feature>/synthesis.md` — merge rationale + per-lens unique
  contributions + contested-resolution notes.
- `specs/<context>/<concept>.md` — the canonical spec the rest of the
  loop consumes.

**Escalation.** If contested Non-negotiables ≥ 3 across lenses, the
council auto-escalates to a second round with the full 7-lens set + a
re-framing of the contested items.

**The verifiability gate.** Before the spec is committed, ask:

- Every `##` heading in this spec — will it have a matching top-level
  `pub struct/enum/trait/type` in the code? If not, why is it in the
  spec? (Rephrase as prose, or remove.)
- Every invariant in the "Operational invariants" section — can cfdb
  encode it as a Cypher ban rule? If yes, file the rule alongside the
  spec. If no, document why the invariant is inherently prose-only.

A spec that names invariants neither tool can check is a documentation
artifact, not a fence. That's allowed but must be explicit.

**Failure mode.** Single-agent authoring. The lens parallelism IS the
mechanism — Claude alone defaults to one viewpoint and misses
cross-lens drift. Skipping council means you're trusting one model of
the feature.

### Step 3 — `/to-issues`

**Purpose.** Decompose the spec into vertical-slice issues
(tracer-bullets) on the forge with explicit dependency tree.

**Input.** The synthesized spec from Step 2.

**Output.** An EPIC issue + N sub-issues. Each sub-issue:

- **Title.** Short, names the slice.
- **Type.** HITL (needs human interaction) or AFK (autonomous).
- **Blocked by.** Other sub-issue numbers (or "None").
- **What to build.** End-to-end behavior, not layer-by-layer.
- **Acceptance criteria.** Bullets, derivable from the spec's
  Operational Invariants and Concept headings.

Each issue MUST be a vertical slice — a thin path through every layer
the feature touches (entry point → leaf, plus the test that proves it).
Horizontal slicing (one layer across multiple features) is rejected.

**Failure mode.** Coarse issues that bundle multiple slices.
Symptom: brief size exceeds coder's single-shot capacity, partial work,
self-review-unapplied verdict (cf. `feedback_brief_sizing.md`).

### Step 4 — Brief composition + dispatch

**Purpose.** For each AFK sub-issue, author a brief and dispatch into
`agentry-self-host-v3`. Each brief verb cites the spec section it
satisfies AND the cfdb rule (if any) that gates it.

**Input.** One sub-issue from Step 3.

**Output.** A coder PR.

**Brief template (additions on top of `docs/dogfood-protocol.md`):**

- Verbs cite `specs/<context>/<concept>.md#<heading>` for any pub
  type/trait the verb introduces.
- If the verb introduces a new pub fn parameter, the verb MUST require
  an end-to-end test that asserts two distinct values produce different
  outputs (param-effect canary, per qbot pattern).
- Acceptance command MUST include `cargo clippy --workspace
  --all-targets -- -D warnings` (the `--all-targets` is non-negotiable;
  see `feedback_brief_acceptance_all_targets.md`).
- Acceptance command MUST include `bash scripts/arch-check.sh`
  (graph-specs concept-level check + cfdb ban rules).

**Failure mode.** Brief that says "implement X" without verbs, without
spec references. Coder fills in interpretation; bias-driven shortcuts
land in the diff.

### Step 5 — CI fence

**Purpose.** Block merge of any PR whose code drifts from the spec OR
violates a cfdb ban rule.

**Mechanism.** `bash scripts/arch-check.sh` runs in `.gitea/workflows/arch.yml`:

1. `graph-specs check --specs specs/concepts/ --code crates/`
   — concept-level equivalence. Every `##` heading in `specs/concepts/*.md`
   must have a matching top-level `pub struct/enum/trait/type` in
   `crates/`. Vice versa: every pub type at file root must have a
   matching spec heading. Zero tolerance, no baseline.
2. `cfdb violations` against every rule in `.cfdb/queries/*.cypher`.
   Empty result = clean. Any row = violation = PR blocked.

**Spec status field.** A spec with `status: draft` (front-matter)
suppresses Concept-level failures for types declared but not yet in
code. Transition to `status: ratified` when the implementation PR opens
and `code_landing_pr:` is set. Lets a council pre-author a spec, then
land code that fills it in over multiple PRs.

**Failure mode.** Skipping `scripts/arch-check.sh` from brief acceptance,
or treating a violation as "false positive — disable the rule." Rules
are added one per PR with zero existing violations. Disabling a rule is
a major architectural decision and itself needs a council.

## What we have today (state of the loop)

| Step | Mechanism | State |
|---|---|---|
| /grill-me | `~/.claude/skills/grill-me/SKILL.md` | ✓ used in this session for runner pivot |
| Council | `TeamCreate` + `~/.claude/commands/pre-council.md` | ✗ NOT yet used in this session — gap |
| Spec format | `specs/dialect.md` (this repo) | ✓ defined; concept-level only today |
| Specs authored | `specs/concepts/*.md` (13 files) | ✓ several; `runner.md` from EPIC #182 |
| /to-issues | `~/.claude/skills/to-issues/SKILL.md` | △ used informally in this session; not strictly per the skill template |
| graph-specs check | `scripts/arch-check.sh` invokes `graph-specs check` | ✓ wired into CI via `.gitea/workflows/arch.yml` |
| cfdb ban rules | `.cfdb/queries/*.cypher` | △ only 2 rules today; room for more |
| Spec-status discipline | `status: draft / ratified / deprecated` per `/pre-council` template | ✗ none of agentry's current specs use the front-matter |

## Threshold — when does this loop fire

A brief MUST go through the full loop if ANY of:

- Adds, renames, or removes a top-level `pub struct/enum/trait/type` (the
  graph-specs gate triggers anyway; the council ensures the spec is
  authored upstream rather than retrofitted)
- Introduces a new bounded-context boundary (new `specs/concepts/*.md`
  file)
- Introduces a new public fn parameter on the money-path or a port trait
- Introduces a new cross-crate dependency
- Introduces a new CI gate or fence
- Touches more than 2 architectural concepts in the same diff

A brief MAY skip the loop if ALL of:

- Pure mechanical refactor (rename, file split, dedup with no new pub
  surface)
- Bug fix in a single fn with no signature change and no new pub item
- Doc-only change
- Test-only addition that doesn't change source under test

The skip path still respects every CI gate — it just doesn't pre-author
a spec via council, because there's no architectural decision to make.

## Anti-patterns (recorded so we don't drift back)

1. **Process-only fences.** "Brief author MUST scope vertical slices."
   Without code-level enforcement, the rule degrades over time.
   Solution: encode the structural check (graph-specs equivalence + cfdb
   horizontal-slice query).
2. **Prose-only specs.** Markdown that describes intent but uses no
   `##` headings or `- depends on:` bullets the parsers recognize. Looks
   like a spec, fences nothing.
3. **Council producing specs no tool can check.** Synthesis includes an
   invariant the lenses agreed on but graph-specs and cfdb cannot
   express. The invariant becomes documentation, not a fence —
   acceptable only when explicitly labelled "Operational invariants (not
   enforced by graph-specs)".
4. **Brief that omits `--all-targets`.** Default clippy doesn't lint
   test-target code; CI does. Briefs touching tests need the flag.
   See `feedback_brief_acceptance_all_targets.md`.
5. **Single-agent authoring of specs.** Skipping the council, having
   Claude write the spec alone, then committing. The lens parallelism
   IS the verification mechanism; one model = one viewpoint = drift.
6. **Reviewer-claude as architectural review.** The role catches some
   things and misses many. Don't trust it to catch architectural drift —
   that's what graph-specs + cfdb are for. Reviewer-claude is a
   readability + obvious-bugs check.
7. **Treating cfdb violations as false positives to disable.** Rules
   land one per PR with zero existing violations. Disabling a rule is
   itself a council-level decision.

## Cross-references

- `~/.claude/CLAUDE.md` §2b — councils use TeamCreate, not parallel
  Agent calls.
- `~/.claude/CLAUDE.md` §4 — task framing protocol; this document
  extends Step 0b (Prescription) by upstreaming it into the council.
- `~/.claude/skills/grill-me/SKILL.md` — Step 1 mechanism.
- `~/.claude/commands/pre-council.md` — Step 2 mechanism (full
  template, lens routing rules, escalation criteria).
- `~/.claude/skills/to-issues/SKILL.md` — Step 3 mechanism (vertical
  slices, HITL/AFK labels, dep tree publishing).
- `~/workspaces/cfdb/README.md` — what cfdb can express. Cypher subset,
  schema (Crate / Module / File / Item / Field / CallSite / EntryPoint /
  Concept / BoundedContext nodes; INVOKES_AT / CALLS / CANONICAL_FOR /
  RESOLVES_TO / IMPORTS / CONTAINS edges).
- `~/workspaces/graph-specs-rust/README.md` — what graph-specs checks.
  Four levels (Concept, Signature, Relationship, Bounded Context); only
  Concept is active in agentry today.
- `specs/dialect.md` (this repo) — agentry's current spec dialect
  subset; relationship-level edges parsed but not diffed yet.
- `~/.claude/projects/-var-mnt-workspaces-agentry/memory/feedback_no_tech_debt_for_speed.md`
  — every fix adds a fence.
- `~/.claude/projects/-var-mnt-workspaces-agentry/memory/feedback_claude_coder_biases.md`
  — the biases this loop catches structurally.

## Open questions parked

1. Where do `council/<feature>/` artifacts live in agentry's tree?
   The qbot/cfdb pattern uses `council/` at repo root. Agentry has no
   precedent. Decide before first use.
2. Bounded-context level of graph-specs is planned but not implemented.
   When does agentry adopt it?
3. The threshold table is a first cut. Refine after running the loop on
   2-3 features and seeing what bit us.
4. Lens selection for agentry features. The default 7 includes
   `quant-trader` which is qbot-specific. Agentry's first-pass lens set
   should drop that and possibly add a `runtime-substrate` lens (or
   reuse `clean-arch` + `rust-systems` to cover the role).
