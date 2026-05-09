# Captain doctrine — operator protocols beyond brief dispatch

This document is the companion to `docs/dogfood-protocol.md`. That doc covers brief authoring, dispatch, and observation; this one covers what the captain (human or AI) does AFTER a brief merges, when the merged code requires action beyond the substrate's automatic reach. The first concrete protocol covered is redeploy.

## Redeploy protocol

Some briefs change code that the running substrate executes — the daemon binary, captain CLI, orchestrator CLI, or role-runner binaries. Merging the PR puts the new code on develop, but the running processes keep executing whatever they were compiled with. Until the operator runs the redeploy step, the new behavior is dormant.

### When a brief MUST set redeploy_required

Set the `Brief.redeploy_required` field whenever the verbs touch one of three trees, mapped to the three RedeployTarget enum variants.

| Path | Target |
| --- | --- |
| `crates/orchestrator-runtime/src/daemon.rs` or any module the daemon depends on | `Daemon` |
| `crates/orchestrator-runtime/src/bin/orchestrator.rs` or any module it depends on | `OrchestratorCli` |
| `crates/orchestrator-runtime/src/bin/captain.rs` or any module it depends on | `CaptainCli` |

When set, the shipper appends a `## Redeploy required` block to the merged PR body listing the targets in kebab-case, with a one-line `captain redeploy` instruction. This was implemented by F8c-a / PR #452.

Role-runner binaries (shipper-runner, ci-watcher-runner, coder-claude-runner, reviewer-claude-runner, reviewer-mechanical-runner, ac-verifier-runner, pr-rebaser-runner, verifier-dol-runner) are NOT yet modeled by RedeployTarget; briefs touching `crates/agentry-role-runtime/**` need an out-of-band operator note in the PR body until F8d ships.

### How the operator redeploys

```bash
cd /var/mnt/workspaces/agentry
git -c http.sslVerify=false pull origin develop
captain redeploy --target <one of: daemon, orchestrator-cli, captain-cli, all>
```

The subcommand runs `cargo build --release` for the listed binaries into `target/release/`. For CLI targets the new binary is live the moment the build finishes; for the `daemon` target an additional swap step is required because the running orchestratord process must be replaced.

```bash
PID=$(pgrep -f release/orchestratord)
cp /proc/$PID/environ /dev/shm/agentry.env
chmod 600 /dev/shm/agentry.env
kill $PID
until ! kill -0 $PID 2>/dev/null; do sleep 1; done
nohup bash -c '
    while IFS= read -r -d "" kv; do
        export "$kv"
    done < /dev/shm/agentry.env
    exec /var/mnt/workspaces/agentry/target/release/orchestratord
' >/tmp/agentry-orchestratord.log 2>&1 &
shred -u /dev/shm/agentry.env || rm -f /dev/shm/agentry.env
```

Secrets must NEVER be put on the command line; the env-from-/proc + bash-subshell-export pattern is mandatory, not optional.

### How to confirm the new daemon is live

```bash
tail -10 /tmp/agentry-orchestratord.log
```

- `orchestratord starting dashboard_port=7800`
- `connected to Redis`
- `boot: orphan auto/* branch sweep complete`
- `agent state store ready`
- `signing key loaded`

A subsequent dispatch will pick up the new daemon; watch for log lines that exercise the new code path (e.g. `ensure_target_extracted: cache_hit / extracted / failed` for any post-F1d behavior).

### Role-runner binaries (until F8d)

For briefs that touch `crates/agentry-role-runtime/**`, the operator additionally must rebuild and install the affected runner.

```bash
cargo build --release -p agentry-role-runtime --bin <runner-name>
install -m 0755 target/release/<runner-name> ~/.local/bin/<runner-name>
```

The bind-mount in the role's JSON config (source: `${HOME}/.local/bin/<runner-name>`) means the next container spawn picks up the new binary; no daemon restart needed. This step is operator-mediated outside the modeled redeploy_required field; F8d will extend RedeployTarget and captain redeploy to cover role runners.
