# Brief state stream

> Status: **ratified**. Code landing PR: L.2 (EPIC #246). The port
> traits and Redis adapters live in
> `crates/orchestrator-runtime/src/lifecycle.rs`. Daemon wiring
> lands in L.3a (#300).

The bounded context that owns *the durable storage substrate behind the
brief lifecycle FSM*. The lifecycle concept (`brief_lifecycle.md`)
specifies the pure transition table; this concept specifies the two
ports the daemon calls between events (an `EventSource` for inputs, a
`StateProjector` for outputs) and the Redis layout the production
adapters write to.

Three Redis-side surfaces back one brief:

* `agentry:brief:{id}:state_log` — append-only stream of every FSM
  transition, one entry per `handle` call. Granular history.
* `agentry:brief:{id}:state` — single key holding the latest
  `BriefStateRecord`. Fast current-state read for dashboards.
* `agentry:brief:{id}:state_projector_cursor` — last trace-stream
  entry ID consumed. Used by crash-recovery to resume the projector
  loop from the precise event the daemon was processing when it
  crashed.

All three are written atomically via a single Lua script (the
`LUA_PROJECTOR_WRITE` constant). Redis Lua aborts cleanly on OOM, so
the only failure mode is "this transition is not committed and will be
re-attempted on the next event" — never a partial write that leaves
the three keys inconsistent.

The trace stream (`agentry:brief:{id}:trace`) is the **authoritative**
event log; the state stream is a derived projection that can be
rebuilt from `(trace + cursor)` on projector restart. The trace
stream's authority is what makes the state stream cheap to evict on
brief retention boundaries: re-running the FSM from `0-0` produces the
same record sequence.

## EventSource

Port that yields `BriefEvent`s for a single brief. The daemon's
per-brief lifecycle loop pulls one event at a time and feeds it to
`handle()`. Implementations are responsible for blocking the caller
until an event arrives — production blocks on `XREAD`, tests pop a
fixture queue. Returning `None` signals "no further events will
arrive" (the test adapter's empty queue, or a production adapter
hitting a non-blocking timeout when the brief is terminal).

## EventSourceError

Error surface returned by `EventSource::next`. Two variants today:
`Redis` for connection or read failures bubbled up from the
underlying `redis` client, and `Parse` for trace-stream entries
whose shape does not deserialise into the canonical `Event` form
(missing `agent` or `event` field, malformed JSON). The daemon caller
treats `Parse` errors as poison-pill events and is expected to log +
advance past them rather than abort the brief.

## StateProjector

Port that writes one `BriefStateRecord` plus the cursor that produced
it, in a single atomic step. Production runs the embedded Lua script
via `EVALSHA` and gets all-three-keys-or-none atomicity from Redis.
The trait does not describe error-recovery policy: callers re-attempt
the same write on the next event when the previous one failed, since
the trace stream is authoritative and the FSM is deterministic.

## StateProjectorError

Error surface returned by `StateProjector::write`. `Redis` covers
transport-level failures (connection drop, server unreachable);
`LuaFailed` covers the Lua engine's own error replies — most commonly
an OOM abort, where Redis refuses the script outright and the three
keys are guaranteed unchanged. The discriminator is structural so the
daemon can treat OOM-aborts as transient ("retry on next event") and
transport-level failures as session-fatal ("reconnect, re-load script,
re-attempt") without parsing the embedded message.

## RedisEventSource

Production `EventSource` adapter. Subscribes to
`agentry:brief:{id}:trace` via blocking `XREAD` and translates each
`EventKind`+role-name pair into the matching `BriefEvent`. Holds an
internal `agent_id → role_name` map populated from `spawned`
agent-events so that subsequent `Done` events can route to the
correct `BriefEvent` variant (`CoderDone`, `AcVerifierDone`,
`ReviewerDone`) without a second Redis lookup.

The cursor begins at `"0-0"` rather than the customary `"$"` — at
brief dispatch the trace stream is empty and every event arrives
strictly after the `XREAD` is issued, so starting from the beginning
of the stream is race-free. Crash recovery is supported via the
`resume_from` constructor, which seeds the cursor from the persisted
projector cursor key.

## RedisStateProjector

Production `StateProjector` adapter. Lazy-loads `LUA_PROJECTOR_WRITE`
into Redis on the first `write` call (one `SCRIPT LOAD` round-trip
per projector instance), caches the returned SHA, and dispatches every
subsequent write via `EVALSHA`. The atomic three-key write order is:
state-log XADD, state-key SET, cursor-key SET — invariant order so
operators reading the script body can reason about partial-execution
states even though Redis Lua never produces them.

#### Operational invariants (not enforced by graph-specs)

These are prose-only contracts the production adapters and their
daemon callers must uphold. They are not validated by graph-specs
(which only checks heading-to-pub-type equivalence) and so live under
a non-concept heading.

* `RedisEventSource` MUST start `XREAD` from `"0-0"`, never from
  `"$"`. Starting at `"$"` would miss events emitted between the
  brief's dispatch and the daemon's first `XREAD` call — the trace
  stream is empty at dispatch and every entry arrives after, so the
  `"0-0"` start is race-free by construction.
* The cursor MUST be persisted by the projector on every successful
  write, in the same Lua script as the state-log XADD and state SET.
  Persisting the cursor in a separate round-trip would re-introduce
  the partial-write hole that the Lua atomic guard exists to close.
* The state-stream is a **derived** projection — `(trace stream +
  cursor)` is sufficient to rebuild it. The trace stream is the
  authoritative event log; the state stream may be evicted at
  retention boundaries without losing brief history.
* Crash recovery rebuilds adapter state from the cursor key:
  `RedisEventSource::resume_from` seeds the `XREAD` cursor; the
  projector replays from there to overwrite the state key with the
  current record before yielding control to the next event.
* `EventSource::next` is a stream, not a callback — backpressure flows
  naturally from the daemon's loop pacing. No batching, no internal
  queue, no fan-out: one call returns one event (or `None` when
  terminal).
* The legacy `agentry:active_briefs` set is **deprecated** by the
  FSM: consumers that previously polled it for "is this brief still
  running?" must instead read the state key and inspect the
  `BriefState` discriminator (`Shipped` and `Failed` are the only
  terminals).
* Verdict-stream emission lands ONLY on a transition into a terminal
  state (`Shipped` or `Failed`), gated on the FSM result rather than
  on the agent's `Done` verdict alone. This subsumes the
  premature-shipped bug class where a coder's `Done(Shipped)` raced
  the reviewer's verdict.
* Hard cutover at deploy time: drain the previous daemon, deploy the
  L.3a-wired daemon, restart cold. There is no rolling upgrade —
  in-flight briefs are re-projected from their trace streams on the
  fresh daemon's first read.

#### Retention bounds (not enforced by graph-specs)

Submit-time MAXLEN: `agentry:briefs` is trimmed to ~10000 entries via
`XADD MAXLEN ~ N` at submit time. Approximate trim is ~5x cheaper than
exact, and the consumer-side projector doesn't need historical entries
— the trace stream and the per-brief state key are the authoritative
substrate for any post-hoc reconstruction.

Terminal TTL: when `cleanup_failed_brief` or
`cleanup_shipped_no_op_brief` runs, every `agentry:brief:{id}:*`
sibling key gets a 30-day TTL. Operator forensics window preserved;
long-tail leak prevented. Tunable via constant in
`lifecycle_driver.rs` (no env var, no allowlist — single source of
truth per `CLAUDE.md`'s "no metric ratchets" rule generalised to
retention).
