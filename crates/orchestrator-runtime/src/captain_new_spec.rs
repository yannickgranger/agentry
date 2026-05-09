//! captain new-spec — render a graph-specs-compliant skeleton for
//! `specs/concepts/<concept>.md`.
//!
//! The skeleton enforces the graph-specs equivalence rule: the H1 and the
//! sole level-2 heading must be the same CamelCase token, matching a Rust
//! type, function, or trait of that name in the target repo. Level-4
//! headings (`####`) are local subsections and are not concepts.

pub fn render_spec_skeleton(concept: &str, target_repo: &str) -> String {
    format!(
        "# {concept}\n\
         \n\
         > Status: **draft**.\n\
         \n\
         <one-paragraph definition of {concept} for {target_repo} \u{2014} replace this line>\n\
         \n\
         ## {concept}\n\
         \n\
         Top-level container. Replace this prose with the concept definition.\n\
         \n\
         - depends on: <Other>\n\
         - depends on: <Other>\n\
         \n\
         #### <Subsection 1>\n\
         \n\
         Replace with prose.\n\
         \n\
         #### <Subsection 2>\n\
         \n\
         Replace with prose.\n\
         \n"
    )
}
