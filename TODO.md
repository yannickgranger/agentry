# Next Concrete Action

**Status:** **M0 + M1 + M2 GREEN** as of 2026-04-23. Next: M3 — permit broker (AEGIS signing + tool-call enforcement).

## Done

### M0 — runtime + podman spawner + echo-agent
- Commit: `24f7e4f`
- Verdict: `agentry:verdicts` stream-id `1776957867711-0` for `brf_verify_m0`

### M1 — dashboard with SSE
- Commit: `eb51c08`
- Verified: `curl /` contains brf_verify_m1; `curl -N /sse/verdicts` captured live verdict event.

### M2 — typed registry editor (Role/Team/Project forms)
- Commit: see `git log` for the M2 commit
- Verified: `just verify-M2` creates `printer` role + `printer-team` via dashboard POST, submits brief, verdict=shipped.
- Also verified: POST /projects → saves project record with full standing_orders.

### Replay any milestone

```bash
export AGENTRY_REDIS_URL='redis://:RedisRationalized2026@192.168.1.152:6379'
export AGENTRY_DASHBOARD_PORT=7800
cd /var/mnt/workspaces/agentry
cargo build --release --workspace
podman image exists localhost/agentry/echo-agent:v1 || (cd containers/echo-agent && podman build -t agentry/echo-agent:v1 -f Containerfile .)
./target/release/orchestrator seed
ps -eo pid,comm | awk '$2 ~ /^orchestrator/ {print $1}' | xargs -r kill -9   # defensive: one daemon
nohup ./target/release/orchestratord         > /tmp/agentry-orchestratord.log 2>&1 &
nohup ./target/release/orchestrator-dashboard > /tmp/agentry-dashboard.log 2>&1 &
sleep 1
just verify-M0   # or verify-M1 / verify-M2
```

## M3 — Permit broker (next up)

Goal: AEGIS-style ed25519 signing + runtime tool-call enforcement. A role with `allowlist:[read]` that attempts `write` gets killed.

### Subtasks

1. **Salvage agency-aegis** from `~/workspaces/agency-orchestrator/crates/agency-aegis/`:
   - Identify the minimum set: signing key management, Permit sign/verify, audit trail append.
   - Copy into `orchestrator-runtime/src/permit/` module. Adjust to our `WorkPermit` shape (already structurally compatible per `orchestrator-types::permit`).
2. **Key management:**
   - `orchestrator key-gen` CLI subcommand → writes `~/.config/agentry/signing.key` (600).
   - Runtime reads key at startup; fails loudly if absent.
3. **Permit signing:**
   - `mint_permit()` in `daemon.rs` now signs the canonical JSON and sets `signature`.
   - Verify-on-use: the broker checks signature before permitting tool calls.
4. **Tool-call enforcement:**
   - In `spawner.rs`: when an `Event::ToolCall` arrives on stdout, the broker:
     - Checks tool ∈ allowlist → if not, kill container + append `permit_violation` verdict.
     - Appends the call to `agentry:brief:{id}:audit` stream.
   - `permit_violation` becomes a new VerdictKind path.
5. **Echo-agent upgrade (mis-behaving role):**
   - A second agent image `localhost/agentry/naughty-agent:v1` that emits an illegal tool_call event before emitting done → verify the broker kills it.
6. **examples/verify-M3.json** + `just verify-M3`:
   - Submit brief to team with a role whose allowlist is `[read]` and agent attempts `write`.
   - Expect verdict=`permit_violation` on Redis, dashboard shows red banner.

### Budget

~200 LOC new + agency-aegis salvage (copy ≤1,000 LOC depending on trim).

### Invariants to preserve

- Old M0/M1/M2 verifications must still pass.
- Runtime rejects unsigned permits (after M3 goes green, M0/M1/M2 verify recipes need to ensure `mint_permit` signs).
- Keys on disk: `~/.config/agentry/signing.key` — committed to `.gitignore`, never pushed.

## If this session ends mid-task

- `git status` → commit as `wip(m3): <file>:<line> <what>`.
- Update this `TODO.md`.
- `mcp__memory__set key="project:agentry:resume" value=<updated state>`.
- Replay recipe above.
