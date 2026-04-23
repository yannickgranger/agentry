# Next Concrete Action

**Status:** **M0 + M1 + M2 + M3 + M4 GREEN** as of 2026-04-23. Next: M5 — real LLM agent inside a container (first Claude/Grok/Gemini role).

## Done

| # | Scope | Commit | Proof |
|---|-------|--------|-------|
| M0 | runtime + podman spawner + echo | `24f7e4f` | verdict shipped for brf_verify_m0 |
| M1 | dashboard + SSE | `eb51c08` | /sse/verdicts emits live events |
| M2 | registry editor (forms) | `2753ecf` | POST /roles → role saved → brief → shipped |
| M3 | permit broker (signing + enforcement) | `95dc1cb` | naughty-agent blocked → permit_violation |
| M4 | inter-role message routing | see `git log` | speaker → listener; listener received message |

### Replay (order matters: M0 → M1 → M2 → M3 → M4)

```bash
export AGENTRY_REDIS_URL='redis://:RedisRationalized2026@192.168.1.152:6379'
export AGENTRY_DASHBOARD_PORT=7800
cd /var/mnt/workspaces/agentry
cargo build --release --workspace
./target/release/orchestrator key-gen --force    # M3
for img in echo naughty speaker listener; do
  podman image exists localhost/agentry/${img}-agent:v1 \
    || (cd containers/${img}-agent && podman build -t agentry/${img}-agent:v1 -f Containerfile .)
done
cd /var/mnt/workspaces/agentry
./target/release/orchestrator seed
ps -eo pid,comm | awk '$2 ~ /^orchestrator/ {print $1}' | xargs -r kill -9
nohup ./target/release/orchestratord         > /tmp/agentry-orchestratord.log 2>&1 &
nohup ./target/release/orchestrator-dashboard > /tmp/agentry-dashboard.log 2>&1 &
sleep 1
for m in 0 1 2 3 4; do just verify-M$m; done
```

## M5 — real LLM agent inside a container

Goal: a role whose container runs a real LLM (Claude API preferred, Grok/Gemini acceptable) against a prompt derived from the brief payload. Emits events with the model's output. Respects permit allowlist for any tool calls the model wants to make.

### Approach (simplest viable first — don't over-engineer)

1. **Container image:** `agentry/llm-agent:v1` — alpine + `python3` + `py3-pip` + `anthropic` (or `grok-python`/`google-genai`).
2. **Entrypoint:** `entrypoint.py` — reads stdin JSON bundle, pulls API key from env (passed by spawner via `--env ANTHROPIC_API_KEY=...`), calls the model with `brief.payload.prompt`, emits one `event` per streamed chunk (or one at the end if not streaming), then `done`.
3. **Spawner change:** pass any env vars from an orchestrator-side list-of-secrets (e.g. `AGENTRY_PASSTHRU_ENV=ANTHROPIC_API_KEY,OPENAI_API_KEY`) into the container with `--env KEY=value`.
4. **Key handling:** orchestratord reads keys from the environment at startup; the key NEVER lives in a file in the repo; `.gitignore` already covers `.env`.
5. **A role "llm-echo" seeded**: image `llm-agent:v1`, allowlist `[]` (no tools — pure completion), prompt from brief.
6. **verify-M5.json** submits `{payload:{prompt:"Say hello in 5 words"}}`; expect `verdict=shipped` + trace contains a model-output event.

### Budget

~150 LOC new Rust (env passthrough in spawner) + ~50 LOC Python (entrypoint) + container.

### Invariants to preserve

- Keys never committed. `entrypoint.py` never writes keys to stdout.
- Old verifies still pass.
- The model agent respects permit allowlist — if it tries a tool call, broker blocks (M3 mechanism).
- One single inference per brief for the first pass; streaming + long-context + tool-use are separate milestones.

### If Anthropic's API is down or the key is missing

- The brief should verdict=`failed` with reason. Don't hang.
- Timeout: container wall-time capped by podman `--timeout 60s`.

## If this session ends mid-task

- `git status` → commit as `wip(m5): ...`.
- Update this `TODO.md`.
- Update Redis key `project:agentry:resume` with current focus.
- Run replay block to validate nothing regressed.
