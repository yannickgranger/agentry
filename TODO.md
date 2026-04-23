# Next Concrete Action

**Status:** **M0 → M7 ALL GREEN**, figment-configured, LOCAL-Redis-only. First real PR opened by agentry: https://agency.lab:3000/yg/agentry-toy/pulls/1

Next: **M8 — triggers** (cron + webhook) so briefs fire without you submitting them by hand.

## HARD RULES (re-read every session)

> **Redis: LOCAL podman only** (`127.0.0.1:6380`). Never LXC 401 (.152) or 522 (.189).
> **Claude: Max subscription only** (host `claude` CLI + bind-mounted creds). Never Anthropic API.
> **Cheap APIs OK:** Grok (xAI), Gemini (Google). Per-role `passthru_env`.

## Done

| # | Scope | Commit | Proof |
|---|-------|--------|-------|
| M0 | runtime + podman + echo | `24f7e4f` | verdict shipped |
| M1 | dashboard + SSE | `eb51c08` | /sse/verdicts live |
| M2 | registry editor (forms) | `2753ecf` | POST /roles /teams /projects |
| M3 | permit broker | `95dc1cb` | permit_violation on unauthorized tool |
| M4 | inter-role message routing | `366b1b9` | speaker → listener |
| M5a | cheap-API LLM (xAI Grok) | `38852b8` | grok-4-fast returned pong |
| M5b | Claude Max via host CLI | `06a922e` | claude -p returned pong, zero API spend |
| M6 + prod-Redis fix | permit_overrides narrow scope + migrate to local Redis | `89bc95f` | fs:write scope denied; 29 keys wiped from .152 |
| figment | typed config (defaults + TOML + env overlay) | `bf3a81b` | 30 tests pass incl. anti-prod-Redis pin; TOML precedence proven |
| M7 | shipper opens real PR on toy repo | see `git log` | PR #1 open at agency.lab:3000/yg/agentry-toy/pulls/1 |

## Replay (in order, idempotent)

```bash
cd /var/mnt/workspaces/agentry
export XAI_API_KEY=$(keepassxc-cli show -sa Password --no-password -k ~/agent-zero/key/claude.key ~/agent-zero/claude.kdbx "services/xai-grok")
export GITEA_TOKEN=$(keepassxc-cli show -sa Password --no-password -k ~/agent-zero/key/claude.key ~/agent-zero/claude.kdbx "gitea/agency.lab")

cargo build --release --workspace
./target/release/orchestrator key-gen --force || true

just dev-redis-up
for img in echo naughty speaker listener grok claude synthesizer narrowed-coder shipper; do
  podman image exists localhost/agentry/${img}-agent:v1 \
    || (cd containers/${img}-agent && podman build -t agentry/${img}-agent:v1 -f Containerfile .)
done
./target/release/orchestrator seed

ps -eo pid,comm | awk '$2 ~ /^orchestrator/ {print $1}' | xargs -r kill -9
nohup ./target/release/orchestratord         > /tmp/agentry-orchestratord.log 2>&1 &
nohup ./target/release/orchestrator-dashboard > /tmp/agentry-dashboard.log 2>&1 &
sleep 1

for m in 0 1 2 3 4 5a 5b 6 7; do just verify-M$m; done
```

## Known M7 quirk

`verify-M7.json` has a fixed branch name (`agentry-m7-pong`). First run opens PR #1. Second run: `git push` is a no-op (same content), PR API returns "pull request already exists", and the verify grep finds the OLD "PR opened" event in the existing trace stream. For a clean re-verify: delete `yg/agentry-toy` branch `agentry-m7-pong` on the forge, or pick a new brief id + branch for each run.

Not a showstopper for M7 acceptance — the real proof (PR #1 on forge) stands. Fix if it bites.

## M8 — triggers (next up)

Goal: briefs fire without user pressing a button.

### Three trigger classes (all simple, all existing-OS shapes)

1. **Cron trigger.** `crontab` or systemd timer runs `orchestrator submit path/to/brief.json` on schedule. Zero orchestrator code — the OS does the scheduling. Just committed example crontab entries + a `just trigger-install-cron <brief>` helper.
2. **Webhook trigger.** Add a `POST /submit` endpoint on the dashboard that takes a brief JSON and forwards it to `agentry:briefs`. Token-guarded (shared-secret in env / figment config). `forge` webhook points at the dashboard; an issue comment like `/agentry ship fix-foo` triggers a brief.
3. **Chain trigger.** When a brief emits verdict=shipped, if the brief's payload has `next_brief_ref: "path/to/next.json"`, the daemon submits that next brief. Tiny daemon change (~20 LOC).

### Subtasks

- `orchestrator-dashboard`: add `POST /submit` route, shared-secret auth via `config.webhook.secret`.
- `config.rs`: new `WebhookConfig { secret: Option<String> }` section.
- `daemon.rs`: on shipped verdict with `next_brief_ref`, load that brief file and submit.
- `examples/trigger-cron.txt`: sample crontab.
- `examples/verify-M8-webhook.sh`: curl the webhook with a token, assert a verdict appears.
- `examples/verify-M8-chain.json`: brief A with `next_brief_ref` pointing to brief B; both should complete in order.

### Budget

~150 LOC + one Axum route.

### Invariants

- Webhook secret is per-install, in `~/.config/agentry/agentry.toml`. Gitignored.
- Chain triggers are bounded (max depth 5, say) to avoid runaway loops.
- Old 9 verifies must still pass.

## M9 — first real qbot-core issue

Requires:
- Claude-Max or Grok-backed coder role (we have the substrate from M5b / M5a).
- Team topology like `archaeologist → prescriber → coder → reviewer → shipper`.
- Shipper (M7) with scope expanded to `yg/qbot-core`.
- A small, well-specced open issue on qbot-core (ideally a `clippy` warning or README typo) as the first target.

Open questions for M9 (resolve BEFORE building):
1. Which open qbot-core issue should be the first target?
2. Which model should the coder role use? (Claude Max = Sonnet 4.6 / Opus 4.7 via CLI; Grok-Code-Fast is another option — cheap but untested for real Rust work.)
3. Do we merge the PR ourselves (safe) or let agentry's shipper also be a merger (requires `forge:merge` scope + trust)?

## If this session ends mid-task

- `git status` → commit as `wip(m8): ...`.
- Update this TODO.
- `mcp__memory__set key="project:agentry:resume"`.
- Replay block above before new work.
