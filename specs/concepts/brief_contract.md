# Brief contract

> Status: **draft**. Captain doctrine B1 lands the typed shape in
> `crates/orchestrator-types/src/contract.rs`. No daemon validation, no
> Brief schema integration, no captain CLI in this slice — those follow
> as B2-B6.

A validation contract is a typed artifact a captain authors per
non-trivial brief, declaring what must be true after the brief lands.
Each assertion is anchored either to a cfdb qname (verifiable
structurally), a graph-specs concept (verifiable as spec-conformance),
or a behavior target (verifiable against real infra). Assertions
without anchors are forbidden — that is the structural shift that
makes contracts more than prose.

This brief is purely the typed data model + tests + concept spec.
Subsequent slices: B2 introduces a `BriefKind` enum gating when
contracts are required; B3 plumbs `Option<Contract>` onto the Brief
payload and runs anchor validation at daemon intake (log-only
initially); B4 seeds the first topology shape; B5 adds a captain CLI;
B6 switches validation to reject-mode after evidence accrues.

## Contract

Top-level container. A signed brief carries an optional Contract
(introduced now as a type, made required for non-trivial brief kinds
in a later slice).

- depends on: BriefId
- depends on: Assertion

## Assertion

One claim about post-brief state. Carries an id, prose description,
and an anchor that grounds the claim in something verifiable.

- depends on: AssertionId
- depends on: AssertionAnchor

## AssertionAnchor

Discriminated union over three verifiable forms: Cfdb (a qname that
must resolve in the workspace's cfdb keyspace), SpecConcept (a section
in `specs/concepts/` that must resolve via graph-specs), Behavior (a
live-system target verifiable against real infra). An anchor is
mandatory; an Assertion without an anchor is rejected at parse time by
`deny_unknown_fields` plus the enum's structure.

## AssertionId

Newtype wrapper for assertion identifiers (e.g., `"A1"`, `"A2"`).
String inside, hashable, used as a key.

#### Brief.contract field

Optional field on Brief.Payload. When
`Brief.payload.kind.requires_contract()` is true and
`Brief.payload.contract.is_none()`, the daemon logs a WARN at intake
(B3 of the captain-doctrine slices). Existing briefs without the field
deserialize via `#[serde(default)]` with `contract: None`. A later
slice (B6) will switch the WARN to a REJECT, after evidence accrues
that captains author contracts when expected.

#### Operational invariants (not enforced by graph-specs)

- Every Assertion has exactly one anchor.
- `precursor_artifacts` in a Contract are file paths that must exist
  in target_repo at base_branch; subsequent slices will validate this
  at intake.
- Contracts are immutable once dispatched; the typed shape is what
  makes them auditable across captain sessions.
- Brief.contract is observed but not validated against ground truth in
  B3. Anchor existence (cfdb qname resolution, graph-specs concept
  resolution, file-system path resolution for precursor_artifacts) is
  checked at intake in B6.
