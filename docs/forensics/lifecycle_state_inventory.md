# Substrate state inventory — where brief lifecycle state lives today

Captures the implicit state distribution across Redis keys, daemon in-memory state, and worktree filesystems as of develop tip 2618d822 (2026-05-04). Input for the lifecycle FSM EPIC (#246) council.

The lifecycle FSM design must absorb all of these into the single `agentry:brief:{id}:state` source of truth — or explicitly classify each as "outside FSM scope".

## Redis keys (per brief)

| Key | Type | Written by | Read by | Lifecycle role |
|---|---|---|---|---|
| `agentry:brief:{id}:body` | string (JSON) | submit | daemon, runners | Source brief payload |
| `agentry:brief:{id}:trace` | stream | every role | watchdog, dashboard, captain | Append-only event log |
| `agentry:active_briefs` | set | daemon (SADD on dispatch) | daemon, captain | Coarse "in-flight" indicator |
| `agentry:verdict:emitted:{id}` | string (SETNX 1) | daemon | daemon | Dedup gate for verdicts (the SETNX hack) |
| `agentry:verdicts` | stream | daemon | dashboard, captain | First-write-wins terminal verdict |
| `agentry:delivery:{id}` | hash | shipper, ci-watcher | captain, dashboard | pr_number, pr_url, ci_state, merged |
| `agentry:delivery:{id}:attempts` | counter | shipper | shipper | Retry budget counter |
| `agentry:briefs` | stream | submit, daemon | daemon | Inbound brief queue |

## Daemon in-memory state

Lost on restart — currently re-derived from trace + active_briefs.

| State | Where | When lost = correctness loss? |
|---|---|---|
| Role chain progress (which role next per brief) | tokio task per brief | YES — daemon restart mid-brief = orphan unless reaper |
| Outbox length per role | task-local | NO — re-readable from trace |
| Retry counters | active task | YES — restart = retry-budget reset |
| Wall-clock deadline timer | tokio sleep | YES — restart = brief never times out |

## Worktree filesystem state

| Location | Contents | Lifecycle role |
|---|---|---|
| `/var/home/yg/.local/share/agentry/work/briefs/<id>/` | git clone, coder edits, commits | Pre-PR work |
| `/var/home/yg/.local/share/agentry/work/.clones/yg/agentry/` | bare clone (cache) | Source for per-brief local clone |
| `agency.lab:3000/yg/agentry.git` | branches `auto/<brief_id>` | Pushed branches |

## Implicit state derivation rules (current)

The "state" of a brief is currently inferred by combining:

1. Is `id` in `agentry:active_briefs`?
   - **No** → brief is "done" (terminal or orphaned — can't tell)
   - **Yes** → in some non-terminal state
2. Latest trace event's `type` and `payload`:
   - `type:done` with `verdict:shipped` → terminal Shipped (if verdict was emitted)
   - `type:done` with `verdict:failed` → terminal Failed
   - `type:event` with `agent_event:terminated` → role done, daemon may chain next
   - other → role in progress
3. SETNX marker `agentry:verdict:emitted:{id}`:
   - Set → terminal verdict emitted (consume the FIRST one only)
   - Unset → no terminal verdict yet
4. Delivery hash `agentry:delivery:{id}`:
   - `merged:true` → strongest evidence of Shipped
   - missing → not yet shipped (or never will)

## State-derivation pathology cases (cross-ref forensics)

- Case 5/6 (Brief 232 v2 / 233 v1): verdict emitted Shipped via SETNX before chain finished; subsequent Failed events dropped silently.
- Case 2/3/4 (PRs #374/#381/#382): brief LEAVES active_briefs without any terminal event AND without setting verdict marker. The derivation rules say "done" (since not in active_briefs) but no verdict was emitted. Captain has to reconstruct by reading worktree contents.

## Gaps a lifecycle FSM must close

1. **Single source of truth** — replace the active_briefs SISMEMBER + trace-tail-pattern-match + SETNX-marker + delivery-hash composite with one atomic state field.
2. **Deadline-driven transitions** — every non-terminal state has a max wall-clock deadline; daemon's reaper transitions to Failed on expiry.
3. **Restart safety** — daemon recovers state from Redis on restart, not from in-memory tokio tasks.
4. **Retry budget as state field** — currently a counter on the brief; should be part of the state-machine's Reworking node.

## What stays outside lifecycle FSM scope

- `agentry:brief:{id}:trace` — append-only event log, useful for debugging, NOT a state source.
- `agentry:delivery:{id}` — derived from terminal Shipped state + ci-watcher's PR observations; FSM reads it but doesn't own it.
- Worktree filesystems — the lifecycle FSM transitions trigger workspace lifecycle (allocate / teardown / preserve) but doesn't enumerate worktrees in its state.

## Source data

- Daemon source: `crates/orchestrator-runtime/src/daemon.rs`
- Spawner: `crates/orchestrator-runtime/src/spawner.rs`
- Verdict / SETNX gate: `crates/orchestrator-runtime/src/redis_io.rs` (search for SETNX)
- Workspace lifecycle: `crates/orchestrator-runtime/src/workspace.rs` + `crates/orchestrator-runtime/src/lifecycle_driver.rs`
