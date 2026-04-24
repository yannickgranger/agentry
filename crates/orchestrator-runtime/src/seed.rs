//! Seed the Redis registry with the agent roles and team topologies.
//!
//! Each role carries its entrypoint as an inline bash script (no per-agent
//! Containerfile). The spawner picks a stock public base image, installs the
//! role's declared `binaries` via `package_manager`, then execs the script.
//!
//! Idempotent: overwrites existing records with current definitions.

use crate::{redis_io, Config, Result};
use orchestrator_types::{
    AgentRole, MessageEdge, Mount, PackageManager, PermitScope, RoleName, SubstrateClass, TeamName,
    TeamTopology, ToolAllowlist, WorkspaceMount,
};

// ---------------------------------------------------------------------------
// Entrypoint scripts — inlined from what used to live in containers/*/entrypoint.sh.
// Each is a self-contained bash program that reads the startup JSON bundle on
// stdin (unless it ignores it) and emits NDJSON Events on stdout.
//
// Scripts that need structured jq-built events include `BASH_PRELUDE` at the
// top (via concat!) to pull in shared `emit_event` / `emit_done` helpers.
// Minimal scripts that only emit hand-formatted printf lines skip the prelude.
// ---------------------------------------------------------------------------

/// Bash helpers injected at the top of scripts that build structured events
/// via jq. Defines `emit_event <payload-json>`, `emit_done <verdict>`,
/// `emit_finding <severity> <tool> <category> <message>` (mechanical origin),
/// `emit_finding_model <severity> <agent-id> <category> <message>` (LLM origin),
/// and `emit_message <to> <payload-json>`.
const BASH_PRELUDE: &str = r#"emit_event() {
    jq -nc --arg at "$(date -Iseconds)" --argjson payload "$1" \
        '{at:$at, type:"event", payload:$payload}'
}
emit_done() {
    jq -nc --arg at "$(date -Iseconds)" --arg v "$1" \
        '{at:$at, type:"done", verdict:$v}'
}
emit_finding() {
    # args: severity (blocker|warn), tool, category, message
    jq -nc --arg at "$(date -Iseconds)" \
        --arg sev "$1" --arg tool "$2" --arg cat "$3" --arg msg "$4" \
        '{at:$at, type:"finding", finding:{
            severity:$sev,
            origin:{kind:"mechanical", tool:$tool, rule:null},
            file:null, line:null,
            category:$cat, message:$msg, suggested_fix:null
        }}'
}
emit_message() {
    # args: to (role name), payload (JSON)
    jq -nc --arg at "$(date -Iseconds)" --arg to "$1" --argjson payload "$2" \
        '{at:$at, type:"message", to:$to, payload:$payload}'
}
emit_finding_model() {
    # args: severity (blocker|warn), reviewer_agent_id, category, message
    jq -nc --arg at "$(date -Iseconds)" \
        --arg sev "$1" --arg aid "$2" --arg cat "$3" --arg msg "$4" \
        '{at:$at, type:"finding", finding:{
            severity:$sev,
            origin:{kind:"model", reviewer_agent_id:$aid},
            file:null, line:null,
            category:$cat, message:$msg, suggested_fix:null
        }}'
}
"#;

const ECHO_SCRIPT: &str = r#"#!/usr/bin/env bash
# Reads a startup JSON from stdin (ignored), emits one event + one terminal done.
set -euo pipefail
cat > /dev/null
printf '{"at":"%s","type":"event","payload":{"msg":"hello"}}\n' "$(date -Iseconds)"
printf '{"at":"%s","type":"done","verdict":"shipped"}\n' "$(date -Iseconds)"
"#;

const NAUGHTY_SCRIPT: &str = r#"#!/usr/bin/env bash
# Emits a tool_call event claiming "write" — broker must block when allowlist
# is e.g. ["read"] only. If broker fails to block, we hit the done-shipped
# line below, which is the regression signal.
set -euo pipefail
cat > /dev/null
printf '{"at":"%s","type":"event","payload":{"msg":"about to misbehave"}}\n' "$(date -Iseconds)"
printf '{"at":"%s","type":"tool_call","call":{"tool":"write","args":{"path":"/etc/shadow"}}}\n' "$(date -Iseconds)"
printf '{"at":"%s","type":"done","verdict":"shipped"}\n' "$(date -Iseconds)"
"#;

const SPEAKER_SCRIPT: &str = r#"#!/usr/bin/env bash
# Emits one Message to "listener-agent", then done shipped.
set -euo pipefail
cat > /dev/null
printf '{"at":"%s","type":"event","payload":{"msg":"speaker-agent started"}}\n' "$(date -Iseconds)"
printf '{"at":"%s","type":"message","to":"listener-agent","payload":{"greeting":"hello from speaker"}}\n' "$(date -Iseconds)"
printf '{"at":"%s","type":"done","verdict":"shipped"}\n' "$(date -Iseconds)"
"#;

const LISTENER_SCRIPT: &str = r#"#!/usr/bin/env bash
# Reads team_context.messages from stdin JSON bundle, emits one received event
# per incoming message, then done shipped.
set -euo pipefail
bundle="$(cat)"
count="$(jq -r '.team_context.messages | length' <<<"$bundle")"
printf '{"at":"%s","type":"event","payload":{"msg":"listener-agent started","received_count":%s}}\n' "$(date -Iseconds)" "$count"
i=0
while [ "$i" -lt "$count" ]; do
    msg_json="$(jq -c ".team_context.messages[$i]" <<<"$bundle")"
    payload="$(jq -c ".payload" <<<"$msg_json")"
    from="$(jq -r ".from" <<<"$msg_json")"
    printf '{"at":"%s","type":"event","payload":{"received_from":"%s","payload":%s}}\n' "$(date -Iseconds)" "$from" "$payload"
    i=$((i+1))
done
printf '{"at":"%s","type":"done","verdict":"shipped"}\n' "$(date -Iseconds)"
"#;

const GROK_SCRIPT: &str = r#"#!/usr/bin/env bash
# Reads startup JSON, calls xAI Grok API, emits one event with the model reply,
# then done shipped. No Anthropic API.
set -euo pipefail
bundle="$(cat)"
prompt="$(jq -r '.brief.payload.prompt // "Hello?"' <<<"$bundle")"
model="$(jq -r '.role.model // "grok-4-fast"' <<<"$bundle")"
system="$(jq -r '.role.system_prompt // ""' <<<"$bundle")"

if [ -z "${XAI_API_KEY:-}" ]; then
    emit_event '{"msg":"XAI_API_KEY not provided — role.passthru_env must include it and orchestratord env must set it"}'
    emit_done "failed"
    exit 0
fi

