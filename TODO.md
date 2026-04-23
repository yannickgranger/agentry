# Next Concrete Action

**Status:** **M0 + M1 + M2 + M3 GREEN** as of 2026-04-23. Next: M4 — inter-role message routing (two-role teams communicate via agency-bus-style inbox/outbox streams).

## Done

### M0 — runtime + podman spawner + echo-agent
Commit: `24f7e4f`. Verdict `shipped` for `brf_verify_m0` on `agentry:verdicts`.

### M1 — dashboard + SSE
Commit: `eb51c08`. `curl /` shows brf; `/sse/verdicts` emits live.

### M2 — typed registry editor
Commit: `2753ecf`. POST /roles, /teams, /projects all save typed records with auto version bump.

### M3 — permit broker (AEGIS signing + tool-call enforcement)
- ed25519 signing via `ed25519-dalek`. Key at `~/.config/agentry/signing.key` (0600). Generate: `orchestrator key-gen`.
- `sign(&mut permit, &signing_key)`, `verify(&permit, &verifying_key)`, `tool_allowed(&permit, tool)`.
- Daemon loads key at startup; `mint_permit` signs before handing off.
- Spawner verifies permit on entry; intercepts `Event::ToolCall` on stdout.
- Every tool call is appended to `agentry:brief:{id}:audit` (always, allowed or not).
- Unauthorized tool → container killed (`podman stop`), verdict `permit_violation`.
- Verified with a `naughty-agent` container whose allowlist is `[read]` but attempts `write`:
  - verdict on Redis: `{"brief":"brf_verify_m3","kind":"permit_violation","reason":"unauthorized tool call: write"}`
  - audit stream recorded the `write` attempt with `args={"path":"/etc/shadow"}`
- M0 regression: still green after signing pipeline.

### Replay any milestone

```bash
export AGENTRY_REDIS_URL='redis://:RedisRationalized2026@192.168.1.152:6379'
export AGENTRY_DASHBOARD_PORT=7800
cd /var/mnt/workspaces/agentry
cargo build --release --workspace

# One-time key gen (M3 onwards; skip if ~/.config/agentry/signing.key already exists)
./target/release/orchestrator key-gen --force

# Container images
podman image exists localhost/agentry/echo-agent:v1    || (cd containers/echo-agent    && podman build -t agentry/echo-agent:v1    -f Containerfile .)
podman image exists localhost/agentry/naughty-agent:v1 || (cd containers/naughty-agent && podman build -t agentry/naughty-agent:v1 -f Containerfile .)

./target/release/orchestrator seed

ps -eo pid,comm | awk '$2 ~ /^orchestrator/ {print $1}' | xargs -r kill -9   # defensive
nohup ./target/release/orchestratord         > /tmp/agentry-orchestratord.log 2>&1 &
nohup ./target/release/orchestrator-dashboard > /tmp/agentry-dashboard.log 2>&1 &
sleep 1
just verify-M0   # or verify-M1 / verify-M2 / verify-M3
```

## M4 — inter-role message routing (next up)

Goal: a `speaker → listener` team where the speaker's output lands in the listener's inbox. Trace shows ordered events from both roles.

### Subtasks

1. **Agent I/O contract v2** (orchestrator-types::event):
   - The existing `EventKind::Message { to, payload }` on stdout already marks an outbox message.
   - Container runner, on each `Message { to, payload }`, publishes to `agentry:agent:{<to-agent-id>}:inbox` (and mirrors to trace).
   - Need: a way to map `to: <role-name>` → the actual sibling's agent_id in the same brief. Introduce a per-brief role-agent-id table (in-memory in the daemon or Redis hash `agentry:brief:{id}:agents`).
2. **Spawner upgrade:** before spawning each role, resolve outgoing edges from `team.message_graph` and register the agent_id → outbox-stream mapping.
3. **Agent entrypoint:** role containers now receive `AGENTRY_INBOX_STREAM` env var; agents can tail the stream on their own schedule. For M4, keep agents simple: shell scripts that read `AGENTRY_STARTUP` (stdin) and optionally read one inbox line.
4. **Two-role example:**
   - `speaker` role: emits `Message { to: "listener", payload: {msg:"hi"} }` + `done shipped`.
   - `listener` role: reads inbox for one line, emits an `event` with the received payload, then `done shipped`.
5. **Team:** `speaker → listener` message graph, terminal `listener`.
6. **Daemon change:** sequential spawn, but when spawning role N, the daemon has the agent_id of role N-1 in hand. When role N-1's `Message { to: N }` arrives, route to role N's inbox.
7. **verify-M4.json** + `just verify-M4` — assert both trace events appear and verdict is `shipped`.

### Budget

~150 LOC new. Ceiling per drift rules.

### Invariants to preserve

- Sequential role execution (parallel = later milestone; adds complexity).
- Permit broker still enforces allowlists; `message` tool is implicit (not listed in allowlist as a distinct tool).
- Old M0–M3 verifications must still pass.

## If this session ends mid-task

- `git status` → commit as `wip(m4): <file>:<line> <what>`.
- Update this `TODO.md`.
- `mcp__memory__set key="project:agentry:resume" value=<updated state>`.
- Run replay recipe above.
