# Review

The bounded context that owns *structured findings from any quality gate*.
Produced by any role that acts as a gate — reviewer containers today,
coder exitpoints and ci-watcher tomorrow. Consumed by the daemon's rework
loop, which routes findings back to the upstream worker via the team's
`message_graph`.

Distinct from `outcome::Verdict` — a `Verdict` is the terminal record for
a role (one per role run); a `ReviewFinding` is a single actionable issue
within a run (zero or many per role run).

## Severity

The two-level consequence axis. `Blocker` triggers daemon rework.
`Warn` is informational; the daemon does not act on warnings.

Producers decide: a role that only has warnings to report emits
`VerdictKind::Shipped` with the warnings attached to its outbox, not
`VerdictKind::ReworkNeeded`. This keeps the daemon's rework decision
simple — if the verdict variant says rework, it means rework.

## FindingOrigin

Who produced the finding. Two variants:

- `Mechanical` — a deterministic tool (cargo fmt, cargo clippy, cargo
  test, scripts/arch-check.sh, quality-hygiene). Names the `tool` binary
  and optionally the specific `rule` (lint name, test name).
- `Model` — an LLM-driven reviewer. Names the `reviewer_agent_id` for
  traceability back to the agent's trace stream.

Dashboards and chain triggers branch on origin without parsing
`message`. A fleet-wide lint-rule frequency report reads
`Mechanical.rule`; a reviewer-bias audit reads `Model.reviewer_agent_id`.

## ReviewFinding

One actionable issue against a candidate change. Fields:

- `file`, `line` — optional source location
- `severity` — Blocker or Warn
- `origin` — Mechanical or Model
- `category` — free-form string (lint, test, fmt, arch, design)
- `message` — human-readable body
- `suggested_fix` — optional proposed remediation
- `prohibitions` — list of constraints the next coder iteration MUST NOT violate (populated by Blockers to anchor rework; empty for Warns)
- `requirements` — list of constraints the next coder iteration MUST satisfy (populated by Blockers to anchor rework; empty for Warns)

Round-trips through serde so the daemon can ship a finding inside a
`RoutedMessage.payload` to an upstream role on rework.