messages=$(jq -nc --arg sys "$system" --arg user "$prompt" '
    ( if $sys == "" then [] else [{role:"system", content:$sys}] end )
    + [{role:"user", content:$user}]')
req=$(jq -nc --arg m "$model" --argjson msgs "$messages" \
    '{model:$m, messages:$msgs, max_tokens:512}')

emit_event "$(jq -nc --arg m "$model" --arg p "$prompt" '{msg:"calling Grok", model:$m, prompt_chars:($p|length)}')"

resp=$(curl -sS -f -X POST "https://api.x.ai/v1/chat/completions" \
    -H "Authorization: Bearer $XAI_API_KEY" \
    -H "Content-Type: application/json" \
    -d "$req" 2>&1) || {
    emit_event "$(jq -nc --arg err "$resp" '{error:"xAI call failed", detail:$err}')"
    emit_done "failed"
    exit 0
}

reply="$(jq -r '.choices[0].message.content' <<<"$resp")"
tokens_in="$(jq -r '.usage.prompt_tokens // 0' <<<"$resp")"
tokens_out="$(jq -r '.usage.completion_tokens // 0' <<<"$resp")"

emit_event "$(jq -nc --arg r "$reply" --argjson ti "$tokens_in" --argjson to "$tokens_out" \
    '{reply:$r, tokens_in:$ti, tokens_out:$to}')"
emit_done "shipped"
"#;

const CLAUDE_SCRIPT: &str = r#"#!/usr/bin/env bash
# Claude Max via host `claude` CLI. Headless mode (`-p`). Binary is bind-mounted
# from host at /usr/local/bin/claude; OAuth credentials at /root/.claude/.
set -euo pipefail
bundle="$(cat)"
prompt="$(jq -r '.brief.payload.prompt // "Hello?"' <<<"$bundle")"

# The bind-mount target directory must exist before podman mounts the creds
# file into /root/.claude/. On a stock debian:bookworm-slim image HOME=/root
# already exists but /root/.claude/ does not, so create it defensively.
mkdir -p /root/.claude

if [ ! -x /usr/local/bin/claude ]; then
    emit_event '{"error":"claude binary not mounted at /usr/local/bin/claude"}'
    emit_done "failed"
    exit 0
fi
if [ ! -s /root/.claude/.credentials.json ]; then
    emit_event '{"error":"/root/.claude/.credentials.json missing — role.mounts must bind it from the host"}'
    emit_done "failed"
    exit 0
fi

emit_event "$(jq -nc --arg p "$prompt" '{msg:"calling Claude Max (headless)", prompt_chars:($p|length)}')"

reply=$(HOME=/root claude -p "$prompt" 2>&1) || {
    emit_event "$(jq -nc --arg err "$reply" '{error:"claude -p failed", detail:$err}')"
    emit_done "failed"
    exit 0
}

emit_event "$(jq -nc --arg r "$reply" '{reply:$r}')"
emit_done "shipped"
"#;

const SYNTHESIZER_SCRIPT: &str = r#"#!/usr/bin/env bash
# Emits a Message to "narrowed-coder" whose payload carries `permit_overrides`.
# The orchestrator extracts it and narrows the coder's permit before spawn.
set -euo pipefail
cat > /dev/null
printf '{"at":"%s","type":"event","payload":{"msg":"synthesizer producing contract"}}\n' "$(date -Iseconds)"
printf '{"at":"%s","type":"message","to":"narrowed-coder","payload":{"files_to_touch":["/workspace/allowed.rs"],"permit_overrides":{"fs_write":["/workspace/allowed.rs"]}}}\n' "$(date -Iseconds)"
printf '{"at":"%s","type":"done","verdict":"shipped"}\n' "$(date -Iseconds)"
"#;

const NARROWED_CODER_SCRIPT: &str = r#"#!/usr/bin/env bash
# Base permit has fs:write:/workspace/**, narrowed to fs:write:/workspace/allowed.rs
# only. Attempts a write to /workspace/denied.rs — broker must block.
set -euo pipefail
cat > /dev/null
printf '{"at":"%s","type":"event","payload":{"msg":"narrowed-coder starting"}}\n' "$(date -Iseconds)"
printf '{"at":"%s","type":"tool_call","call":{"tool":"write","args":{"path":"/workspace/denied.rs","content":"// should not land"}}}\n' "$(date -Iseconds)"
# If we reach here, broker didn't block — regression.
printf '{"at":"%s","type":"event","payload":{"msg":"NOT BLOCKED — regression if you see this"}}\n' "$(date -Iseconds)"
printf '{"at":"%s","type":"done","verdict":"shipped"}\n' "$(date -Iseconds)"
"#;

/// Probe role used by agentry's own workspace tests. Writes and reads a file
/// under the brief's workspace mount so integration tests can assert that
/// the host dir is live during the run and gone after teardown.
const WORKSPACE_PROBE_SCRIPT: &str = r#"#!/usr/bin/env bash
set -euo pipefail
cat > /dev/null
printf '{"at":"%s","type":"event","payload":{"msg":"workspace-probe starting"}}\n' "$(date -Iseconds)"
touch /workspace/hello
echo "content from workspace-probe" > /workspace/hello
body=$(cat /workspace/hello)
printf '{"at":"%s","type":"event","payload":{"msg":"wrote and read","body":"%s"}}\n' "$(date -Iseconds)" "$body"
printf '{"at":"%s","type":"done","verdict":"shipped"}\n' "$(date -Iseconds)"
"#;

/// Probe role used by agentry's own sccache-wiring tests. Installs a small
/// rust toolchain via apk, compiles a trivial program twice under
/// `sccache rustc`, and asserts the second compile hits the shared
/// sccache-redis cache. Proves the `--network=agentry-net` flag reaches the
/// cache container by name and the `sccache=true` env vars are honoured.
const SCCACHE_PROBE_SCRIPT: &str = r#"#!/usr/bin/env bash
set -euo pipefail
cat > /dev/null

emit_event() {
    jq -nc --arg at "$(date -Iseconds)" --argjson p "$1" '{at:$at,type:"event",payload:$p}'
}
emit_done() {
    jq -nc --arg at "$(date -Iseconds)" --arg v "$1" '{at:$at,type:"done",verdict:$v}'
}

emit_event "$(jq -nc --arg wrap "$RUSTC_WRAPPER" --arg ep "$SCCACHE_REDIS_ENDPOINT" '{msg:"sccache-probe starting",RUSTC_WRAPPER:$wrap,SCCACHE_REDIS_ENDPOINT:$ep}')"

# Explicitly start the sccache server so stats persist across compile calls.
# Without this, each `sccache rustc` call spins up a transient server and
# the stats don't accumulate.
sccache --start-server 2>&1 | head -3 > /tmp/sccache-start.log || true
sleep 1

echo 'pub fn hello() -> i32 { 42 }' > /tmp/hello.rs
mkdir -p /tmp/out1 /tmp/out2

# Compile twice with identical inputs, different --out-dir. sccache's rust
# parser rejects `-o <file>` invocations (Non-cacheable reason "-o") so we
# use `--out-dir` + `--emit=metadata` — same shape cargo emits. First call
# populates the cache across the agentry-net hop; second reads it back. A
# cache hit proves the container→agentry-sccache-redis round-trip works.
sccache rustc --edition=2021 --crate-name=hello --crate-type=rlib --emit=metadata --out-dir=/tmp/out1 /tmp/hello.rs 2>/tmp/c1.err
sccache rustc --edition=2021 --crate-name=hello --crate-type=rlib --emit=metadata --out-dir=/tmp/out2 /tmp/hello.rs 2>/tmp/c2.err

sccache_version=$(sccache --version 2>&1 | head -1)
stats_text=$(sccache --show-stats 2>&1 | head -40)

# Use text output — json formats vary across sccache versions. The cache-hit
# count lives on a "Cache hits  N" line.
hits=$(echo "$stats_text" | awk 'tolower($0) ~ /^cache hits/ && $NF ~ /^[0-9]+$/ {print $NF; exit}')
misses=$(echo "$stats_text" | awk 'tolower($0) ~ /^cache misses/ && $NF ~ /^[0-9]+$/ {print $NF; exit}')
hits=${hits:-0}
misses=${misses:-0}

emit_event "$(jq -nc --arg v "$sccache_version" --argjson h "$hits" --argjson m "$misses" --arg s "$stats_text" '{msg:"sccache stats",version:$v,hits:$h,misses:$m,raw:$s}')"

if [ "$hits" -ge 1 ] || [ "$misses" -ge 1 ]; then
    # Cache was actually consulted (miss on first compile OK; second should hit).
    emit_done "shipped"
else
    c1_tail=$(tail -3 /tmp/c1.err 2>/dev/null || echo "")
    c2_tail=$(tail -3 /tmp/c2.err 2>/dev/null || echo "")
    start_log=$(cat /tmp/sccache-start.log 2>/dev/null || echo "")
    emit_event "$(jq -nc --argjson h "$hits" --argjson m "$misses" --arg c1 "$c1_tail" --arg c2 "$c2_tail" --arg s "$start_log" '{error:"sccache recorded no activity",hits:$h,misses:$m,compile1_stderr:$c1,compile2_stderr:$c2,server_start:$s}')"
    emit_done "failed"
fi
"#;

/// Probe role used by agentry's own wall-clock-timeout tests. Sleeps long
/// enough that the spawner's `permit.max_wall_seconds` guard must fire. If
/// the probe ever reaches its `done shipped` line, the budget enforcement
/// regressed.
const TIMEOUT_PROBE_SCRIPT: &str = r#"#!/usr/bin/env bash
set -euo pipefail
cat > /dev/null
printf '{"at":"%s","type":"event","payload":{"msg":"timeout-probe sleeping; spawner should kill me"}}\n' "$(date -Iseconds)"
sleep 300
printf '{"at":"%s","type":"done","verdict":"shipped"}\n' "$(date -Iseconds)"
"#;

/// Coder role for the `agentry-self-host-v0` team. The workspace arrives
/// pre-cloned (as a `git worktree` off a shared bare clone) at `/workspace`
/// with branch `auto/<brief_id>` already checked out — the daemon's
/// `workspace::allocate` did the work. The coder calls `claude -p` with a
/// verb-structured prompt built from the brief payload, runs the acceptance
/// command, and commits locally. Does NOT push — the shipper does that
/// after the reviewer approves.
const CODER_CLAUDE_AGENTRY_SCRIPT: &str = r##"#!/usr/bin/env bash
set -euo pipefail
bundle="$(cat)"

brief_id=$(jq -r '.brief.id' <<<"$bundle")
target_repo=$(jq -r '.brief.payload.target_repo // "yg/agentry"' <<<"$bundle")
base_branch=$(jq -r '.brief.payload.base_branch // "develop"' <<<"$bundle")
issue_title=$(jq -r '.brief.payload.issue_title // ""' <<<"$bundle")
issue_body=$(jq -r '.brief.payload.issue_body // ""' <<<"$bundle")
acceptance=$(jq -r '.brief.payload.acceptance // "true"' <<<"$bundle")
forge_host=$(jq -r '.brief.payload.forge_host // "agency.lab:3000"' <<<"$bundle")

if [ -z "${GITEA_TOKEN:-}" ]; then
    emit_event '{"error":"GITEA_TOKEN not in env"}'
    emit_done "failed"; exit 0
fi

mkdir -p /root/.claude
export HOME=/root

cd /workspace
git config --global user.email "coder-claude-agentry@agentry.lab"
git config --global user.name "coder-claude-agentry"
git config --global http.sslVerify false

branch="auto/${brief_id}"
# Workspace is a git worktree allocated by the daemon; it is already on
# branch auto/${brief_id}, forked from origin/${base_branch}.
emit_event "$(jq -nc --arg b "$branch" '{msg:"workspace worktree ready",branch:$b}')"

cat > /tmp/brief_vars.sh <<'VARS_EOF'
#!/bin/bash
VARS_EOF
printf 'export brief_id=%q\n' "$brief_id"        >> /tmp/brief_vars.sh
printf 'export base_branch=%q\n' "$base_branch"  >> /tmp/brief_vars.sh
printf 'export issue_title=%q\n' "$issue_title"  >> /tmp/brief_vars.sh
printf 'export acceptance=%q\n' "$acceptance"    >> /tmp/brief_vars.sh
printf 'export branch=%q\n' "$branch"            >> /tmp/brief_vars.sh

cat > /tmp/prompt.txt <<PROMPT
You are the coder role inside the agentry autonomous team, operating in the
container-local working directory /workspace. The repo is cloned at
branch "$base_branch"; you are on a fresh branch "$branch".

Your task is described in verb-structured form below. Follow it literally:
each verb (CREATE / UPDATE / REPLACE / DELETE / MOVE) names a transformation
on a specific file:line target. Do NOT invent additional changes.

Task title: $issue_title

Task body:
$issue_body

Constraints:
- Operate only inside /workspace. Never touch files outside it.
- When you are done editing, the acceptance command below must pass. You
  may run it yourself to check. The orchestrator will re-run it before
  accepting the diff:
    $acceptance
- Do not commit or push. The orchestrator handles commit and push on your
  behalf after you exit.

When the transformations are complete and the acceptance passes, simply
report success and exit.
PROMPT

emit_event "$(jq -nc --arg len "$(wc -c < /tmp/prompt.txt)" '{msg:"calling claude -p",prompt_bytes:$len}')"

reply=$(HOME=/root claude -p "$(cat /tmp/prompt.txt)" 2>&1) || {
    emit_event "$(jq -nc --arg err "$reply" '{error:"claude -p failed",detail:$err}')"
    emit_done "failed"; exit 0
}

emit_event "$(jq -nc --arg len "${#reply}" '{msg:"claude reply received",bytes:$len}')"
"##;

const CODER_CLAUDE_AGENTRY_EXITPOINT: &str = r##"#!/usr/bin/env bash
set -euo pipefail
. /tmp/brief_vars.sh
cd /workspace

# Baseline fmt — always run, no install dependency. rustfmt ships with
# rustup and is already provisioned via extra_bootstrap. Protects against
# quality-hygiene being absent (its `cargo install` is best-effort).
emit_event '{"msg":"running cargo fmt --all (baseline)"}'
if ! cargo fmt --all 2>/tmp/fmt.err; then
    err=$(tail -50 /tmp/fmt.err)
    emit_event "$(jq -nc --arg err "$err" '{error:"cargo fmt --all failed",detail:$err}')"
    emit_finding "blocker" "cargo-fmt" "fmt" "$err"
    emit_done "failed"; exit 0
fi
emit_event '{"msg":"cargo fmt --all clean"}'

# quality-hygiene — role-local hygiene gate. If the binary was installed by
# extra_bootstrap, run --fix so the commit is clean. If the install failed
# (binary absent), skip and let the reviewer catch anything hygiene would have.
if command -v quality-hygiene >/dev/null 2>&1; then
    emit_event '{"msg":"running quality-hygiene --fix"}'
    if ! quality-hygiene --fix --workspace /workspace --base "${base_branch}" >/tmp/qh.out 2>/tmp/qh.err; then
        err=$(tail -100 /tmp/qh.err)
        emit_event "$(jq -nc --arg err "$err" '{error:"quality-hygiene --fix failed",detail:$err}')"
        emit_finding "blocker" "quality-hygiene" "hygiene" "$err"
        emit_done "failed"; exit 0
    fi
    emit_event '{"msg":"quality-hygiene --fix clean"}'
else
    emit_event '{"msg":"quality-hygiene not installed, skipping role-local gate"}'
fi

# Acceptance self-check — same command the reviewer will run.
if eval "$acceptance" >/tmp/acc.out 2>/tmp/acc.err; then
    emit_event '{"msg":"acceptance passed (self-check)"}'
else
    err=$(tail -50 /tmp/acc.err)
    emit_event "$(jq -nc --arg err "$err" '{error:"acceptance failed (self-check)",detail:$err}')"
    emit_finding "blocker" "cargo" "acceptance" "$err"
    emit_done "failed"; exit 0
fi

# Stage + commit whatever claude (and quality-hygiene --fix) changed.
git add -A
if git diff --cached --quiet; then
    emit_event '{"error":"no changes produced"}'
    emit_done "failed"; exit 0
fi
git commit -m "auto(${brief_id}): ${issue_title}" > /dev/null
sha=$(git rev-parse HEAD)
emit_event "$(jq -nc --arg br "$branch" --arg s "$sha" '{msg:"committed",branch:$br,sha:$s}')"

emit_done "shipped"
"##;

/// Mechanical reviewer role. Reads the coder's workspace read-only,
/// re-runs the acceptance command in an isolated build dir (`CARGO_TARGET_DIR=/tmp/review-target`),
/// emits `shipped` on success and `failed` on any non-zero exit with the
/// tail of stderr/stdout in the reason payload.
const REVIEWER_MECHANICAL_AGENTRY_SCRIPT: &str = r##"#!/usr/bin/env bash
set -euo pipefail
bundle="$(cat)"

base_branch=$(jq -r '.brief.payload.base_branch // "develop"' <<<"$bundle")
acceptance=$(jq -r '.brief.payload.acceptance // "cargo test --workspace"' <<<"$bundle")

if [ ! -d /workspace/.git ] && [ ! -f /workspace/.git ]; then
    emit_event '{"error":"workspace missing — coder did not produce it"}'
    emit_done "failed"; exit 0
fi

cd /workspace

emit_event '{"msg":"reviewer starting"}'

# Diff summary. base_branch is on `origin/<base_branch>`.
diff_stat=$(git diff --stat "${base_branch}"..HEAD 2>&1 | tail -1 || true)
emit_event "$(jq -nc --arg d "$diff_stat" '{msg:"diff",summary:$d}')"

# Workspace is read-only for this role — redirect Cargo's target/ to /tmp.
export CARGO_TARGET_DIR=/tmp/review-target
mkdir -p "$CARGO_TARGET_DIR"

emit_event "$(jq -nc --arg a "$acceptance" '{msg:"running acceptance (isolated)",cmd:$a}')"
if eval "$acceptance" >/tmp/rev.out 2>/tmp/rev.err; then
    emit_event '{"msg":"acceptance passed"}'
    emit_done "shipped"
else
    err=$(tail -50 /tmp/rev.err)
    out=$(tail -20 /tmp/rev.out)
    emit_event "$(jq -nc --arg err "$err" --arg out "$out" '{error:"acceptance failed",stderr:$err,stdout:$out}')"
    # Minimal single-finding emit — one blocker bundling the full stderr+stdout
    # tail. Per-lint structured parsing (cargo clippy --message-format=json,
    # test --format json) is a follow-up; the primitive is the point here.
    combined=$(printf '%s\n---stdout---\n%s' "$err" "$out" | head -c 2000)
    emit_finding "blocker" "cargo" "acceptance" "$combined"
    emit_done "rework_needed"
fi
"##;

/// LLM reviewer role for the `agentry-self-host-v0` team. Reads the diff
/// produced by the coder, prompts claude -p for a JSON array of findings,
/// emits each as a Finding event, and resolves rework_needed if any Blocker
/// is present. Runs sequentially after the mechanical reviewer.
const REVIEWER_CLAUDE_AGENTRY_SCRIPT: &str = r##"#!/usr/bin/env bash
set -euo pipefail
bundle="$(cat)"

base_branch=$(jq -r '.brief.payload.base_branch // "develop"' <<<"$bundle")
issue_title=$(jq -r '.brief.payload.issue_title // ""' <<<"$bundle")
issue_body=$(jq -r '.brief.payload.issue_body // ""' <<<"$bundle")
agent_id=$(jq -r '.permit.agent_id' <<<"$bundle")

if [ ! -d /workspace/.git ] && [ ! -f /workspace/.git ]; then
    emit_event '{"error":"workspace is not a git repo — coder did not produce it"}'
    emit_done "failed"; exit 0
fi

mkdir -p /root/.claude
export HOME=/root
cd /workspace

# Diff against develop. The coder produces commits on top of origin/develop;
# we review what was ADDED, not the whole file set.
if ! git diff "${base_branch}..HEAD" > /tmp/diff.patch 2>/tmp/diff.err; then
    err=$(tail -20 /tmp/diff.err)
    emit_event "$(jq -nc --arg err "$err" '{error:"git diff failed",detail:$err}')"
    emit_done "failed"; exit 0
fi

diff_bytes=$(wc -c < /tmp/diff.patch)
if [ "$diff_bytes" -eq 0 ]; then
    emit_event '{"error":"empty diff — coder produced no changes"}'
    emit_done "failed"; exit 0
fi

emit_event "$(jq -nc --argjson b "$diff_bytes" '{msg:"reviewing diff",diff_bytes:$b}')"

# Review prompt. The output format is strict so downstream parsing is
# deterministic. Any prose (fences, explanations) breaks the jq parse and
# the script resolves the role as Failed — signaling prompt drift.
cat > /tmp/rev_prompt.txt <<PROMPT
You are a senior code reviewer for the agentry project — a Rust workspace
that orchestrates short-lived agent containers. Review the following diff
produced against branch "${base_branch}" in response to this task:

TITLE: ${issue_title}

BODY (first 2000 chars):
$(printf '%s' "$issue_body" | head -c 2000)

--- DIFF ---
$(cat /tmp/diff.patch)
--- END DIFF ---

Output EXACTLY a JSON array of findings — nothing else. No markdown fences,
no prose, no preamble, no explanation. Each element:

{
  "severity": "blocker" | "warn",
  "category": "design" | "naming" | "clarity" | "invariant" | "other",
  "message": "one-sentence human-readable description (max 200 chars)"
}

Guidance:
- \`blocker\` = ships-broken, security-risk, invariant-violation, wrong abstraction.
- \`warn\` = minor cleanup, non-load-bearing style.
- If the diff is acceptable as-is, output exactly: []
- Maximum 6 findings. Prefer a single Blocker over many Warns.
- Do not comment on fmt/clippy/test — those are mechanical-reviewer scope.

Your response, right now, starting with [ and ending with ]:
PROMPT

response=$(HOME=/root claude -p "$(cat /tmp/rev_prompt.txt)" 2>&1) || {
    emit_event "$(jq -nc --arg err "$response" '{error:"claude -p failed",detail:$err}')"
    emit_done "failed"; exit 0
}

# Tolerate (and strip) leading/trailing fences if claude adds them despite
# the instruction — common drift pattern.
cleaned=$(printf '%s' "$response" | sed -e 's/^```json$//' -e 's/^```$//' -e '/^$/d' | tr -d '\r')
# Find first [ and last ] — slice.
start=$(printf '%s' "$cleaned" | grep -b -m1 '\[' | head -1 | cut -d: -f1)
end=$(printf '%s' "$cleaned" | grep -bo '\]' | tail -1 | cut -d: -f1)
if [ -z "$start" ] || [ -z "$end" ]; then
    emit_event "$(jq -nc --arg r "$(printf '%s' "$cleaned" | head -c 300)" '{error:"claude response missing JSON array brackets",head:$r}')"
    emit_done "failed"; exit 0
fi
payload=$(printf '%s' "$cleaned" | tail -c +$((start+1)) | head -c $((end-start+1)))

if ! printf '%s' "$payload" | jq -e 'type == "array"' >/dev/null 2>&1; then
    emit_event "$(jq -nc --arg r "$(printf '%s' "$payload" | head -c 300)" '{error:"claude response not a JSON array",head:$r}')"
    emit_done "failed"; exit 0
fi

count=$(printf '%s' "$payload" | jq 'length')
emit_event "$(jq -nc --argjson n "$count" '{msg:"claude review parsed",findings_count:$n}')"

# Emit each finding as a Finding event. A file marker captures whether any
# Blocker was seen, since a while-read piped subshell can't set outer vars.
rm -f /tmp/has_blocker.marker
printf '%s' "$payload" | jq -c '.[]' | while read -r finding; do
    severity=$(jq -r '.severity // "warn"' <<<"$finding")
    category=$(jq -r '.category // "other"' <<<"$finding")
    message=$(jq -r '.message // ""' <<<"$finding")
    emit_finding_model "$severity" "$agent_id" "$category" "$message"
    if [ "$severity" = "blocker" ]; then
        touch /tmp/has_blocker.marker
    fi
done

if [ -f /tmp/has_blocker.marker ]; then
    emit_event '{"msg":"blockers present — requesting rework"}'
    emit_done "rework_needed"
else
    emit_event '{"msg":"no blockers — claude-reviewer passes"}'
    emit_done "shipped"
fi
"##;

/// Shipper role for the `agentry-self-host-v0` team. Pushes the branch
/// already committed in the brief's workspace (by the coder) and opens a
/// PR on the forge. Emits the PR URL and number in its terminal event.
const SHIPPER_AGENTRY_SCRIPT: &str = r##"#!/usr/bin/env bash
set -euo pipefail
bundle="$(cat)"

brief_id=$(jq -r '.brief.id' <<<"$bundle")
target_repo=$(jq -r '.brief.payload.target_repo // "yg/agentry"' <<<"$bundle")
base_branch=$(jq -r '.brief.payload.base_branch // "develop"' <<<"$bundle")
pr_title=$(jq -r '.brief.payload.pr_title // ("auto(" + .brief.id + ")")' <<<"$bundle")
pr_body=$(jq -r '.brief.payload.pr_body // "Agentry-produced PR. See brief trace stream."' <<<"$bundle")
forge_host=$(jq -r '.brief.payload.forge_host // "agency.lab:3000"' <<<"$bundle")
branch="auto/${brief_id}"

if [ -z "${GITEA_TOKEN:-}" ]; then
    emit_event '{"error":"GITEA_TOKEN not in env"}'
    emit_done "failed"; exit 0
fi

if [ ! -d /workspace/.git ] && [ ! -f /workspace/.git ]; then
    emit_event '{"error":"workspace missing — coder did not produce it"}'
    emit_done "failed"; exit 0
fi

cd /workspace
git config http.sslVerify false
git config user.email "shipper-agentry@agentry.lab"
git config user.name "shipper-agentry"

# DO NOT `git remote set-url` to a token-bearing URL: this is a worktree
# off a shared bare clone, and `set-url` would write the token into the
# bare clone's config — visible to every other brief that reuses this
# bare. Instead, pass the Authorization header on this single push only,
# via `-c http.extraheader`. Security-clean: the header is in the
# command's argv, not on disk.
emit_event "$(jq -nc --arg b "$branch" '{msg:"pushing branch",branch:$b}')"
if ! git -c http.sslVerify=false \
        -c http.extraheader="Authorization: token ${GITEA_TOKEN}" \
        push -u origin "$branch" 2>/tmp/push.err; then
    err=$(tail -20 /tmp/push.err)
    emit_event "$(jq -nc --arg err "$err" '{error:"git push failed",detail:$err}')"
    emit_done "failed"; exit 0
fi

owner="${target_repo%%/*}"
repo_name="${target_repo##*/}"
emit_event "$(jq -nc --arg r "$target_repo" --arg b "$branch" '{msg:"opening PR",repo:$r,head:$b}')"

pr_body_json=$(jq -n --arg t "$pr_title" --arg b "$pr_body" --arg h "$branch" --arg base "$base_branch" \
    '{title:$t,body:$b,head:$h,base:$base}')
pr_resp=$(curl -sS -k -X POST "https://${forge_host}/api/v1/repos/${owner}/${repo_name}/pulls" \
    -H "Authorization: token ${GITEA_TOKEN}" \
    -H "Content-Type: application/json" \
    -d "$pr_body_json")

pr_url=$(jq -r '.html_url // ""' <<<"$pr_resp")
pr_number=$(jq -r '.number // 0' <<<"$pr_resp")
if [ -z "$pr_url" ] || [ "$pr_url" = "null" ]; then
    emit_event "$(jq -nc --arg err "$pr_resp" '{error:"PR API call failed",detail:$err}')"
    emit_done "failed"; exit 0
fi

emit_event "$(jq -nc --arg u "$pr_url" --argjson n "$pr_number" '{msg:"PR opened",url:$u,number:$n}')"

head_sha=$(git rev-parse HEAD)
emit_message "ci-watcher-agentry" "$(jq -nc \
    --argjson n "$pr_number" --arg u "$pr_url" --arg s "$head_sha" \
    '{pr_number:$n, pr_url:$u, head_sha:$s}')"

emit_done "shipped"
"##;

/// Ci-watcher role for the `agentry-self-host-v0` team. Reads the shipper's
/// Message payload from `TeamContext.messages` (pr_number + head_sha), polls
/// the forge's commit-status endpoint every 15s, merges the PR on CI green,
/// emits Failed with CI context on CI red.
const CI_WATCHER_AGENTRY_SCRIPT: &str = r##"#!/usr/bin/env bash
set -euo pipefail
bundle="$(cat)"

target_repo=$(jq -r '.brief.payload.target_repo // "yg/agentry"' <<<"$bundle")
forge_host=$(jq -r '.brief.payload.forge_host // "agency.lab:3000"' <<<"$bundle")
owner="${target_repo%%/*}"
repo_name="${target_repo##*/}"

# Pull pr_number + head_sha from the shipper's routed message. message_graph
# puts the shipper's payload in TeamContext.messages where from=shipper-agentry.
msg=$(jq -c '[.team_context.messages[] | select(.from=="shipper-agentry")] | last // empty' <<<"$bundle")
if [ -z "$msg" ] || [ "$msg" = "null" ]; then
    emit_event '{"error":"no shipper-agentry message in team_context — cannot locate PR to watch"}'
    emit_done "failed"; exit 0
fi

pr_number=$(jq -r '.payload.pr_number' <<<"$msg")
head_sha=$(jq -r '.payload.head_sha' <<<"$msg")
pr_url=$(jq -r '.payload.pr_url' <<<"$msg")

if [ -z "$pr_number" ] || [ "$pr_number" = "null" ] || [ -z "$head_sha" ] || [ "$head_sha" = "null" ]; then
    emit_event "$(jq -nc --arg m "$msg" '{error:"shipper message missing pr_number or head_sha",detail:$m}')"
    emit_done "failed"; exit 0
fi

emit_event "$(jq -nc --argjson n "$pr_number" --arg s "$head_sha" --arg u "$pr_url" \
    '{msg:"ci-watcher starting",pr_number:$n,head_sha:$s,pr_url:$u}')"

if [ -z "${GITEA_TOKEN:-}" ]; then
    emit_event '{"error":"GITEA_TOKEN not in env"}'
    emit_done "failed"; exit 0
fi

# Poll the combined status. Max 120 iterations × 15s = 30 min. The daemon's
# wall-clock budget from permit.max_wall_seconds is the authoritative cap;
# this loop gives up earlier only if the budget is small.
max_polls=120
status_url="https://${forge_host}/api/v1/repos/${owner}/${repo_name}/commits/${head_sha}/status"
for i in $(seq 1 "$max_polls"); do
    resp=$(curl -sS -k -H "Authorization: token ${GITEA_TOKEN}" "$status_url" 2>/tmp/ci.err) || {
        err=$(tail -5 /tmp/ci.err)
        emit_event "$(jq -nc --arg err "$err" '{error:"status GET failed",detail:$err}')"
        sleep 15; continue
    }
    state=$(jq -r '.state // "unknown"' <<<"$resp")
    emit_event "$(jq -nc --arg s "$state" --argjson i "$i" \
        '{msg:"polling CI",state:$s,iteration:$i}')"
    case "$state" in
        success)
            merge_body='{"Do":"merge"}'
            merge_resp=$(curl -sS -k -X POST \
                "https://${forge_host}/api/v1/repos/${owner}/${repo_name}/pulls/${pr_number}/merge" \
                -H "Authorization: token ${GITEA_TOKEN}" \
                -H "Content-Type: application/json" \
                -d "$merge_body" \
                -o /tmp/merge.body -w '%{http_code}')
            if [ "$merge_resp" = "200" ] || [ "$merge_resp" = "204" ]; then
                emit_event "$(jq -nc --argjson n "$pr_number" --arg u "$pr_url" \
                    '{msg:"merged",pr_number:$n,pr_url:$u}')"
                emit_done "shipped"; exit 0
            else
                detail=$(cat /tmp/merge.body 2>/dev/null || echo "")
                emit_event "$(jq -nc --arg code "$merge_resp" --arg d "$detail" \
                    '{error:"merge API call failed",http_code:$code,detail:$d}')"
                emit_done "failed"; exit 0
            fi
            ;;
        failure|error)
            # Pull the first failing context for reason.
            ctx=$(jq -r '[.statuses[]? | select(.state=="failure" or .state=="error") | .context] | .[0] // "(no context)"' <<<"$resp")
            emit_event "$(jq -nc --arg s "$state" --arg ctx "$ctx" \
                '{error:"CI red",state:$s,failing_context:$ctx}')"
            emit_done "failed"; exit 0
            ;;
        pending|unknown|"")
            sleep 15
            ;;
        *)
            emit_event "$(jq -nc --arg s "$state" '{error:"unexpected CI state",state:$s}')"
            emit_done "failed"; exit 0
            ;;
    esac
