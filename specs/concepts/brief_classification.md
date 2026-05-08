# BriefClassification

> Status: **draft**. Council-authored 2026-05-08 to resolve a name collision between two `BriefKind` enums introduced by B1+B2 of the captain-doctrine work. Implementation lands in B2.5.

A brief carries two distinct classifications at different layers of the runtime: an authorial **task shape** declared by the captain, and an executional **validator pipeline** consumed by the daemon. These are related but separate concepts. This concept doc records the canonical names, the mapping between them, and the migration that resolves the prior collision.

The captain authors a brief and declares its `TaskShape`. At validation time, the daemon translates the task shape into a `ValidatorPipeline` via a pure `From` impl and dispatches the corresponding validator chain. Captain authors in captain-language; the daemon executes in pipeline-language; the boundary between them is the `From` impl.

## TaskShape

Captain-facing classification of a brief by authoring intent. Variants describe the shape of work the captain has scoped, not the policy that runs to validate it. Lives in `crates/orchestrator-types/src/kind.rs` (file kept for git-history continuity from B2; the type was renamed from `BriefKind`). Re-exported at the crate root as `orchestrator_types::TaskShape`.

Variants (kebab-case on the wire):
- `TrivialDoc` — README typos, single-bullet additions, formatting. No contract required.
- `TrivialMechanical` — pure rename / move / reformat with no logical change. No contract required.
- `Mechanical` — rename across many sites, type strengthening, dedup with semantic implications. Contract required (lean).
- `BugFix` — produce a regression test first, then fix. Contract required, must include a Behavior anchor.
- `Feature` — new behavior. Contract required, must include a SpecConcept anchor.
- `Migration` — swap doubles for real infra. Contract required, must reference a precursor council artifact.
- `Portage` — move existing code into a new shell. Contract required, must include a Cfdb anchor against a clones invariant.
- `Sweep` — fan-out epic. Contract required at parent; child briefs are TrivialMechanical.
- `Triage` — emits a routing decision artifact (no code). Contract required.

Carries a pure predicate `requires_contract(self) -> bool` (the contract-correspondence rule) preserved verbatim from B2's existing predicate. The exhaustive match without wildcard arm is intentional — adding a future variant must force an explicit contract-requirement decision.

## ValidatorPipeline

Daemon-facing classification of a brief by execution policy. Variants name validator chains: which `&'static dyn Validator` instances run for this kind of brief. Lives in `crates/orchestrator-types/src/pipeline.rs` (new module; previously lived as `BriefKind` in `brief.rs`). Re-exported at the crate root as `orchestrator_types::ValidatorPipeline`.

Variants (snake_case on the wire — preserves backward compatibility for any pre-existing serialized briefs):
- `Refactor` — same wire form as before.
- `BugFix` — renamed from `Debug`; carries `#[serde(alias = "debug")]` for backward compat.
- `Mechanical` — same wire form as before. **Homonym with `TaskShape::Mechanical` — see Operational invariants below.**
- `Feature` — renamed from `NewFeature`; carries `#[serde(alias = "new_feature")]`.
- `Substrate` — same wire form as before.
- `Triage` — renamed from `Audit`; carries `#[serde(alias = "audit")]`.
- `TrivialDoc` — renamed from `Doc`; carries `#[serde(alias = "doc")]`.

`crates/validators::registry_for(pipeline: ValidatorPipeline) -> Vec<&'static dyn Validator>` is updated to dispatch on the renamed variants. Validator instances and their static lifetimes are unchanged.

#### TaskShape-to-pipeline mapping

The boundary translation. Implemented as `impl From<TaskShape> for ValidatorPipeline` in `crates/orchestrator-types/src/pipeline.rs`. Lives in `orchestrator-types` because the orphan rule requires at least one of the types to be local to the crate where the impl is defined; both enums are in `orchestrator-types`, satisfying the rule. The impl is a pure exhaustive match — the type system forces every new `TaskShape` variant to declare its pipeline target at compile time.

Conservative defaults are explicit and documented in code:
- `TaskShape::TrivialDoc` → `ValidatorPipeline::TrivialDoc` (1:1)
- `TaskShape::TrivialMechanical` → `ValidatorPipeline::Mechanical` (uses the lightweight pipeline)
- `TaskShape::Mechanical` → `ValidatorPipeline::Mechanical` (1:1, despite different intent — see invariants)
- `TaskShape::BugFix` → `ValidatorPipeline::BugFix` (1:1)
- `TaskShape::Feature` → `ValidatorPipeline::Feature` (1:1)
- `TaskShape::Migration` → `ValidatorPipeline::Feature` (no migration-specific pipeline yet; uses fullest available)
- `TaskShape::Portage` → `ValidatorPipeline::Refactor`
- `TaskShape::Sweep` → `ValidatorPipeline::Refactor`
- `TaskShape::Triage` → `ValidatorPipeline::Triage` (1:1)

Each conservative mapping carries a comment naming the eventual purpose-built pipeline. The mapping evolves as the topology catalog grows.

#### Brief.kind field

Field name preserved as `kind` for serde wire-format continuity. The field's *type* changes from `Option<BriefKind>` (legacy) to `Option<TaskShape>`. Any pre-existing brief JSON with an old `kind` value such as `refactor` no longer deserializes as `Brief.kind` (TaskShape has no Refactor variant); briefs with such legacy kinds were not used in production before this rename.

#### Operational invariants (not enforced by graph-specs)

- The captain authors `TaskShape`; the daemon consumes `ValidatorPipeline`. The boundary is `From<TaskShape>`. No code outside the daemon's validation-dispatch path should ever construct a `ValidatorPipeline` directly — it is derived, not authored.
- The Mechanical homonym is intentional and load-bearing. `TaskShape::Mechanical` describes captain authoring intent. `ValidatorPipeline::Mechanical` describes execution policy. Type context disambiguates at every call site. The DDD lens flagged the homonym; rust-systems noted wire-format constraints; synthesis preserved the wire form and accepts the homonym with this invariant as the explicit acknowledgment.
- The `#[serde(alias)]` attributes on `ValidatorPipeline` variants are non-negotiable. They ensure that briefs serialized under the old taxonomy with `kind: "debug"`, `kind: "doc"`, `kind: "new_feature"`, `kind: "audit"` deserialize cleanly as `ValidatorPipeline` even after the rename.
- The `From<TaskShape> for ValidatorPipeline` impl is exhaustive without wildcard. Adding a new `TaskShape` variant must force an explicit pipeline-target decision.
- The `Brief.kind` field name is preserved. Existing serialized briefs deserialize with no wire change; the type rename is an internal refactor.
- Naming: `TaskShape` was preferred over `BriefKind` by 3 of 4 lenses (Clean-Arch, SOLID, Rust-Systems). DDD argued for `BriefKind` based on the user's ubiquitous language. Synthesis kept the user's word as the field name (`Brief.kind`) and used `TaskShape` as the type name. This is the only lens disagreement that survived synthesis; recorded so future reviewers understand the trade-off.
- No new crate.
- The mapping has no I/O. `From<TaskShape> for ValidatorPipeline` is a pure exhaustive match.
