# Captain ground

The `captain ground` subcommand renders a markdown grounding sheet for a
captain who is about to author a brief. Inputs: a free-form directive, a
cfdb name pattern, and the workspace whose specs/concepts directory holds
the spec corpus. Outputs: a sheet listing candidate cfdb qnames,
candidate spec sections, and a seed `Vec<AssertionAnchor>` serialized as
JSON inside a fenced block.

The seeds section is the load-bearing surface: it is constructed by
building real `orchestrator_types::contract::AssertionAnchor` values and
serializing them, so a rename of the anchor enum's variants forces a
compile error in the renderer rather than producing a wire shape that
diverges from the type. The grounding sheet is the literal mechanism by
which captains will produce conforming `Brief.contract.assertions` arrays
going forward.

## CfdbItem

One row from `cfdb list-items-matching` — a candidate symbol the captain
may anchor an assertion to. Carries the qname, the cfdb-reported kind,
the source file, the source line, and the bounded context the symbol
belongs to (when the cfdb extractor knows it). Public so integration
tests for the renderer can construct fixture rows without shelling out
to a real cfdb.

## SpecMatch

One spec file that contained at least one heading whose lowercased text
included a directive token as a substring. Carries the file path
(workspace-relative when possible) and the matched heading texts in
their original case. Public for the same fixture-construction reason as
`CfdbItem`.
