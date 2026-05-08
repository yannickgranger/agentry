# Brief kind

> Status: **draft**. Captain doctrine B2 lands the typed enum + predicate
> in `crates/orchestrator-types/src/kind.rs`. No Brief schema integration,
> no daemon validation, no captain CLI in this slice — those follow as
> B3-B6.

The brief kind is a typed classification of a brief by task shape. It
seeds two later behaviors: gating which briefs require a typed Contract
at intake (B3), and selecting which topology shape suits the brief
(further slices). The brief author declares the kind; the orchestrator
keys off it but does not infer it.

## BriefKind

Typed classification of a brief by task shape. Drives whether a contract
is required at intake (later slices) and which topology shape suits the
brief (further slices). Variants: TrivialDoc, TrivialMechanical,
Mechanical, BugFix, Feature, Migration, Portage, Sweep, Triage.

- **TrivialDoc** — README typo fixes, single-bullet additions,
  formatting. No contract required.
- **TrivialMechanical** — pure rename, file move, reformat with no
  logical change. No contract required.
- **Mechanical** — rename across many sites, type strengthening, dedup
  with semantic implications. Contract required (lean — 1-3 assertions
  cite cfdb qnames).
- **BugFix** — produce a regression test first, then fix. Contract
  required, must include a Behavior anchor.
- **Feature** — new behavior. Contract required, must include a
  SpecConcept anchor.
- **Migration** — swap doubles for real infra. Contract required, must
  reference a precursor council artifact.
- **Portage** — move existing code into a new shell. Contract required,
  must include a Cfdb anchor against a clones invariant.
- **Sweep** — fan-out epic. Contract required at parent; child briefs
  are TrivialMechanical.
- **Triage** — emits a routing decision artifact (no code). Contract
  required (the routing rules ARE the assertions).

#### Operational invariants (not enforced by graph-specs)

- The exhaustive match on `BriefKind` in `requires_contract` is
  intentional: adding a future variant must force a deliberate
  contract-requirement decision; a wildcard arm would let new variants
  silently default.
- Trivial variants exist so the dogfood loop is not bear-loaded by
  contract authoring on docs/typo briefs. Whether a brief is genuinely
  trivial is auditable — reviewer-claude will (in a later slice) flag
  misclassified trivial briefs whose diff touches non-trivial files.
- These nine variants are the seed vocabulary; the topology catalog
  (separate slice) will materialize one shape per non-trivial kind.
  The list is not closed — when an irreducibly-new task shape emerges,
  the enum extends.