done

emit_event '{"error":"CI poll exhausted 30min without success — giving up"}'
emit_done "failed"
"##;

const SHIPPER_SCRIPT: &str = r#"#!/usr/bin/env bash
# Reads {repo, branch, file, content, commit_msg, pr_title, pr_body, base,
# forge_host} from brief.payload. Clones forge repo with GITEA_TOKEN,
# creates branch, writes file, commits, pushes, opens PR.
set -euo pipefail
bundle="$(cat)"
repo="$(jq -r '.brief.payload.repo' <<<"$bundle")"
branch="$(jq -r '.brief.payload.branch' <<<"$bundle")"
file_path="$(jq -r '.brief.payload.file' <<<"$bundle")"
content="$(jq -r '.brief.payload.content' <<<"$bundle")"
commit_msg="$(jq -r '.brief.payload.commit_msg' <<<"$bundle")"
pr_title="$(jq -r '.brief.payload.pr_title' <<<"$bundle")"
pr_body="$(jq -r '.brief.payload.pr_body' <<<"$bundle")"
base_branch="$(jq -r '.brief.payload.base // "main"' <<<"$bundle")"
forge_host="$(jq -r '.brief.payload.forge_host // "agency.lab:3000"' <<<"$bundle")"

if [ -z "${GITEA_TOKEN:-}" ]; then
    emit_event '{"error":"GITEA_TOKEN not in env — role.passthru_env must include it"}'
    emit_done "failed"
    exit 0
