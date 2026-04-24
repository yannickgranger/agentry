# .cfdb — architectural fact base

agentry's code-fact extraction and ban-rule surface. Paired with
`specs/` (consumed by graph-specs-rust) per the x-ray + vaccine model:

- **cfdb** — the x-ray. Extracts a typed fact graph from the workspace
  and runs Cypher ban rules against it.
- **graph-specs-rust** — the vaccine. Diffs `specs/` markdown vs the
  code's pub surface.

Both run in `.gitea/workflows/arch.yml` on every push and PR.

## Pinned revisions

- `cfdb.rev` — the commit of [yg/cfdb](https://agency.lab:3000/yg/cfdb)
  that CI installs.
- `graph-specs.rev` — the commit of
  [yg/graph-specs-rust](https://agency.lab:3000/yg/graph-specs-rust) that
  CI installs.

Bumping either is a reviewed PR. Bump the rev file, re-run
`scripts/arch-check.sh` locally, resolve any new violations in the same
PR.

## Ban rules

`queries/*.cypher` is the ban-rule directory. **Currently empty by
design.** Rules are added one at a time, each in its own PR with its own
justification and its own set of existing violations fixed first. No
baseline file, no ratchet, no allowlist — violation counts start at
zero for every declared rule.

Rule additions live on a separate track from the workflow wiring: CI
iterates `.cypher` files and treats any matching row as a failure.
Adding a new file is the gate that turns on its enforcement.

## Facts, not enforcement (today)

The workflow runs `cfdb extract` on every change and archives the
resulting fact graph as a CI artifact. This is the x-ray always running,
even before any ban rule is declared. It proves the extractor stays
healthy against agentry's growing code and gives future rules a warm
target.
