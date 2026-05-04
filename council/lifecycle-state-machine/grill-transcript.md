# Grill transcript — brief lifecycle state machine

**Scope:** issue #246 — substrate brief lifecycle state machine
**Date:** 2026-05-02
**Captain:** Claude Opus 4.7
**Subject:** yg

The current substrate emits verdicts opportunistically as the chain progresses. SETNX dedup (#178 / PR #225) catches duplicate XADDs but doesn't prevent premature-shipped verdicts that mask later failures. Today's drain (EPIC #255 + EPIC #256) hit the bug 5+ times and exposed several adjacent failure modes. The conceptual move per issue body: replace ad-hoc verdict emission + SETNX dedup with an **explicit brief lifecycle state machine** with terminal-only verdicts and explicit transitions.

The grill below resolved 10 architectural branches before opening the council. Decisions are load-bearing for the spec; the council can refine details but should not re-litigate these.

---

## Resolved branches

### B1 — State storage model

**Question:** Single Redis key per brief / append-only state stream / both?

**Resolution:** **Append-only state-transition stream in Redis.** User: *"you can make them append-only in redis."* Plus a constraint surfaced for the rest of the grill: *"we must be prepared for multi-agents, multi-teams, multi-tracks coding on multiple projects."* — the design must scale and support multiple concurrent state machines observable from the dashboard.

The trace stream already provides per-brief audit; the state stream is a separate, structured feed of just transitions. Append-only naturally provides ordering guarantees + replay-able audit.

---

### B2 — What "multi" means concretely

**Question:** Multi-agents at scale / project as first-class scope / cross-brief workflows / all three?

**Resolution:** **Project = runner pipeline; pipelines compose; compositions can mix projects.** User: *"The project is a runner pipeline they can be composed / Composition can mix projects."*

This aligns with #158 (DRAFT — composition of pipelines / sub-topologies as members) and #182 (runner pivot — workflows-as-data, Commandant-authored). The state machine must respect:
- Each project owns a runner pipeline definition (workflow + roles + payload schema)
- A parent brief may spawn child briefs, possibly in different projects
- The state machine layer must support cross-project lineage queries

---

### B3 — Composition shipping mechanics

**Question:** Flat with parent pointer + on-demand aggregation / reactive nested state machine / defer composition entirely?

**Resolution:** **Defer composition shipping, but the data model is composition-ready from day one.** User: *"defer, but it's loosy, because composition WILL be needed / and if you keep being lazy we write the briefes in postgres and you WILL have to make the composition naturally."*

Concrete implications:
- Every state record carries `parent_brief_id: Option<BriefId>` and `composition_role: Option<...>` from day one
- State queries support both single-brief lookup AND lineage rollup
- The reactive layer (parent state-changes-on-child-event) is a follow-up EPIC slice, but the data model accepts it without restructure
- Don't bake single-brief assumptions that composition would later need to undo

---

### B4 — Who writes the state?

**Question:** Daemon-only / distributed (each role writes) / hybrid intent-queue?

**Resolution:** **Daemon-only writer, event-driven LOGIC.** User picked (a) and confirmed *"i genuinely want event-driven logic for state, does a can prepare the events logic?"* Yes — daemon-only writer is fully compatible with event-driven; the daemon is the FSM consumer that subscribes to role-emitted events and projects state transitions.

Architecture:

```
Role emits event (e.g. coder_done, reviewer_blocker, ci_red)
      ↓
trace stream (Redis)
      ↓
Daemon's state-machine consumer subscribes via XREAD BLOCK
      ↓
Validates: is this event valid for the current state?
      ↓
If valid: write new state to state stream (XADD)
      ↓
Publishes state-change event (other consumers react: dashboard, future composition layer)
```

The intent-queue extension point (option c) is left explicit for the composition follow-up, but is NOT in the first slice's scope.

---

### B5 — Rust representation of the FSM

**Question:** Hand-rolled typed enum + transition function / FSM crate (statig, rust-fsm) / data-driven transition table?

**Resolution:** **(a) hand-rolled typed enum + transition function.**

```rust
enum BriefState {
    Submitted,
    Authoring { agent_id: String, started_at: Ts, attempt: u32 },
    Verifying { ... },
    Reviewing { ... },
    Shipping { ... },
    Watching { pr_number: u32, head_sha: String },
    Reworking { iteration: u32, max: u32, target: ReworkTarget },
    Shipped, Failed, ReworkedOut, Aborted,  // terminals
}

enum BriefEvent {
    CoderStarted { agent_id: String },
    CoderDone(EventVerdict),
    AcVerifierDone(...),
    ReviewerDone(EventVerdict, Vec<ReviewFinding>),
    ShipperDone { pr_number: u32, head_sha: String },
    CiResult { state: CiState, head_sha: String },
    RetryRequested { reason: String },
    AbortRequested { reason: String },
    BudgetExhausted,
}

fn handle(state: &BriefState, event: &BriefEvent) -> Result<BriefState, InvalidTransition>
```

Pure data + pure function. Daemon's event-consumer loop calls `handle()`. Zero deps. Compile-time exhaustive matching guarantees no missing transition. Same pattern as `compute_verdict` in spawner.rs.

---

### B6 — Universal lifecycle vs per-topology

**Question:** One universal state enum for ALL briefs / per-topology FSM / universal skeleton + extension points?

**Resolution:** **(a) one universal lifecycle now, designed to accept extension points later.** All 7 current topologies fit one shape (Submitted → Authoring → Verifying → Reviewing → Shipping → Watching → terminal). Topologies that skip phases (e.g. `agentry-verify-v0` has no coder) auto-transition through them. Future topology-specific phases would land as `Extension(name, ...)` variant + per-extension transition rules — same pattern as FENCE_MATRIX, extensible without restructure.

---

### B7 — Operator-facing contract: verdict stream

**Question:** Verdict stream stays as-is (derived from terminal states) / verdict stream deprecated / both verdict stream + state stream?

**Resolution:** **(a) verdict stream stays, derived from terminal-state transitions.** When the FSM transitions to a terminal state, the daemon writes ONE entry to `agentry:verdicts` (same shape as today). Old SETNX dedup goes away — terminal state is by definition a single transition. Dashboard, operator scripts, and `XREVRANGE agentry:verdicts` keep working. State stream is NEW and additive — granular consumers opt in.

---

### B8 — Retry budget: two-level model

**Question (revised after user observation):** "in dashboard i always see 2 entries or more per brief" — `brf_work_271_..._v1`, `_v2`, `_v3`, `_v4`, `_v5`. The pattern emerges from captain-driven re-dispatch (each `_v(N)` is a separate brief ID with its own verdict). Two completely different retry concepts emerged:
- **Inner loop (within one brief):** reviewer Blocker → coder rework
- **Outer loop (across briefs):** captain re-authors after terminal Failed

**Resolution:** **(b) one brief UUID per issue.** User: *"b. One uuid per issue."*

Concrete implications:
- Brief ID is `brf_work_271_cfdb_ban_inline_cfg_test` — no `_v(N)` suffix
- State machine tracks `attempt: u32` as a state field
- On terminal `Failed`, captain or operator emits `RetryRequested(brief_id)` event → FSM transitions back to `Authoring` with `attempt += 1`, within outer budget
- Dashboard shows ONE entry per issue with attempt count
- Verdict stream gets ONE terminal per logical work item — not one per attempt
- Inner + outer loops share ONE `attempt: u32` counter, ONE cap

The captain workflow changes: instead of re-authoring with a new ID, captain emits `RetryRequested` to the substrate.

**Cap:** captain proposed **3 with manual operator override flag**. Council can refine. (Today's session: X.0 needed 5 attempts — cap of 3 would have forced operator escalation after v3 instead of grinding to v5; the override flag preserves operator agency.)

---

### B9 — Backward-compat / cutover

**Question:** Hard cutover / dual-mode reader / lazy migration?

**Resolution:** **(a) hard cutover.** User: *"a."* Drain in-flight briefs → deploy FSM → restart. Accept the quiet window for clean data model. Single source of truth at all times; no dual-mode complexity.

---

### B10 — Forensic precursors

**Question:** Captain pre-work doc before council / let lenses do archaeology / separate brief ships first?

**Resolution:** **(a) captain pre-work captured in this transcript.** User: *"a."* Forensic data is fresh in the captain's context from today's drain; capturing it now while it's still hot serves the council better than asking lenses to re-derive it from `git log`.

---

## Captain pre-work — forensic snapshot

Today's drain across EPIC #255 + EPIC #256 produced concrete evidence of the bugs the FSM is meant to fix. The council should ground its design against these cases.

### Case 1 — Premature shipped + subsequent rework cycle (X.5 / `brf_work_268_coder_precommit_tests_migration_v1`)

- Verdict stream: `shipped` at 21:45:46Z (first reviewer-claude pass returned clean)
- Trace stream: continued for 30+ min after the "shipped" verdict — multiple reviewer rework cycles producing `done shipped` then `done rework_needed` then `done shipped` again
- Final outcome: actually shipped (PR #278 merged); but the verdict stream's "shipped" at 21:45 misled my own diagnostic queries multiple times in the session
- **Root cause:** team-aggregator emitted `Shipped` after first reviewer pass; SETNX locked the verdict; subsequent `done failed` / `done rework_needed` events from rework iterations were silently swallowed by the dedup gate

### Case 2 — Premature shipped + zombie branch (Y.6 v1 / `brf_work_262_reviewer_integration_v1`)

- Verdict stream: `shipped` at 00:11:25Z
- Develop tip: **commit was never pushed**; branch deleted from origin
- Trace tail showed clean `done shipped` at 00:14:36 (after the verdict-stream emission), suggesting the substrate emitted verdict early, then later cleaned up the branch as if the brief failed
- Outcome: had to dispatch `_v2` with fresh ID — Y.6 v2 shipped (`f647165`)
- **Root cause:** as Case 1 + plus an additional bug: branch lifecycle was tied to a later (post-verdict) decision point that disagreed with the verdict-stream's earlier decision

### Case 3 — Stalled chain after ac-verifiers (X.0 v5 / `brf_work_271_cfdb_ban_inline_cfg_test_v5`)

- Verdict stream: `shipped` at 09:29:32Z (coder phase)
- Trace stream: ac-verifiers (claude/gemini/grok) all completed `shipped` at 09:30:33-09:30:43Z
- Daemon log: NO further role spawns after 09:30:43Z. No reviewer-mechanical, no reviewer-claude, no shipper, no ci-watcher. Substrate stuck.
- Coder commit `d2fb577` sat in the worktree for ~10 min until I noticed and manually pushed + opened PR #295 + merged
- **Root cause:** premature-shipped verdict locked SETNX; team-aggregator's "is the brief done?" check returned true; subsequent role spawns never fired because the brief was already considered terminal

### Case 4 — Multi-merge-commits-per-brief (Y.4 v3, X.6b-α)

- Y.4 v3: commits `22b5806` + `23b22ac` on develop, both authored by the same brief
- X.6b-α: commits `299f6d6` + `76da8b2` + `759609d` on develop
- Pattern: rework cycle produced multiple commits, each one merged because the substrate auto-merged at each `done shipped` rather than waiting for the chain to terminally settle
- **Root cause:** verdict emission per role rather than per brief lifecycle

### Case 5 — Partial-PR shipping (X.6a-ii-α: shipped 3 files, brief asked for daemon+delivery+permit)

- Brief: 3 files (daemon, delivery, permit)
- PR diff: only 2 files (delivery, permit) — daemon.rs (heavy file, 1054 LoC) was skipped by the coder, presumably under rework context pressure
- Reviewer-claude let it ship (didn't flag the missing file)
- Discovered post-merge by the next captain wakeup: `grep -rln '#[cfg(test)]' crates/orchestrator-runtime/src/` showed daemon.rs still inline → had to author X.6γ to redo it
- **Root cause:** reviewer didn't compare diff against brief verbs structurally; brief contract enforcement is not part of the lifecycle today

### Case 6 — Stale-worktree GC blocking new dispatches (recurrent, ~6× today)

- Pattern: brief A fails or is manually merged; its worktree at `/var/mnt/workspaces/agentry-work/briefs/<brief_id>` and branch `auto/<brief_id>` are not cleaned up
- Brief B dispatches; daemon tries `git fetch` in the bare clone; fails with `fatal: refusing to fetch into branch 'refs/heads/auto/<brief_A_id>' checked out at <path>`
- Brief B fails immediately with 0 trace events
- I had to manually `git worktree remove --force` + `git branch -D` 6+ times today
- **Root cause:** workspace destruction logic only runs on shipped, not on failed; failed-brief cleanup is a manual operation. Adjacent issue, would benefit from FSM (terminal `Failed` could trigger cleanup just like terminal `Shipped` does today).

### Case 7 — Manual rebase needed (PR #291 / X.7c)

- X.7c (auto/brf_work_270_..._c_v1) and X.7b (auto/brf_work_270_..._b_v1) both touched `crates/agentry-role-runtime/src/lib.rs` + `specs/concepts/agent_contract.md`
- X.7b shipped first; X.7c then had merge conflicts on develop
- Substrate had no auto-rebase wired (137b shipped just hours ago); I had to manually rebase + push + merge
- **Root cause:** parallel-dispatch scenario; the just-shipped #137 (auto-rebase) is the structural fix. Mentioned here because the FSM should accept the auto-rebaser's output as a state transition (e.g. `RebaseStarted` → `Rebased` → back to `Watching`).

### Adjacent observation — substrate's existing implicit state inventory

State today is distributed across:
- `agentry:brief:{id}:trace` — append-only event stream (per brief)
- `agentry:active_briefs` — Redis SET of brief IDs currently being processed
- `agentry:verdicts` — Redis stream, terminal verdicts (1 per brief, dedup-locked by SETNX)
- `agentry:verdict:emitted:{brief_id}` — SETNX sentinel key per brief, set when first verdict lands
- `agentry:brief:{id}:verifier_verdict`, `agentry:brief:{id}:verifier_pending` — meta-brief verifier coordination
- `agentry:brief:{id}:children_pending`, `agentry:brief:{id}:children_verdicts` — meta-brief child aggregation
- Daemon process memory — role chain progress, message graph routing, retry counts (none persisted across restart)
- Forge state — open PR, branch, mergeable flag (single source of truth for shipping outcome, but not for in-flight state)
- Workspace directory existence — `/var/mnt/workspaces/agentry-work/briefs/<brief_id>` (presence ⇒ brief was started; absence ⇒ either never started or successfully cleaned)

The FSM should subsume `active_briefs`, the SETNX sentinel, and the per-brief verifier/children coordination keys. Forge state and workspace directory remain external (FSM observes them via events but doesn't own them).

---

## Final design summary (what the council inherits)

| Decision | Locked |
|---|---|
| State storage | append-only Redis stream per brief (`agentry:brief:{id}:state_log`) + projected current-state key (`agentry:brief:{id}:state`) |
| Multi-X composition | data-model-ready (parent_brief_id + composition_role on every state record); reactive layer = follow-up slice |
| FSM writer | daemon-only |
| FSM logic | event-driven (daemon subscribes to trace events, validates, writes state) |
| Rust representation | hand-rolled typed enum + pure transition function |
| Lifecycle scope | one universal lifecycle for all topologies; extension points designed for future topology-specific phases |
| Operator surface | `agentry:verdicts` stays as terminal projection; state stream is additive |
| Retry budget | one brief UUID per issue; `attempt: u32` on state; both inner-rework and outer-retry share the counter; cap=3 default + manual override flag (council to confirm) |
| Cutover | hard — drain → deploy → restart |
| Forensics | this document, captured by captain pre-work |

## Open items for the council to refine

1. **Concrete state name list.** Captain's starting proposal: `Submitted, Authoring, Verifying, Reviewing, Shipping, Watching, Reworking, Shipped, Failed, ReworkedOut, Aborted`. Council can split or merge.
2. **Concrete event name list.** Captain's starting proposal: `CoderStarted, CoderDone, AcVerifierDone, ReviewerDone, ShipperDone, CiResult, RebaseStarted, Rebased, RetryRequested, AbortRequested, BudgetExhausted`. Council can refine vocabulary.
3. **Transition table.** Council derives from B6's universal lifecycle.
4. **Failure recovery on daemon crash mid-transition.** Event-sourced replay from trace stream is the natural answer; council should validate.
5. **`RetryRequested` delivery mechanism.** Operator CLI? Issue comment? Captain dispatch tool? Operational detail.
6. **Whether to deprecate `agentry:active_briefs` set explicitly.** Subsumed by FSM (a brief is "active" iff its current state is non-terminal). Council call.
7. **Cap value for `attempt` budget.** Captain proposed 3; council can argue.
8. **Cross-reference with #182 (runner pivot) and #158 (composition).** Council should explicitly read those issues to ensure the FSM doesn't conflict with their planned shapes.

## Inputs to /pre-council

Three concepts likely emerge as canonical specs:

- **`brief_lifecycle.md`** — the FSM itself: states, events, transitions, terminal semantics, attempt budget, daemon-as-writer contract
- **`brief_state_stream.md`** — the Redis stream contract: format, ordering guarantees, replay semantics, projection key
- **`brief_retry_budget.md`** — the attempt counter, cap, RetryRequested mechanics, budget-exhaustion → terminal Failed transition

(Two of these may collapse into one if the council judges them as one concept.)

Lenses likely needed: **rust-systems** (FSM enum design, Redis client semantics, event-driven async patterns), **clean-arch** (daemon-as-writer port purity, state-vs-event separation, composition-readiness), **solid-architect** (SRP between FSM core / event consumer / state writer; OCP for extension points). Same calibration as EPICs #255/#256 council.

## Out of scope for this council

- Reactive parent-from-child layer (composition slice; follow-up EPIC)
- Dashboard rewrite (existing dashboard reads `agentry:verdicts` per Q7(a); FSM ships without dashboard work)
- Cross-project state queries (composition follow-up)
- `RetryRequested` operator-facing CLI surface (operational, post-FSM)
- #182 runner-pivot workflow definition format (orthogonal)