fi

git config --global user.email "shipper@agentry.lab"
git config --global user.name "agentry-shipper"
git config --global http.sslVerify false

clone_url="https://oauth2:${GITEA_TOKEN}@${forge_host}/${repo}.git"

cd /tmp
rm -rf workrepo
emit_event "$(jq -nc --arg r "$repo" '{msg:"cloning", repo:$r}')"
git clone --depth=1 --branch "$base_branch" "$clone_url" workrepo 2>/tmp/gitclone.err || {
    emit_event "$(jq -nc --arg e "$(cat /tmp/gitclone.err)" '{error:"clone failed", detail:$e}')"
    emit_done "failed"
    exit 0
}
cd workrepo

emit_event "$(jq -nc --arg b "$branch" '{msg:"creating branch", branch:$b}')"
git checkout -b "$branch" 2>&1 >/dev/null

mkdir -p "$(dirname "$file_path")" 2>/dev/null || true
printf '%s' "$content" > "$file_path"
git add "$file_path"

emit_event "$(jq -nc --arg f "$file_path" --arg m "$commit_msg" '{msg:"committing", file:$f, commit_msg:$m}')"
git commit -m "$commit_msg" 2>&1 >/dev/null

emit_event '{"msg":"pushing"}'
git push -u origin "$branch" 2>/tmp/gitpush.err || {
    emit_event "$(jq -nc --arg e "$(cat /tmp/gitpush.err)" '{error:"push failed", detail:$e}')"
    emit_done "failed"
    exit 0
}

