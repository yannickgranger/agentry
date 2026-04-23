# v2 Anti-Patterns — MUST NOT DO

Derived from v1 post-mortem. Each is a terminal smell; each has a concrete substitute.

1. **Do NOT split a single concept into a `{concept}-domain / -contracts / -adapters` triad by default.** 28 of v1's 62 crates came from this reflex and the average triad is <1,200 LOC total. Instead, keep the concept in ONE crate with `mod domain; mod port; mod adapter;` and only extract when a second external consumer appears.

2. **Do NOT ship InMemory doubles in the same crate as the trait definition.** v1 put `src/doubles/` in 8 of 13 contracts crates; those doubles were then wired into production binaries (`coder-daemon.rs` has 4 InMemory refs, 1 redis ref). Instead, keep doubles in `#[cfg(test)]` or a `test-doubles` feature, and make production binaries fail to compile without a real adapter.

3. **Do NOT write an RFC catalogue that assigns every future crate an ID before the code ships.** v1 had 20 RFCs (3,909 lines) and introduced 14 more crates on paper that were never built. Instead, write RFCs when building the component, not the reverse; let the code be the RFC's proof.

4. **Do NOT encode the methodology/gates as runtime Rust code inside the orchestrator.** v1 put 30,556 LOC (24% of the codebase) into `devkit-gates` + `devkit-validators` + `agency-ci-generator` + `quality-contracts/judges/` — every methodology change required a compile cycle. Instead, keep the methodology as **team topology + configuration** (skills files, YAML recipes à la `quality-recipes`, prompts); the orchestrator executes a graph, it does not enforce a grammar.

5. **Do NOT introduce Anti-Corruption-Layer translators between bounded contexts.** v1 catalogues 8 ACLs (5 missing, 3 half-wired — `PhospheneAlertTranslator`, `AgentLifecycleTranslator`, etc.). Every boundary re-translates the same events. Instead, use ONE canonical event vocabulary for the pipeline (`agent-events`-style) and make every daemon speak it natively; a translator is the tax you pay for not agreeing on names.

6. **Do NOT label issues `wave-0`..`wave-5`.** v1 has 32 open wave-labelled portage issues; `wave-0` = "types", `wave-1` = "wiring", `wave-3` = "translators", `wave-5` = "consumers" — horizontal slicing by naming convention. Instead, require every issue to deliver an end-to-end observable behavior (entry → storage → observable output) in a single PR; the methodology rule "vertical slices only" belongs in the issue template, not in a wave label.

7. **Do NOT ship Redis streams whose consumer is "planned for a later wave."** v1 has 8 orphan streams; 3 depend on a consumer (agency-control, PHP) that was decommissioned and the decision (#391) has been open for 6 weeks. Instead, no stream gets a producer until the consumer exists in the same PR — a dashboard subscriber, a daemon, or a test harness that drains it.

8. **Do NOT allow a production binary to compile with InMemory wiring.** v1's `commands-daemon.rs` (5 InMemory, 0 Redis), `coder-mcp-server.rs` (2 InMemory), and `coder-daemon.rs` (4 InMemory) all run — bug #430 (P0 stream-name mismatch) survived because no live path exercised them. Instead, gate InMemory wiring behind `#[cfg(feature = "test-doubles")]` and build production binaries with `--no-default-features --features=real-infra`.

9. **Do NOT create "decision: delete or populate" issues for shim crates.** v1 has `#370` open since 2026-03-08 asking whether `agency-message-broker` (49 LOC, 0 tests, excluded from workspace) should live or die. A crate you cannot decide the fate of is already dead; delete it. Instead, require every crate to have a shipping consumer within its first PR or it never merges.

10. **Do NOT file "verify the wiring is correct" as a portage issue.** v1's wave-0/wave-1/wave-2/wave-3 portage issues (#371, #372, #373, #408, ...) ask humans to manually verify that code moved into the triad still works. Instead, the wiring is verified by running the E2E dashboard or a BDD scenario against real infrastructure — if the automation cannot prove it, the code is not done, and no verification ticket will change that.
