# Next Concrete Action

**Status:** **M0 + M1 + M2 + M3 + M4 GREEN** as of 2026-04-23. Next: M5 — real LLM agent inside a container.

## HARD RULE (re-read every time M5+ is touched)

> **Claude is Claude Max subscription only. NEVER the per-token Anthropic API.**
> Any Claude agent subprocesses the host's `claude` CLI. No `ANTHROPIC_API_KEY`, no `anthropic` Python SDK, no HTTP to `api.anthropic.com`, EVER.
> **Per-token APIs are OK for cheap/fast models only: Grok (xAI), Gemini (Google).**

If the next session violates this rule, stop and re-read `~/.claude/projects/-var-mnt-workspaces-agency-orchestrator/memory/feedback_claude_max_only.md`.

## Done

| # | Scope | Commit | Proof |
|---|-------|--------|-------|
| M0 | runtime + podman spawner + echo | `24f7e4f` | verdict shipped for brf_verify_m0 |
| M1 | dashboard + SSE | `eb51c08` | /sse/verdicts emits live events |
| M2 | registry editor (forms) | `2753ecf` | POST /roles → role saved → brief → shipped |
| M3 | permit broker (signing + enforcement) | `95dc1cb` | naughty-agent blocked → permit_violation |
| M4 | inter-role message routing | `366b1b9` | speaker → listener via team_context.messages |

## Replay (order matters: M0 → M1 → M2 → M3 → M4)

```bash
export AGENTRY_REDIS_URL='redis://:RedisRationalized2026@192.168.1.152:6379'
export AGENTRY_DASHBOARD_PORT=7800
cd /var/mnt/workspaces/agentry
cargo build --release --workspace
./target/release/orchestrator key-gen --force
for img in echo naughty speaker listener; do
  podman image exists localhost/agentry/${img}-agent:v1 \
    || (cd containers/${img}-agent && podman build -t agentry/${img}-agent:v1 -f Containerfile .)
done
./target/release/orchestrator seed
ps -eo pid,comm | awk '$2 ~ /^orchestrator/ {print $1}' | xargs -r kill -9
nohup ./target/release/orchestratord         > /tmp/agentry-orchestratord.log 2>&1 &
nohup ./target/release/orchestrator-dashboard > /tmp/agentry-dashboard.log 2>&1 &
sleep 1
for m in 0 1 2 3 4; do just verify-M$m; done
```

## M5 — real LLM agent inside a container

Two agent classes to validate in this milestone; pick whichever is easier to verify first and file the other as M5b.

### M5a (preferred first) — Grok or Gemini via API (cheap/fast, explicitly allowed)

- **Container image:** `agentry/grok-agent:v1` — alpine + `curl` + `jq`. No Python SDK necessary; xAI Grok API is a plain HTTPS JSON endpoint.
- **Entrypoint (bash):**
  1. `cat` the stdin bundle.
  2. Extract `brief.payload.prompt` with `jq`.
  3. `curl -X POST https://api.x.ai/v1/chat/completions` (or Gemini equivalent) with `$XAI_API_KEY` from env.
  4. Emit one `event` with the model response + `done shipped`.
- **Spawner change:** add `passthru_env: Vec<String>` support on the spawner so the brief can name which env vars to forward (e.g. `XAI_API_KEY`). Values come from orchestratord's env at startup; keys NEVER hit disk in the repo.
- **Role `grok-echo`** seeded: image `grok-agent:v1`, allowlist `[]`, `model: "grok-4-mini"` (or whatever cheap tier).
- **`examples/verify-M5a.json`**: `{payload:{prompt:"Reply with 'pong'"}}`. Expect verdict=shipped, trace contains the model's reply.

### M5b — Claude Max via the host `claude` CLI

**The Claude path uses the subscription, not the API.** Never `anthropic` SDK, never `ANTHROPIC_API_KEY`, never HTTP to api.anthropic.com.

- **Approach:** the agent container subprocesses `claude -p "<prompt>"` (headless mode). The binary + auth must be available inside the container.
- **Open questions to confirm with user before building:**
  - Where does Claude Max store auth on this machine? `~/.claude/`? Keychain?
  - Can the auth be bind-mounted into a rootless podman container?
  - Is `claude -p` rate-limited such that an agent-per-brief would trip it? If yes, needs throttling.
- **If those answers are green:** container is `alpine + jq + the claude binary` (bind-mount from host or install via npm/curl). Entrypoint mirrors M5a but shells out to `claude` instead of `curl`.
- **If those answers are red (e.g. auth can't leave the host):** run the `claude` CLI on the host, expose a tiny localhost-bound HTTP shim, and have the container call that shim.
- **Verify `examples/verify-M5b.json`**: same prompt, same expected shape, but model=claude-opus-4-7 and uses Claude Max.

### Budget for M5a

~100 LOC Rust (env-passthrough in spawner + role/seed) + ~30 LOC bash (entrypoint) + container image. Under the ~250 ceiling.

### Invariants

- Any `ANTHROPIC_API_KEY` anywhere = bug. Remove it.
- Old M0–M4 verifies still pass.
- Model agent respects permit allowlist — any tool-call it tries goes through the M3 broker.
- Container wall-time capped (`podman run --timeout 60s`) so a hung API doesn't freeze the brief forever.

## If this session ends mid-task

- `git status` → commit as `wip(m5): ...`.
- Update this `TODO.md` with exact next step.
- `mcp__memory__set key="project:agentry:resume" value=<updated>`.
- Replay block at top before new work.