owner="${repo%%/*}"
repo_name="${repo##*/}"

emit_event "$(jq -nc --arg r "$repo" --arg b "$branch" '{msg:"opening PR", repo:$r, head:$b}')"

pr_body_json=$(jq -n --arg t "$pr_title" --arg b "$pr_body" --arg h "$branch" --arg base "$base_branch" \
    '{title:$t, body:$b, head:$h, base:$base}')

pr_resp=$(curl -sS -k -X POST "https://${forge_host}/api/v1/repos/${owner}/${repo_name}/pulls" \
    -H "Authorization: token ${GITEA_TOKEN}" \
    -H "Content-Type: application/json" \
    -d "$pr_body_json")

pr_url="$(jq -r '.html_url // ""' <<<"$pr_resp")"
pr_number="$(jq -r '.number // 0' <<<"$pr_resp")"

if [ -z "$pr_url" ] || [ "$pr_url" = "null" ]; then
    emit_event "$(jq -nc --arg err "$pr_resp" '{error:"PR open failed", detail:$err}')"
    emit_done "failed"
    exit 0
fi

emit_event "$(jq -nc --arg u "$pr_url" --argjson n "$pr_number" '{msg:"PR opened", url:$u, number:$n}')"
emit_done "shipped"
"#;

// ---------------------------------------------------------------------------
// Stock base images. Two families: alpine (small, fast apk) and debian
// (glibc-compatible, needed for Claude Max static-glibc binary).
// ---------------------------------------------------------------------------

const ALPINE: &str = "docker.io/library/alpine:3.21";
const DEBIAN: &str = "docker.io/library/debian:bookworm-slim";

/// Container-scoped Claude permissions. Checked into the repo so agentry
/// owns its own container settings; not derived from the host user's
/// `~/.claude/settings.json` (which is host-path-scoped and drifts).
const CONTAINER_CLAUDE_SETTINGS: &str =
    include_str!("../../../containers/claude/container-settings.json");

