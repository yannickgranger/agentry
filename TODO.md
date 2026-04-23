# Next Concrete Action

**Status:** **M0 + M1 + M2 + M3 + M4 + M5a + M5b GREEN** as of 2026-04-23.
Next: M6 — upstream `permit_overrides` narrow downstream scope (synthesizer → coder pattern).

## HARD RULE

> **Claude is Claude Max subscription only. NEVER the per-token Anthropic API.**
> Claude agents subprocess the host's `claude` CLI via bind-mounted binary + `~/.claude/.credentials.json`. No `ANTHROPIC_API_KEY`, no `anthropic` Python SDK, no HTTP to `api.anthropic.com` using an API key, EVER.

## Done

| # | Scope | Commit | Proof |
|---|-------|--------|-------|
| M0 | runtime + podman + echo | `24f7e4f` | verdict shipped |
| M1 | dashboard + SSE | `eb51c08` | /sse/verdicts live |
| M2 | registry editor (forms) | `2753ecf` | POST /roles /teams /projects |
| M3 | permit broker | `95dc1cb` | permit_violation on unauthorized tool |
| M4 | inter-role message routing | `366b1b9` | speaker → listener |
| M5a | cheap-API LLM (xAI Grok) | `38852b8` | `grok-4-fast` returned `pong` |
| M5b | Claude Max via host `claude` CLI | see `git log` | claude -p returned `pong`, zero API spend |

### Replay

```bash
export AGENTRY_REDIS_URL='redis://:RedisRationalized2026@192.168.1.152:6379'
export AGENTRY_DASHBOARD_PORT=7800
export XAI_API_KEY=$(keepassxc-cli show -sa Password --no-password \
    -k ~/agent-zero/key/claude.key ~/agent-zero/claude.kdbx "services/xai-grok")

cd /var/mnt/workspaces/agentry
cargo build --release --workspace
./target/release/orchestrator key-gen --force

for img in echo naughty speaker listener grok claude; do
  podman image exists localhost/agentry/${img}-agent:v1 \
    || (cd containers/${img}-agent && podman build -t agentry/${img}-agent:v1 -f Containerfile .)
done
./target/release/orchestrator seed

ps -eo pid,comm | awk '$2 ~ /^orchestrator/ {print $1}' | xargs -r kill -9
nohup ./target/release/orchestratord         > /tmp/agentry-orchestratord.log 2>&1 &
nohup ./target/release/orchestrator-dashboard > /tmp/agentry-dashboard.log 2>&1 &
sleep 1

for m in 0 1 2 3 4 5a 5b; do just verify-M$m; done
```

## M5b implementation notes (reference for future containers that need Claude)

- Container base: `debian:bookworm-slim` (glibc; host `claude` is a static ELF needing glibc).
- Mounts (declared in role.mounts):
  - host `~/.local/bin/claude` → container `/usr/local/bin/claude` (ro)
  - host `~/.claude/.credentials.json` → container `/root/.claude/.credentials.json` (ro)
  - host `~/.claude/settings.json` → container `/root/.claude/settings.json` (ro)
- Entrypoint sets `HOME=/root` and shells to `claude -p "<prompt>"`.
- Spawner adds `--security-opt label=disable` when role.mounts is non-empty. Reason: rootless podman on Fedora/Silverblue SELinux otherwise returns EACCES on host-owned files. `:z` mount flag is WORSE because it relabels the host path (could break host claude).
- No Anthropic API key anywhere. Auth is OAuth tokens in `.credentials.json`.

## M6 — upstream `permit_overrides` narrow downstream scope (next up)

Goal: a `synthesizer → coder` team where the synthesizer emits a Message whose payload carries `permit_overrides.fs_write = [src/a.rs]`. When `coder` spawns, its permit's fs:write scope is narrowed to that list; attempts to touch `src/b.rs` are blocked at the permit broker (same mechanism as M3).

### Subtasks

1. **Team parsing:** `MessageEdge.permit_overrides_from: Option<String>` already exists in the type; just use it. When an edge declares `permit_overrides_from: "permit_overrides"`, and the upstream emits a message whose payload has that key, the extracted value is saved per (downstream_role, field_path).
2. **Permit minting:** when minting the next role's permit, apply narrowing:
   - If `permit_overrides.fs_write` present: replace any permit_scope entries starting with `fs:write:` with exactly the listed paths.
   - Same pattern for `fs_read`, `tool_allowlist` (intersection).
3. **Broker:** file-system scope enforcement is still symbolic for M6 (we check against the stored scope; actual filesystem enforcement is a later milestone using seccomp/namespaces). The event's `tool_call.args.path` is checked against the narrowed `fs:write:*` list.
4. **Containers:**
   - `synthesizer-agent` — emits a Message with `permit_overrides.fs_write = ["/workspace/a.rs"]`, then `done shipped`.
   - `narrowed-coder-agent` — emits a `tool_call { tool:"write", args:{path:"/workspace/b.rs"} }` to probe the narrowing.
5. **Seed** synthesizer-coder-team with `synthesizer -> narrowed-coder :permit_overrides`.
6. **verify-M6.json** expects verdict=permit_violation (reason mentions fs:write narrowing, not a generic unauthorized tool).

### Budget

~100 LOC. Permit-narrowing helper + 2 small shell agents.

## M7+ backlog

| # | Scope |
|---|-------|
| M7 | E2E on toy repo with shipper role (first PR opened by agentry) |
| M8 | Project + triggers (cron / webhook) |
| M9 | First real qbot-core issue closed by agentry |

## If this session ends mid-task

- `git status` → commit as `wip(m6): ...`.
- Update this `TODO.md`.
- `mcp__memory__set key="project:agentry:resume"`.
- Replay block before new work.
