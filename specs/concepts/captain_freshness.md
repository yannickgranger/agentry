# Captain freshness

The `captain freshness` subcommand probes refs cited in a forge issue
body against `target_repo@base_branch` to catch drift between issue
filing and brief authoring. It runs two probe classes: the **file:line
probe** parses `crates?/<path>.<ext>[:N]` refs out of the issue body and
GETs the forge contents endpoint for each, catching file-moves and
line-shifts; the **pub-name probe** parses backtick-quoted CamelCase /
snake_case identifiers and looks each up in a pre-populated cfdb cache
(from `captain ground --target-repo`), catching type/function
relocations across crates that the file:line probe misses. The
subcommand prints a tab-separated classification table to stdout and a
summary line to stderr. Exit code is 0 when every ref classifies clean
and 1 when any ref is `MISSING`, `LINE_OUT_OF_RANGE`, or `RENAMED`.

The per-ref classification is factored out of the run loop so the
line-count comparison path is independently unit-testable without an
HTTP mock: the pure comparison lives in `classify_against_content`, and
the HTTP-fronted wrapper in `classify_ref` delegates to it.

## RefStatus

The classification verdict for a single ref. Four variants:
`Ok` (the path exists and any cited `:N` line is within range),
`Missing` (the forge returned HTTP 404 for the contents endpoint, or
the pub-name probe found no matching cfdb qname),
`OutOfRange { actual_lines }` (the path exists but the cited line
exceeds the file's line count), and
`Renamed { name, expected_file, actual_file }` (the pub-name probe
found a matching cfdb qname but its recorded file does not equal the
path cited near the backtick-quoted identifier in the issue body).
Variant names mirror the operator-facing stdout column values
(`OK` / `MISSING` / `LINE_OUT_OF_RANGE` / `RENAMED`) so the caller's
print loop stays readable. Public because integration tests under
`tests/` need to construct and match it directly — `pub(crate)` would
not be visible across the crate boundary, and inline `#[cfg(test)]`
modules in `src/` are banned by
`arch-ban-inline-cfg-test-in-src.cypher`.

The `Renamed` verdict catches type/function relocations across crates
that the file:line probe misses (the cited path still exists, the
file:line probe stays clean, but the actual definition has moved). The
pub-name probe is gated on a pre-populated cfdb cache from `captain
ground --target-repo` — without it, the probe is skipped with a stderr
note, not an error, so the file:line probe still runs. The probe
queries keyspace `"ground"` inside the cache directory derived from
`captain_ground_cache_dir`.

## CfdbRow

One row of the `cfdb query` projection the pub-name probe consumes: a
`qname` (the fully-qualified item name cfdb stores) and a `file` (the
path cfdb recorded the item at). Pure data carrier with no I/O —
`probe_pub_name` shells out to `cfdb query`, parses the JSON `rows`
array, lifts the first row into a `CfdbRow`, and hands it to the pure
classification helper. `pub` so unit tests can build fixtures without
spinning up a cfdb subprocess.
