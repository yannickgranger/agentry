# Captain freshness

The `captain freshness` subcommand probes the file/line refs cited in a
forge issue body against `target_repo@base_branch` to catch drift between
issue filing and brief authoring (file split / renamed / moved, line
ranges shifted). It parses `crates?/<path>.<ext>[:N]` refs out of the
issue body, GETs the forge contents endpoint for each, prints a tab-
separated classification table to stdout, and a summary line to stderr.
Exit code is 0 when every ref classifies clean and 1 when any ref is
`MISSING` or `LINE_OUT_OF_RANGE`.

The per-ref classification is factored out of the run loop so the
line-count comparison path is independently unit-testable without an
HTTP mock: the pure comparison lives in `classify_against_content`, and
the HTTP-fronted wrapper in `classify_ref` delegates to it.

## RefStatus

The classification verdict for a single file ref. Three variants:
`Ok` (the path exists and any cited `:N` line is within range),
`Missing` (the forge returned HTTP 404 for the contents endpoint), and
`OutOfRange { actual_lines }` (the path exists but the cited line
exceeds the file's line count). Variant names mirror the operator-facing
stdout column values (`OK` / `MISSING` / `LINE_OUT_OF_RANGE`) so the
caller's print loop stays readable. Public because integration tests
under `tests/` need to construct and match it directly — `pub(crate)`
would not be visible across the crate boundary, and inline
`#[cfg(test)]` modules in `src/` are banned by
`arch-ban-inline-cfg-test-in-src.cypher`.
