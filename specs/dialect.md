# Spec dialect

agentry's spec directory is gated by
[graph-specs-rust](https://agency.lab:3000/yg/graph-specs-rust). This file
describes how the gate reads specs and code; moving a file between
`specs/` and `docs/` changes whether it is under the gate.

## Registry boundary

`specs/` is the spec registry. Only `specs/concepts/*.md` is walked by the
CI gate. Anything else under `specs/` (including this file) is meta and
not parsed.

## What the markdown reader parses (in `specs/concepts/`)

- `##` (h2) and `###` (h3) headings — each heading becomes a concept
  node. Heading text is normalised: inline backticks stripped, whitespace
  trimmed, generic parameters removed.
- Fenced ` ```rust ` blocks — reserved for signature-level equivalence
  (future). Parsed but not diffed today.
- Bullets prefixed with recognised relationship keywords
  (`- depends on: X`, `- implements: X`) — reserved for relationship-level
  equivalence (future). Parsed but not diffed today.

## What the markdown reader ignores

Prose, paragraphs, ordered lists, tables, bullets without a recognised
prefix, `#` and `####+` headings, links, images, HTML, files outside
`specs/concepts/`, any non-`.md` file.

## What the Rust reader parses (in `crates/`)

Top-level `pub struct`, `pub enum`, `pub trait`, `pub type` declarations
at the root of each `.rs` file. Identifier is the concept name, file path
and line number are the source location.

## What the Rust reader ignores

- Non-`pub` items
- Items gated by `#[cfg(test)]` or `#[cfg(feature = "…test…")]`
- Declarations nested inside `pub mod foo { … }`
- `impl` blocks, `fn`, `const`, `static`, `use`, `macro_rules!`, `mod`
- Tests, benches, examples directories; `target/`, `.git/`, `.claude/`

## The CI gate

One rule today: concept-level equivalence. Every pub type in the code
must have a heading in `specs/concepts/`; every heading in
`specs/concepts/` must have a matching pub type. Zero tolerance, no
baseline. Signature- and relationship-level gates are added later, rule
by rule — see `.cfdb/README.md`.
