# Next Concrete Action

**Status:** **M0 GREEN** as of 2026-04-23. Next: M1 — Dashboard (Axum + htmx + SSE) that shows briefs live.

## M0 — DONE (proof on real infra)

- Verdict: `agentry:verdicts` stream-id `1776957867711-0` → `{"brief":"brf_verify_m0","kind":"shipped", ...}`
- Trace: `agentry:brief:brf_verify_m0:trace` has 2 events (`hello` + `done`)
- Commit: see `git log` for the M0 commit

To replay M0 at any time:
```bash
export AGENTRY_REDIS_URL='redis://:RedisRationalized2026@192.168.1.152:6379'
cd /var/mnt/workspaces/agentry
cargo build --release --workspace
cd containers/echo-agent && podman build -t agentry/echo-agent:v1 -f Containerfile .
cd ../..
./target/release/orchestrator seed
./target/release/orchestratord > /tmp/agentry-orchestratord.log 2>&1 &
sleep 1
./target/release/orchestrator submit examples/verify-M0.json
sleep 5
redis-cli -h 192.168.1.152 -p 6379 -a RedisRationalized2026 --no-auth-warning XREVRANGE agentry:verdicts + - COUNT 1
kill $(pgrep -f orchestratord)
```

## M1 — Dashboard (next up)

Goal: Axum + htmx + SSE at http://localhost:7800 showing briefs live.

### Subtasks (in order)

1. `crates/orchestrator-dashboard/src/`:
   - `main.rs` — Axum server on 7800.
   - `state.rs` — shared Redis `ConnectionManager`.
   - `sse.rs` — SSE endpoint that tails `agentry:verdicts` (and per-brief trace streams).
   - `views.rs` — Askama templates: `layout.html`, `index.html`, `brief.html`.
   - `templates/layout.html` — base page, htmx CDN, Tailwind via CDN.
   - `templates/index.html` — "Briefs in flight" + "Recent verdicts" panels.
   - `templates/brief.html` — live trace view for one brief.
2. Routes:
   - `GET /` → index (list of briefs + verdicts).
   - `GET /brief/:id` → brief detail (live trace).
   - `GET /sse/verdicts` → SSE stream of new verdicts.
   - `GET /sse/brief/:id/trace` → SSE stream of brief's trace events.
3. `examples/verify-M1.json` — same as M0; the M1 verify is "replay M0, see the dashboard render it".
4. `justfile verify-M1` — kick off a brief and `curl -N localhost:7800/sse/verdicts`, expect to see the new verdict arrive.

### Budget

~250 LOC new. Ceiling per drift rules.

### Invariants to preserve

- All keys under `agentry:`. No `agency:`.
- No changes to `orchestrator-types` unless a new field is needed (M1 should not need any).
- No new primitives between M0–M9.
- Dashboard reads from Redis; never writes (until M2).

## If this session ends mid-task

- `git status`: commit uncommitted work as `wip(m1): checkpoint before session end`.
- Update this TODO.md with the exact next file/line.
- `mcp__memory__set key="project:agentry:resume" value=<state>` with current milestone.
- Run the "replay M0" block above to sanity-check nothing regressed.
