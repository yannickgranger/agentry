# Next Concrete Action

**Status:** **M0 + M1 GREEN** as of 2026-04-23. Next: M2 — typed registry editor (Role / Team / Project forms on the dashboard).

## M0 — DONE
- Verdict: `agentry:verdicts` stream-id `1776957867711-0` → `{"brief":"brf_verify_m0","kind":"shipped"}`
- Commit: `24f7e4f`

## M1 — DONE (proof on real infra)

- Dashboard: Axum on `0.0.0.0:7800`, `/`, `/brief/:id`, `/sse/verdicts`, `/sse/brief/:id/trace`, `/healthz`.
- Build: single `main.rs` (~400 LOC incl. inline HTML), hand-rolled HTML, Tailwind CDN, vanilla `EventSource`.
- Verify:
  - submitted `examples/verify-M1.json` → verdict `shipped` on `agentry:verdicts`
  - `curl /` → contains `brf_verify_m1` + `shipped` badge
  - `curl /brief/brf_verify_m1` → contains trace history
  - `curl -N /sse/verdicts` during a fresh submission → 1 `event: verdict` arrived live

### Replay M1

```bash
export AGENTRY_REDIS_URL='redis://:RedisRationalized2026@192.168.1.152:6379'
export AGENTRY_DASHBOARD_PORT=7800
cd /var/mnt/workspaces/agentry && cargo build --release --workspace
podman image exists localhost/agentry/echo-agent:v1 || (cd containers/echo-agent && podman build -t agentry/echo-agent:v1 -f Containerfile .)
./target/release/orchestrator seed
ps -eo pid,comm | awk '$2 ~ /^orchestrator/ {print $1}' | xargs -r kill -9   # defensive: ensure single daemon
nohup ./target/release/orchestratord         > /tmp/agentry-orchestratord.log 2>&1 &
nohup ./target/release/orchestrator-dashboard > /tmp/agentry-dashboard.log 2>&1 &
sleep 1
./target/release/orchestrator submit examples/verify-M1.json
sleep 5
curl -sS http://localhost:7800/ | grep -c brf_verify_m1     # expect >= 1
redis-cli -h 192.168.1.152 -p 6379 -a RedisRationalized2026 --no-auth-warning XREVRANGE agentry:verdicts + - COUNT 1
```

## M2 — Typed registry editor (next up)

Goal: dashboard forms that create/update `AgentRole`, `TeamTopology`, `Project` records in Redis. Serde-validated at save.

### Subtasks

1. `crates/orchestrator-dashboard/src/`:
   - Split `main.rs` into `main.rs`, `views.rs`, `forms.rs`, `sse.rs` (split becomes worth it at this LOC).
   - New routes:
     - `GET /roles`, `GET /roles/new`, `POST /roles`, `GET /roles/:name/:v`, `POST /roles/:name/:v`
     - `GET /teams`, `GET /teams/new`, `POST /teams`, `GET /teams/:name/:v`, `POST /teams/:name/:v`
     - `GET /projects`, `POST /projects`
   - Form rendering: `<form>` with typed inputs; server validates on POST.
   - Include basic styling via Tailwind CDN; no JS frameworks.
2. Persist: write the typed record to Redis (`agentry:role:{name}:v{version}` etc).
3. Version bump: every save increments `version`; old versions stay for diff.
4. `examples/verify-M2.json` — submit a brief referencing a *freshly-created* role (created via form in the verify recipe: `curl -X POST /roles -d ...`).
5. `justfile verify-M2`.

### Budget

~250 LOC new (incl. form templates). Ceiling per drift rules.

### Invariants to preserve

- Dashboard is still read-only for streams (no writes to `agentry:briefs`).
- Only role / team / project records are writable from the UI.
- Serde validation rejects malformed input.
- `orchestrator seed` becomes a fallback; the dashboard is the primary way to create records from M2 on.

## If this session ends mid-task

- `git status` → commit as `wip(m2): <file>:<line> <what>`.
- Update this `TODO.md`.
- `mcp__memory__set key="project:agentry:resume" value=<updated state>`.
- Run the "replay M1" block above to confirm no regression.
