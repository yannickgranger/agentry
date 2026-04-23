# Agency-Terminal — Streaming UX + State Model

**Source keys:**
- `agency-terminal:lean-architecture`
- `agency-terminal:architecture-decisions`
- `agency-terminal:bugs-consolidated`
- `agency-terminal:bugs-identified`
- `agency-terminal:gaps`
- `agency-terminal:issue-draft`
- `agency-terminal:issue-dump-complete`

## Why interesting for v2
The agency-terminal was user's UI for the agency (TUI client reading Redis streams). The lean-architecture decisions are a good distilled model for "how a human consumes multi-agent output in real time." v2 dashboard can steal the state model.

## Data lifecycle — the clean three-tier

From `agency-terminal:architecture-decisions`:
| Tier | What | Where |
|---|---|---|
| RAM | InFlightBuffer (partials + events + thread mapping) | in-process |
| Persistence | Thread metadata + cursor + collapsed summaries | JSON (was SQLite, simplified) |
| Transport | Live messages | Redis streams |

## The streaming gate (is_final invariant)

From `architecture:streaming-gate` (verbatim):
> is_final is a gate that prevents premature canonicalization
>
> is_final=false:
>   InFlightBuffer.append(thread_id, correlation_id, chunk)
>   ThreadManager NO CHANGE
>   unread NO CHANGE
>   ghost SHOWN
>
> is_final=true:
>   InFlightBuffer.finalize(thread_id) → accumulated content
>   ThreadManager.add_response() with final content
>   unread +1
>   ghost CLEARED
>
> Critical invariant: add_response() called EXACTLY ONCE per logical response, on is_final=true only

v2 dashboard: build the same invariant. "Partial chunks render, only final commits to thread history."

## UX patterns worth keeping

From `agency-terminal:lean-architecture`:
1. **Ghost indicator** — minimal status line ("⋯ Claude is responding... (Esc to cancel)"), not full fake message bubble.
2. **Soft lock** — buffer input during streaming, auto-send on `is_final`, no hard disable. "No grayed-out frustration, queued feel."
3. **JSON state file (atomic tempfile+rename)** — replaces SQLite. "Survives Ctrl+C, disk-full, crash mid-write."
4. **ReplayCursor trait** — single `last_cursor` in state, XREAD from there on cold start. Responses replayed, events ignored during catch-up.
5. **Events ephemeral** — discard on finalize, no persistence. "Replay paradox fix: replay only res: stream, ignore evt: during catch-up."
6. **One in-flight per thread** — new message auto-cancels old. No TTL needed, restart clears RAM.

## Contract with stdin-daemon (from contract:*)

Three streams, correlation-id based routing:
- `cmd:a0-agency` (terminal → daemon)
- `res:a0-agency` (daemon → terminal; `is_final` bool drives UI)
- `evt:a0-agency` (broadcast: tool_call, thinking, context_high)

Routing rules (verbatim):
> 1. Match correlation_id → route to originating thread
> 2. Match metadata.source=LeadDev:{project} → create/find {project} lead-dev thread
> 3. Default → human thread

`ResPayload::Partial` + `EvtPayload::ToolCall` + `EvtPayload::Thinking` added as variants for streaming support. New fields all `Option<T>` with skip_serializing_if — backward-compatible evolution.

## Bugs to learn from (actual failures from 2026-01-22)

From `agency-terminal:bugs-consolidated`:
| Severity | Bug | Fix |
|---|---|---|
| Critical | Panic → terminal corruption | Panic hook restores terminal |
| Critical | SIGINT/SIGTERM not handled | ctrlc / tokio::signal + cleanup |
| High | UI freeze on Redis hang (sender.lock().await blocks TUI) | try_lock / fire-and-forget |
| High | Router fails on malformed metadata | Dead Letter thread + robust parsing |
| High | Project name used as thread ID (collision) | Prioritize correlation_id over project name |
| Medium | Stale partials (no timeout) | Heartbeat check — 30s timeout with warning |
| Medium | Clock skew — replay Utc::now() overwrites original timestamps | Accept timestamp from message |

v2 dashboard should inherit these fixes as starting assumptions.

## Dead-end: SQLite persistence
> Removed complexity: SQLite → JSON file
> 60 FPS debounce → use existing 100ms poll
> Event collapse to metadata → discard entirely
> TTL cleanup → one-per-thread only
> Hard input lock → soft lock with buffer

The simpler model won. v2 should not default to SQL for UI state.
