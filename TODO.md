# Next Concrete Action

**Status:** **M0 + M1 + M2 + M3 + M4 + M5a + M5b + M6 GREEN**, and **prod-Redis squatting corrected**. Dev now uses a LOCAL podman Redis container (`agentry-dev-redis`, `127.0.0.1:6380`).

Next: introduce **figment** for typed configuration (user directive).

## HARD RULES

> **Redis: LOCAL podman container only** (`127.0.0.1:6380`). NEVER `192.168.1.152` (LXC 401 A0 memory) or `192.168.1.189` (LXC 522 PROD-AGENCY streams).
> **Claude: Claude Max subscription only** (host `claude` CLI + bind-mounted OAuth creds). NEVER the Anthropic API.
> **Cheap APIs OK for non-Claude:** Grok (xAI), Gemini (Google).

## Done

| # | Scope | Commit | Proof |
|---|-------|--------|-------|
| M0 | runtime + podman + echo | `24f7e4f` | verdict shipped |
| M1 | dashboard + SSE | `eb51c08` | /sse/verdicts live |
| M2 | registry editor (forms) | `2753ecf` | POST /roles /teams /projects |
| M3 | permit broker | `95dc1cb` | permit_violation on unauthorized tool |
| M4 | inter-role message routing | `366b1b9` | speaker → listener |
| M5a | cheap-API LLM (xAI Grok) | `38852b8` | grok-4-fast returned `pong` |
| M5b | Claude Max via host CLI | `06a922e` | claude -p returned `pong`, zero API spend |
| M6 | permit_overrides narrow downstream fs-scope | see `git log` | synthesizer narrowed coder; write to denied path blocked |
| Fix | migrate off prod Redis to local podman | see `git log` | `agentry:*` wiped from .152; dev Redis on 127.0.0.1:6380 |

## Replay (local Redis, in order)

```bash
cd /var/mnt/workspaces/agentry
export XAI_API_KEY=$(keepassxc-cli show -sa Password --no-password \
    -k ~/agent-zero/key/claude.key ~/agent-zero/claude.kdbx "services/xai-grok")

cargo build --release --workspace
./target/release/orchestrator key-gen --force || true

just dev-redis-up       # starts agentry-dev-redis on 127.0.0.1:6380
for img in echo naughty speaker listener grok claude synthesizer narrowed-coder; do
  podman image exists localhost/agentry/${img}-agent:v1 \
    || (cd containers/${img}-agent && podman build -t agentry/${img}-agent:v1 -f Containerfile .)
done
./target/release/orchestrator seed

ps -eo pid,comm | awk '$2 ~ /^orchestrator/ {print $1}' | xargs -r kill -9
nohup ./target/release/orchestratord         > /tmp/agentry-orchestratord.log 2>&1 &
nohup ./target/release/orchestrator-dashboard > /tmp/agentry-dashboard.log 2>&1 &
sleep 1

for m in 0 1 2 3 4 5a 5b 6; do just verify-M$m; done
```

## Next: figment for parameters

User directive: wire **`figment`** (https://docs.rs/figment) for typed config. Replace the scattered `std::env::var("AGENTRY_*")` calls with a single `Config` struct deserialized from:
- defaults in code,
- a TOML file (likely `~/.config/agentry/agentry.toml`),
- env overlay (existing env-var interface stays, just read via figment).

Variables to consolidate:
- `AGENTRY_REDIS_URL` / password file path
- `AGENTRY_DASHBOARD_PORT`
- `AGENTRY_SIGNING_KEY` path
- `RUST_LOG` (keep standard)
- `XAI_API_KEY` / `GEMINI_API_KEY` — stays per-role passthru, NOT in the central config

Shape (proposal — confirm before implementing):
```toml
# ~/.config/agentry/agentry.toml
[redis]
url = "redis://:PASSWORD@127.0.0.1:6380"
# Or: password_file = "~/.config/agentry/redis.password"

[dashboard]
port = 7800

[signing]
key_path = "~/.config/agentry/signing.key"
```

Plus env overlay: `AGENTRY_REDIS__URL`, `AGENTRY_DASHBOARD__PORT`, etc. (figment's `Env::prefixed("AGENTRY_").split("__")`).

## M7+ backlog

| # | Scope |
|---|-------|
| M7 | E2E on toy repo with shipper role (first PR opened by agentry) |
| M8 | Project + triggers (cron / webhook) |
| M9 | First real qbot-core issue closed by agentry |

## If this session ends mid-task

- `git status` → commit as `wip(figment): ...` or `wip(m7): ...`.
- Update this `TODO.md`.
- `mcp__memory__set key="project:agentry:resume"`.
- `just dev-redis-up` before any new work.
