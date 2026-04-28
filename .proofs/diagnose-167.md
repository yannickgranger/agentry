# /diagnose proof — issue #167 (dashboard pipe sharing)

## Phase 1 — Feedback loop

Built `crates/orchestrator-dashboard/tests/integration_pipe_sharing.rs` with two
deterministic regression tests:

- `fetch_blocked_behind_tail_loop_xread` — spawns DashboardStore, opens a
  verdicts subscription so a tail loop is parked on `XREAD BLOCK 800ms`,
  asserts a parallel `fetch_recent_verdicts(20)` returns in <200 ms.
- `list_blocked_behind_tail_loop_xread` — same setup, asserts
  `list("role")` (ZRANGE + MGET) returns in <200 ms.

Both tests gated on `AGENTRY_TEST_REDIS_URL` env var; CI without redis skips.

### Exit criteria

- ✅ Reproduces user's exact failure mode (page latency = command latency).
  Browser-observed `/teams` 5–10 s ↔ test-observed `fetch_recent_verdicts`
  ≈ block_ms; same mechanism, different scaling factor.
- ✅ Deterministic — failed on every run before the probe, passes on every run after.
- ✅ Asserts specific timing threshold (200 ms ceiling, 100 ms acceptance target).
- ✅ Runs in <2 s (well under /diagnose's 30 s ideal).

## Phase 2 — Hypothesis ranking

| # | Hypothesis | Prediction (falsifiable) | Status |
|---|---|---|---|
| **H1** ⭐ | `redis::aio::ConnectionManager` 0.27.6 `clone()` returns a handle to the **same multiplexed TCP pipe**; tail's `XREAD … BLOCK` parks the pipe's reader, queuing all other clones' commands. | Giving the tail loop its own connection (`redis_io::connect(&url)` for each tail spawn) → tests pass. Keeping the shared clone → tests fail. | **CONFIRMED** |
| H2 | `tokio::sync::broadcast` channel back-pressures command dispatch. | Drop the broadcast send from tail_stream → still fails. | Not tested (pre-refuted: broadcast has its own runtime, isolated from redis driver state). |
| H3 | `std::sync::Mutex` on the fanout HashMap held across an `.await`. | Drop the Mutex → still fails. | Pre-refuted by code read: lock is held only over HashMap insert at lines 250–260, no `.await` inside. |
| H4 | redis-rs 0.27 internal mpsc serialises dispatches even though "multiplexed". | Same fix as H1 → tests pass. | Subset of H1; H1 confirmation covers it. |
| H5 | tokio runtime starvation. | Increasing `worker_threads` reduces delay. | Pre-refuted: test uses `flavor="multi_thread", worker_threads=4` and still reproduces. |

## Phase 2 — Confirmation

### Probe applied

`crates/orchestrator-dashboard/src/store.rs::subscribe_stream` modified to:
1. Add `redis_url: String` to `Inner` (carried from `DashboardStore::new_with`).
2. Replace `let conn = self.inner.conn.clone()` with a fresh
   `redis_io::connect(&url)` per spawned tail loop.

### Before probe (.proofs/red-167.txt)

```
test fetch_blocked_behind_tail_loop_xread … FAILED  (848 ms vs 200 ms cap)
test list_blocked_behind_tail_loop_xread  … FAILED  (1.65 s vs 200 ms cap)
test result: FAILED. 0 passed; 2 failed
```

`list()`'s 2× block-window (≈ 1.65 s ≈ 2 × 800 ms) is itself diagnostic: BOTH
commands inside `list()` (`ZRANGE` + `MGET`) queued behind the tail's BLOCK,
each waiting its own 800 ms window. That's only possible if the pipe is
shared at the redis-rs driver level, not at any user-side mutex.

### After probe (.proofs/green-167.txt)

```
test fetch_blocked_behind_tail_loop_xread … ok
test list_blocked_behind_tail_loop_xread  … ok
test result: ok. 2 passed; 0 failed
```

Combined with rest-of-package: 23 tests passed, 0 failed, 4 ignored. No regressions.

## Cause site

`crates/orchestrator-dashboard/src/store.rs:261` (pre-fix):

```rust
let conn = self.inner.conn.clone();
let inner = self.inner.clone();
tokio::spawn(tail_stream(inner, conn, stream, field, tx));
```

Cloning `ConnectionManager` reuses the same multiplexed pipe. The redis-rs
driver dispatches commands through an internal mpsc to a single pipeline-writer
task; commands wait in order, and a parked `BLOCK` command stalls the queue.
The fix is structural: tails get their own TCP connection.

## /diagnose discipline

Phases 3–4 (fix + cleanup) deferred to /fix-issue per `/fix-issue::Step 0d`
hand-off note. The probe is left in place as the proposed fix. /fix-issue's
Phase 1 (Regression Gate) will re-validate RED→GREEN on this same test plus
gate-domain.