/// Materialize the embedded container settings to a stable host path and
/// return it. Idempotent — overwrites on every call so the materialized
/// file always matches the checked-in source.
fn materialize_container_claude_settings() -> Result<String> {
    let home = std::env::var("HOME").unwrap_or_else(|_| "/var/home/yg".into());
    let dir = format!("{home}/.config/agentry");
    std::fs::create_dir_all(&dir)?;
    let path = format!("{dir}/claude-container-settings.json");
    std::fs::write(&path, CONTAINER_CLAUDE_SETTINGS)?;
    Ok(path)
}

/// Seed the registry (roles + teams) using the URL from `Config`.
pub async fn seed_m0(cfg: &Config) -> Result<()> {
    let mut conn = redis_io::connect(&cfg.redis.url).await?;
    let claude_settings_path = materialize_container_claude_settings()?;

    // ---- echo-agent (stateless hello → done shipped) ----
    let echo = AgentRole {
        name: RoleName("echo-agent".into()),
        version: 1,
        model: None,
        system_prompt: None,
        image: ALPINE.into(),
        substrate_class: SubstrateClass::Podman,
        package_manager: PackageManager::Apk,
        entrypoint_script: ECHO_SCRIPT.into(),
        exitpoint_script: None,
        binaries: vec![],
        mcp_servers: vec![],
        tool_allowlist: ToolAllowlist(vec![]),
        permit_scope: PermitScope(vec!["net:deny:*".into()]),
        passthru_env: vec![],
        extra_bootstrap: vec![],
        mounts: vec![],
        workspace_mount: None,
        sccache: false,
    };
    let echo_team = TeamTopology {
        name: TeamName("echo-team".into()),
        version: 1,
        roles: vec![echo.name.clone()],
        message_graph: Vec::<MessageEdge>::new(),
        terminal_role: echo.name.clone(),
        max_retries: 0,
    };

    // ---- naughty-agent (emits illegal tool_call → broker must block) ----
    let naughty = AgentRole {
        name: RoleName("naughty-agent".into()),
        version: 1,
        model: None,
        system_prompt: None,
        image: ALPINE.into(),
        substrate_class: SubstrateClass::Podman,
        package_manager: PackageManager::Apk,
        entrypoint_script: NAUGHTY_SCRIPT.into(),
        exitpoint_script: None,
        binaries: vec![],
        mcp_servers: vec![],
        tool_allowlist: ToolAllowlist(vec!["read".into()]),
        permit_scope: PermitScope(vec!["net:deny:*".into()]),
        passthru_env: vec![],
        extra_bootstrap: vec![],
        mounts: vec![],
        workspace_mount: None,
        sccache: false,
    };
    let naughty_team = TeamTopology {
        name: TeamName("naughty-team".into()),
        version: 1,
        roles: vec![naughty.name.clone()],
        message_graph: Vec::<MessageEdge>::new(),
        terminal_role: naughty.name.clone(),
        max_retries: 0,
    };

    // ---- speaker + listener (inter-role message routing) ----
    let speaker = AgentRole {
        name: RoleName("speaker-agent".into()),
        version: 1,
        model: None,
        system_prompt: None,
        image: ALPINE.into(),
        substrate_class: SubstrateClass::Podman,
        package_manager: PackageManager::Apk,
        entrypoint_script: SPEAKER_SCRIPT.into(),
        exitpoint_script: None,
        binaries: vec![],
        mcp_servers: vec![],
        tool_allowlist: ToolAllowlist(vec![]),
        permit_scope: PermitScope(vec!["net:deny:*".into()]),
        passthru_env: vec![],
        extra_bootstrap: vec![],
        mounts: vec![],
        workspace_mount: None,
        sccache: false,
    };
    let listener = AgentRole {
        name: RoleName("listener-agent".into()),
        version: 1,
        model: None,
        system_prompt: None,
        image: ALPINE.into(),
        substrate_class: SubstrateClass::Podman,
        package_manager: PackageManager::Apk,
        entrypoint_script: LISTENER_SCRIPT.into(),
        exitpoint_script: None,
        binaries: vec![],
        mcp_servers: vec![],
        tool_allowlist: ToolAllowlist(vec![]),
        permit_scope: PermitScope(vec!["net:deny:*".into()]),
        passthru_env: vec![],
        extra_bootstrap: vec![],
        mounts: vec![],
        workspace_mount: None,
        sccache: false,
    };
    let speaker_listener_team = TeamTopology {
        name: TeamName("speaker-listener-team".into()),
        version: 1,
        roles: vec![speaker.name.clone(), listener.name.clone()],
        message_graph: vec![MessageEdge {
            from: speaker.name.clone(),
            to: listener.name.clone(),
            permit_overrides_from: None,
        }],
        terminal_role: listener.name.clone(),
        max_retries: 0,
    };

    // ---- grok-echo (xAI Grok API) ----
    let grok_echo = AgentRole {
        name: RoleName("grok-echo".into()),
        version: 1,
        model: Some("grok-4-fast".into()),
        system_prompt: None,
        image: ALPINE.into(),
        substrate_class: SubstrateClass::Podman,
        package_manager: PackageManager::Apk,
        entrypoint_script: format!("{BASH_PRELUDE}{GROK_SCRIPT}"),
        exitpoint_script: None,
        binaries: vec!["curl".into()],
        mcp_servers: vec![],
        tool_allowlist: ToolAllowlist(vec![]),
        permit_scope: PermitScope(vec!["net:allow:api.x.ai".into()]),
        passthru_env: vec!["XAI_API_KEY".into()],
        extra_bootstrap: vec![],
        mounts: vec![],
        workspace_mount: None,
        sccache: false,
    };
    let grok_team = TeamTopology {
        name: TeamName("grok-echo-team".into()),
        version: 1,
        roles: vec![grok_echo.name.clone()],
        message_graph: vec![],
        terminal_role: grok_echo.name.clone(),
        max_retries: 0,
    };

    // ---- claude-echo (Claude Max via host CLI) ----
    // The `claude` binary is static glibc — alpine's musl can't run it, so
    // this role uses debian as its base.
    let home = std::env::var("HOME").unwrap_or_else(|_| "/var/home/yg".into());
    let claude_echo = AgentRole {
        name: RoleName("claude-echo".into()),
        version: 1,
        model: Some("claude-max".into()),
        system_prompt: None,
        image: DEBIAN.into(),
        substrate_class: SubstrateClass::Podman,
        package_manager: PackageManager::Apt,
        entrypoint_script: format!("{BASH_PRELUDE}{CLAUDE_SCRIPT}"),
        exitpoint_script: None,
        binaries: vec![],
        mcp_servers: vec![],
        tool_allowlist: ToolAllowlist(vec![]),
        permit_scope: PermitScope(vec![
            "net:allow:api.anthropic.com".into(), // claude CLI authed via OAuth, NOT API key
        ]),
        passthru_env: vec![],
        extra_bootstrap: vec![],
        mounts: vec![
            Mount {
                source: format!("{home}/.local/bin/claude"),
                target: "/usr/local/bin/claude".into(),
                readonly: true,
            },
            Mount {
                source: format!("{home}/.claude/.credentials.json"),
                target: "/root/.claude/.credentials.json".into(),
                readonly: true,
            },
            Mount {
                source: claude_settings_path.clone(),
                target: "/root/.claude/settings.json".into(),
                readonly: true,
            },
        ],
        workspace_mount: None,
        sccache: false,
    };
    let claude_team = TeamTopology {
        name: TeamName("claude-echo-team".into()),
        version: 1,
        roles: vec![claude_echo.name.clone()],
        message_graph: vec![],
        terminal_role: claude_echo.name.clone(),
        max_retries: 0,
    };

    // ---- synthesizer + narrowed-coder (permit_overrides narrowing) ----
    let synthesizer = AgentRole {
        name: RoleName("synthesizer".into()),
        version: 1,
        model: None,
        system_prompt: None,
        image: ALPINE.into(),
        substrate_class: SubstrateClass::Podman,
        package_manager: PackageManager::Apk,
        entrypoint_script: SYNTHESIZER_SCRIPT.into(),
        exitpoint_script: None,
        binaries: vec![],
        mcp_servers: vec![],
        tool_allowlist: ToolAllowlist(vec![]),
        permit_scope: PermitScope(vec!["net:deny:*".into()]),
        passthru_env: vec![],
        extra_bootstrap: vec![],
        mounts: vec![],
        workspace_mount: None,
        sccache: false,
    };
    let narrowed_coder = AgentRole {
        name: RoleName("narrowed-coder".into()),
        version: 1,
        model: None,
        system_prompt: None,
        image: ALPINE.into(),
        substrate_class: SubstrateClass::Podman,
        package_manager: PackageManager::Apk,
        entrypoint_script: NARROWED_CODER_SCRIPT.into(),
        exitpoint_script: None,
        binaries: vec![],
        mcp_servers: vec![],
        // Broad base — will be narrowed by synthesizer's Message.
        tool_allowlist: ToolAllowlist(vec!["write".into(), "edit".into(), "read".into()]),
        permit_scope: PermitScope(vec![
            "fs:read:/workspace/**".into(),
            "fs:write:/workspace/**".into(),
            "net:deny:*".into(),
        ]),
        passthru_env: vec![],
        extra_bootstrap: vec![],
        mounts: vec![],
        workspace_mount: None,
        sccache: false,
    };
    let narrowed_team = TeamTopology {
        name: TeamName("narrowed-team".into()),
        version: 1,
        roles: vec![synthesizer.name.clone(), narrowed_coder.name.clone()],
        message_graph: vec![MessageEdge {
            from: synthesizer.name.clone(),
            to: narrowed_coder.name.clone(),
            permit_overrides_from: Some("permit_overrides".into()),
        }],
        terminal_role: narrowed_coder.name.clone(),
        max_retries: 0,
    };

    // ---- shipper (opens PRs on the forge) ----
    let shipper = AgentRole {
        name: RoleName("shipper".into()),
        version: 1,
        model: None,
        system_prompt: None,
        image: ALPINE.into(),
        substrate_class: SubstrateClass::Podman,
        package_manager: PackageManager::Apk,
        entrypoint_script: format!("{BASH_PRELUDE}{SHIPPER_SCRIPT}"),
        exitpoint_script: None,
        binaries: vec!["git".into(), "curl".into()],
        mcp_servers: vec![],
        tool_allowlist: ToolAllowlist(vec![]),
        permit_scope: PermitScope(vec![
            "net:allow:agency.lab".into(),
            "forge:write:yg/agentry-toy".into(), // symbolic (no runtime enforcement yet)
        ]),
        passthru_env: vec!["GITEA_TOKEN".into()],
        extra_bootstrap: vec![],
        mounts: vec![],
        workspace_mount: None,
        sccache: false,
    };
    let shipper_team = TeamTopology {
        name: TeamName("shipper-solo-team".into()),
        version: 1,
        roles: vec![shipper.name.clone()],
        message_graph: vec![],
        terminal_role: shipper.name.clone(),
        max_retries: 0,
    };

    // ---- workspace-probe (for workspace regression tests) ----
    let workspace_probe = AgentRole {
        name: RoleName("workspace-probe".into()),
        version: 1,
        model: None,
        system_prompt: None,
        image: ALPINE.into(),
        substrate_class: SubstrateClass::Podman,
        package_manager: PackageManager::Apk,
        entrypoint_script: WORKSPACE_PROBE_SCRIPT.into(),
        exitpoint_script: None,
        binaries: vec![],
        mcp_servers: vec![],
        tool_allowlist: ToolAllowlist(vec![]),
        permit_scope: PermitScope(vec![
            "fs:read:/workspace/**".into(),
            "fs:write:/workspace/**".into(),
        ]),
        passthru_env: vec![],
        extra_bootstrap: vec![],
        mounts: vec![],
        workspace_mount: Some(WorkspaceMount {
            container_path: "/workspace".into(),
            readonly: false,
        }),
        sccache: false,
    };
    let workspace_probe_team = TeamTopology {
        name: TeamName("workspace-probe-team".into()),
        version: 1,
        roles: vec![workspace_probe.name.clone()],
        message_graph: Vec::<MessageEdge>::new(),
        terminal_role: workspace_probe.name.clone(),
        max_retries: 0,
    };

    // ---- sccache-probe (for sccache-wiring regression tests) ----
    let sccache_probe = AgentRole {
        name: RoleName("sccache-probe".into()),
        version: 1,
        model: None,
        system_prompt: None,
        image: ALPINE.into(),
        substrate_class: SubstrateClass::Podman,
        package_manager: PackageManager::Apk,
        entrypoint_script: SCCACHE_PROBE_SCRIPT.into(),
        exitpoint_script: None,
        // Alpine ships rust/cargo in its community repo; sccache is added
        // automatically by `effective_binaries` when `sccache=true`.
        binaries: vec!["rust".into(), "cargo".into()],
        mcp_servers: vec![],
        tool_allowlist: ToolAllowlist(vec![]),
        permit_scope: PermitScope(vec!["net:allow:agentry-sccache-redis".into()]),
        passthru_env: vec![],
        extra_bootstrap: vec![],
        mounts: vec![],
        workspace_mount: None,
        sccache: true,
    };
    let sccache_probe_team = TeamTopology {
        name: TeamName("sccache-probe-team".into()),
        version: 1,
        roles: vec![sccache_probe.name.clone()],
        message_graph: Vec::<MessageEdge>::new(),
        terminal_role: sccache_probe.name.clone(),
        max_retries: 0,
    };

    // ---- timeout-probe (for wall-clock-timeout regression tests) ----
    let timeout_probe = AgentRole {
        name: RoleName("timeout-probe".into()),
        version: 1,
        model: None,
        system_prompt: None,
        image: ALPINE.into(),
        substrate_class: SubstrateClass::Podman,
        package_manager: PackageManager::Apk,
        entrypoint_script: TIMEOUT_PROBE_SCRIPT.into(),
        exitpoint_script: None,
        binaries: vec![],
        mcp_servers: vec![],
        tool_allowlist: ToolAllowlist(vec![]),
        permit_scope: PermitScope(vec!["net:deny:*".into()]),
        passthru_env: vec![],
        extra_bootstrap: vec![],
        mounts: vec![],
        workspace_mount: None,
        sccache: false,
    };
    let timeout_probe_team = TeamTopology {
        name: TeamName("timeout-probe-team".into()),
        version: 1,
        roles: vec![timeout_probe.name.clone()],
        message_graph: Vec::<MessageEdge>::new(),
        terminal_role: timeout_probe.name.clone(),
        max_retries: 0,
    };

    // ---- agentry-self-host-v0 team (cutoff trigger) ----
    // Coder clones, calls claude, runs acceptance, commits locally.
    // Reviewer re-runs acceptance in isolation on the coder's workspace.
    // Shipper pushes the branch and opens a PR on the forge.
    // Ci-watcher polls forge CI on the PR's head sha and merges on green.
    let home = std::env::var("HOME").unwrap_or_else(|_| "/var/home/yg".into());
    let coder_claude_agentry = AgentRole {
        name: RoleName("coder-claude-agentry".into()),
        version: 1,
        model: Some("claude-max".into()),
        system_prompt: None,
        // rust:1.93 already has cargo + rustc. Apt installs the git client
        // + ca-certificates; claude CLI is bind-mounted from the host.
        image: "docker.io/library/rust:1.93".into(),
        substrate_class: SubstrateClass::Podman,
        package_manager: PackageManager::Apt,
        entrypoint_script: format!("{BASH_PRELUDE}{CODER_CLAUDE_AGENTRY_SCRIPT}"),
        exitpoint_script: Some(format!("{BASH_PRELUDE}{CODER_CLAUDE_AGENTRY_EXITPOINT}")),
        binaries: vec!["git".into(), "curl".into(), "ca-certificates".into()],
        mcp_servers: vec![],
        tool_allowlist: ToolAllowlist(vec![]),
        permit_scope: PermitScope(vec![
            "fs:read:/workspace/**".into(),
            "fs:write:/workspace/**".into(),
            "net:allow:api.anthropic.com".into(),
            "net:allow:agency.lab".into(),
        ]),
        passthru_env: vec!["GITEA_TOKEN".into()],
        extra_bootstrap: vec![
            "rustup component add rustfmt clippy".into(),
            "git config --global http.sslVerify false".into(),
            "CARGO_NET_GIT_FETCH_WITH_CLI=true cargo install --git https://oauth2:${GITEA_TOKEN}@agency.lab:3000/yg/quality-architecture.git --bin quality-hygiene --root /usr/local --locked --quiet || true".into(),
        ],
        mounts: vec![
            Mount {
                source: format!("{home}/.local/bin/claude"),
                target: "/usr/local/bin/claude".into(),
                readonly: true,
            },
            Mount {
                source: format!("{home}/.claude/.credentials.json"),
                target: "/root/.claude/.credentials.json".into(),
                readonly: true,
            },
            Mount {
                source: claude_settings_path.clone(),
                target: "/root/.claude/settings.json".into(),
                readonly: true,
            },
        ],
        workspace_mount: Some(WorkspaceMount {
            container_path: "/workspace".into(),
            readonly: false,
        }),
        // sccache disabled for v0 — rust:1.93 is debian-based; sccache is not
        // in apt/bookworm. Issue #9/#10 or a follow-up brief will add a
        // static binary download here. Without sccache, each brief does a
        // cold compile of the target repo (~60–90s for agentry itself).
        sccache: false,
    };
    let reviewer_mechanical_agentry = AgentRole {
        name: RoleName("reviewer-mechanical-agentry".into()),
        version: 1,
        model: None,
        system_prompt: None,
        // Same toolchain as coder — deterministic re-run.
        image: "docker.io/library/rust:1.93".into(),
        substrate_class: SubstrateClass::Podman,
        package_manager: PackageManager::Apt,
        entrypoint_script: format!("{BASH_PRELUDE}{REVIEWER_MECHANICAL_AGENTRY_SCRIPT}"),
        exitpoint_script: None,
        binaries: vec!["git".into(), "ca-certificates".into()],
        mcp_servers: vec![],
        tool_allowlist: ToolAllowlist(vec![]),
        permit_scope: PermitScope(vec!["fs:read:/workspace/**".into()]),
        passthru_env: vec![],
        extra_bootstrap: vec!["rustup component add rustfmt clippy".into()],
        mounts: vec![],
        workspace_mount: Some(WorkspaceMount {
            container_path: "/workspace".into(),
            // Reviewer is mechanical and independent — it does not mutate
            // the workspace. CARGO_TARGET_DIR is redirected to /tmp inside
            // the container so a read-only mount is sufficient.
            readonly: true,
        }),
        sccache: false,
    };
    let reviewer_claude_agentry = AgentRole {
        name: RoleName("reviewer-claude-agentry".into()),
        version: 1,
        model: Some("claude-max".into()),
        system_prompt: None,
        image: "docker.io/library/debian:bookworm-slim".into(),
        substrate_class: SubstrateClass::Podman,
        package_manager: PackageManager::Apt,
        entrypoint_script: format!("{BASH_PRELUDE}{REVIEWER_CLAUDE_AGENTRY_SCRIPT}"),
        exitpoint_script: None,
        // git for diff; no rust toolchain — LLM reviewer does no compilation.
        binaries: vec!["git".into(), "ca-certificates".into()],
        mcp_servers: vec![],
        tool_allowlist: ToolAllowlist(vec![]),
        permit_scope: PermitScope(vec![
            "fs:read:/workspace/**".into(),
            "net:allow:api.anthropic.com".into(),
            "net:allow:agency.lab".into(), // for git fetch origin/<base_branch>
        ]),
        passthru_env: vec![],
        extra_bootstrap: vec![],
        mounts: vec![
            Mount {
                source: format!("{home}/.local/bin/claude"),
                target: "/usr/local/bin/claude".into(),
                readonly: true,
            },
            Mount {
                source: format!("{home}/.claude/.credentials.json"),
                target: "/root/.claude/.credentials.json".into(),
                readonly: true,
            },
            Mount {
                source: claude_settings_path.clone(),
                target: "/root/.claude/settings.json".into(),
                readonly: true,
            },
        ],
        // Read-only workspace — LLM reviewer does not mutate the coder's tree.
        workspace_mount: Some(WorkspaceMount {
            container_path: "/workspace".into(),
            readonly: true,
        }),
        sccache: false,
    };
    let shipper_agentry = AgentRole {
        name: RoleName("shipper-agentry".into()),
        version: 1,
        model: None,
        system_prompt: None,
        image: ALPINE.into(),
        substrate_class: SubstrateClass::Podman,
        package_manager: PackageManager::Apk,
        entrypoint_script: format!("{BASH_PRELUDE}{SHIPPER_AGENTRY_SCRIPT}"),
        exitpoint_script: None,
        binaries: vec!["git".into(), "curl".into(), "ca-certificates".into()],
        mcp_servers: vec![],
        tool_allowlist: ToolAllowlist(vec![]),
        permit_scope: PermitScope(vec![
            "net:allow:agency.lab".into(),
            "forge:write:yg/*".into(),
        ]),
        passthru_env: vec!["GITEA_TOKEN".into()],
        extra_bootstrap: vec![],
        mounts: vec![],
        // Shipper writes to /workspace/.git during `git push` (reflog,
        // FETCH_HEAD), so the workspace mount must be writable.
        workspace_mount: Some(WorkspaceMount {
            container_path: "/workspace".into(),
            readonly: false,
        }),
        sccache: false,
    };
    let ci_watcher_agentry = AgentRole {
        name: RoleName("ci-watcher-agentry".into()),
        version: 1,
        model: None,
        system_prompt: None,
        image: ALPINE.into(),
        substrate_class: SubstrateClass::Podman,
        package_manager: PackageManager::Apk,
        entrypoint_script: format!("{BASH_PRELUDE}{CI_WATCHER_AGENTRY_SCRIPT}"),
        exitpoint_script: None,
        binaries: vec!["curl".into(), "ca-certificates".into()],
        mcp_servers: vec![],
        tool_allowlist: ToolAllowlist(vec![]),
        permit_scope: PermitScope(vec![
            "net:allow:agency.lab".into(),
            "forge:write:yg/agentry".into(),
        ]),
        passthru_env: vec!["GITEA_TOKEN".into()],
        extra_bootstrap: vec![],
        mounts: vec![],
        // ci-watcher doesn't need the repo — all inputs come via the
        // shipper's routed Message payload. Skipping workspace_mount means
        // this role doesn't extend workspace lifetime beyond shipper.
        workspace_mount: None,
        sccache: false,
    };
    let agentry_self_host_v0 = TeamTopology {
        name: TeamName("agentry-self-host-v0".into()),
        version: 1,
        roles: vec![
            coder_claude_agentry.name.clone(),
            reviewer_mechanical_agentry.name.clone(),
            reviewer_claude_agentry.name.clone(),
            shipper_agentry.name.clone(),
            ci_watcher_agentry.name.clone(),
        ],
        // Rework loop enabled — max_retries=2 gives the coder two chances to
        // fix findings emitted by the reviewer before the team resolves Failed.
        message_graph: vec![
            // Both reviewers treat coder as their rework-target upstream.
            MessageEdge {
                from: coder_claude_agentry.name.clone(),
                to: reviewer_mechanical_agentry.name.clone(),
                permit_overrides_from: None,
            },
            MessageEdge {
                from: coder_claude_agentry.name.clone(),
                to: reviewer_claude_agentry.name.clone(),
                permit_overrides_from: None,
            },
            // Mechanical reviewer signals shipper only for sequential flow; no
            // data payload carried on this edge.
            MessageEdge {
                from: reviewer_mechanical_agentry.name.clone(),
                to: shipper_agentry.name.clone(),
                permit_overrides_from: None,
            },
            // Claude reviewer also signals shipper.
            MessageEdge {
                from: reviewer_claude_agentry.name.clone(),
                to: shipper_agentry.name.clone(),
                permit_overrides_from: None,
            },
            // Shipper routes head_sha + pr_number to ci-watcher via Message event.
            MessageEdge {
                from: shipper_agentry.name.clone(),
                to: ci_watcher_agentry.name.clone(),
                permit_overrides_from: None,
            },
        ],
        terminal_role: ci_watcher_agentry.name.clone(),
        max_retries: 2,
    };

    // ---- persist everything ----
    redis_io::save_role(&mut conn, &echo).await?;
    redis_io::save_team(&mut conn, &echo_team).await?;
    redis_io::save_role(&mut conn, &workspace_probe).await?;
    redis_io::save_team(&mut conn, &workspace_probe_team).await?;
    redis_io::save_role(&mut conn, &sccache_probe).await?;
    redis_io::save_team(&mut conn, &sccache_probe_team).await?;
    redis_io::save_role(&mut conn, &timeout_probe).await?;
    redis_io::save_team(&mut conn, &timeout_probe_team).await?;
    redis_io::save_role(&mut conn, &naughty).await?;
    redis_io::save_team(&mut conn, &naughty_team).await?;
    redis_io::save_role(&mut conn, &speaker).await?;
    redis_io::save_role(&mut conn, &listener).await?;
    redis_io::save_team(&mut conn, &speaker_listener_team).await?;
    redis_io::save_role(&mut conn, &grok_echo).await?;
    redis_io::save_team(&mut conn, &grok_team).await?;
    redis_io::save_role(&mut conn, &claude_echo).await?;
    redis_io::save_team(&mut conn, &claude_team).await?;
    redis_io::save_role(&mut conn, &synthesizer).await?;
    redis_io::save_role(&mut conn, &narrowed_coder).await?;
    redis_io::save_team(&mut conn, &narrowed_team).await?;
    redis_io::save_role(&mut conn, &shipper).await?;
    redis_io::save_team(&mut conn, &shipper_team).await?;
    redis_io::save_role(&mut conn, &coder_claude_agentry).await?;
    redis_io::save_role(&mut conn, &reviewer_mechanical_agentry).await?;
    redis_io::save_role(&mut conn, &reviewer_claude_agentry).await?;
    redis_io::save_role(&mut conn, &shipper_agentry).await?;
    redis_io::save_role(&mut conn, &ci_watcher_agentry).await?;
    redis_io::save_team(&mut conn, &agentry_self_host_v0).await?;

    tracing::info!(
        "seeded: roles [echo, workspace-probe, sccache-probe, timeout-probe, naughty, speaker, listener, grok-echo, claude-echo, synthesizer, narrowed-coder, shipper, coder-claude-agentry, reviewer-mechanical-agentry, shipper-agentry, ci-watcher-agentry, reviewer-claude-agentry] (inline entrypoint scripts); teams [echo, workspace-probe, sccache-probe, timeout-probe, naughty, speaker-listener, grok-echo, claude-echo, narrowed-team, shipper-solo-team, agentry-self-host-v0]"
    );
    Ok(())
}
