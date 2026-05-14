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

## Topology-only changes (no redeploy)

Some changes are pure topology JSON edits — adding a node, removing a node, changing fan-in policy, adding an operator-gate, renaming an edge, raising `max_retries`. These do NOT require `redeploy_required`. The runtime is a generic walker; topology data IS the methodology, and the daemon reads each brief's `(team, version)` topology at dispatch time.

### Operator workflow

1. Edit the topology JSON in `seed/topologies/<team>.json`. **Bump `version`** — topologies are `(name, version)`-keyed and the registry is strict (`orchestrator team register` rejects re-registration of an existing `(name, version)`).
2. Validate locally: `orchestrator team validate seed/topologies/<team>.json` — catches schema drift, unknown roles, malformed gate-config, and node-class typos without touching Redis.
3. Dispatch the change as a brief whose verbs include the JSON edit:
   ```
   UPDATE seed/topologies/<team>.json: bump version <N> → <N+1>
   UPDATE seed/topologies/<team>.json: <the topology change>
   ```
   `redeploy_required: []` — the daemon binary is unaffected.
4. After merge: `orchestrator team register seed/topologies/<team>.json` (NOT `captain redeploy`) — pushes the new `(name, version+1)` topology body to Redis atomically.
5. New briefs dispatched after the register step pin `topology.version: <N+1>` and execute against the new shape. Any in-flight brief pinned to `<N>` continues against the old shape — version pinning is the migration mechanism.

### Border case — binary change is needed

If the topology change requires a new `RunData` variant or a new `BriefEvent` variant (e.g. introducing a node-class whose runtime needs a new data carrier), that IS a binary change and MUST set `redeploy_required: ["daemon"]`. The cfdb fence `arch-ban-rundata-phase-names` prevents methodology-named additions; legitimate new `RunData` variants (data-shape-named, e.g. `WebhookGated { url, headers }`) are doctrinally rare but they do happen. When in doubt: if any verb in the brief edits a `.rs` file under `crates/orchestrator-runtime/` or `crates/orchestrator-types/`, set `redeploy_required: ["daemon"]`.

### Why this matters

The v2 finale (#397, council synthesis `council/v2-finale-fsm-collapse/synthesis.md`) collapsed the FSM's hardcoded phase enum into a generic topology walker precisely to make this no-redeploy path viable. Treating every topology shape change as a daemon-redeploy is the old (pre-#532) methodology-in-Rust pattern. Once a topology JSON edit lands and is registered, the dispatch path is the only thing left to update — no compile, no swap, no downtime.

## Spec authoring

Briefs that create or modify `specs/concepts/*.md` should start from a compliant skeleton emitted by `captain new-spec --concept <CamelCaseName> --target-repo <forge/repo>`. The skeleton enforces graph-specs equivalence: the H1 and the level-2 heading must be the same CamelCase token, matching a Rust type, function, or trait of that name in the target_repo's source tree. Level-4 headings (`####`) are local subsections and are not concepts; they are not subject to the equivalence check. Prose-style headings like `## Stage A — size catalog` will be rejected by graph-specs and by the coder self-review (this caused the G2 v1 wasted re-roll cited in #436).

## Bootstrap briefs for greenfield projects

`captain new-brief --bootstrap` emits a Contract with three `Behavior` anchors instead of the default single `Cfdb { qname: "TODO::replace_with_real_qname" }` anchor. The daemon does NOT verify Behavior anchors at intake — they are pass-through, accepted as-authored without resolving against cfdb or graph-specs. This is the right choice for the first 1-3 briefs of any new `target_repo`, before the project has stable Rust types or `specs/concepts/*.md` files to anchor against: there is nothing for `Cfdb { qname }` or `SpecConcept { path, section }` to resolve to, so a Cfdb-TODO anchor would just be a placeholder waiting to be rewritten before dispatch. Three Behavior anchors give the captain a doctrine-aligned scaffold (test invariant, output invariant, structural invariant) that maps cleanly to the verifications a greenfield brief actually needs. As soon as the project has anchorable surface — concrete types, functions, or spec sections — replace `Behavior` with `Cfdb` or `SpecConcept` so the daemon can validate at intake. The canonical example of bootstrap-mode use is glean V0 (briefs G1-G7 dispatched against `yg/glean`), which used Behavior anchors throughout the bring-up phase before the project accumulated enough surface to switch to Cfdb anchors.
