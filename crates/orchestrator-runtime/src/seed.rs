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
/// `emit_finding_model <severity> <agent-id> <category> <message> [prohibitions-json] [requirements-json]` (LLM origin),
/// and `emit_message <to> <payload-json>`.
const BASH_PRELUDE: &str = r#"export GIT_SSL_NO_VERIFY=true
export CARGO_NET_GIT_FETCH_WITH_CLI=true
CLAUDE_P_TIMEOUT="${CLAUDE_P_TIMEOUT:-1200}"
emit_event() {
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
            category:$cat, message:$msg, suggested_fix:null,
            prohibitions:[], requirements:[]
        }}'
}
emit_message() {
    # args: to (role name), payload (JSON)
    jq -nc --arg at "$(date -Iseconds)" --arg to "$1" --argjson payload "$2" \
        '{at:$at, type:"message", to:$to, payload:$payload}'
}
emit_finding_model() {
    # args: severity (blocker|warn), reviewer_agent_id, category, message,
    #       [prohibitions JSON array], [requirements JSON array]
    # Last two args default to "[]" when omitted, so legacy 4-arg call sites
    # keep working unchanged.
    local prohibitions="${5:-[]}"
    local requirements="${6:-[]}"
    jq -nc --arg at "$(date -Iseconds)" \
        --arg sev "$1" --arg aid "$2" --arg cat "$3" --arg msg "$4" \
        --argjson proh "$prohibitions" --argjson reqs "$requirements" \
        '{at:$at, type:"finding", finding:{
            severity:$sev,
            origin:{kind:"model", reviewer_agent_id:$aid},
            file:null, line:null,
            category:$cat, message:$msg, suggested_fix:null,
            prohibitions:$proh, requirements:$reqs
        }}'
}
# Stream `claude -p` to a transcript file under /transcripts/ AND emit each
# stream-json line as a structured trace event. Captures the assistant's
# final text into the named output variable for downstream parsing.
#
#   stream_claude <out_var> <suffix> <prompt>
#
# - <out_var>: bash variable name to receive the assistant's final text
# - <suffix>:  appended to ${brief_id} for the transcript filename, e.g.
#              "" / ".coder" / ".reviewer" — extension `.jsonl` is added
# - <prompt>:  the prompt string to pass to claude -p
#
# Caller MUST have set $brief_id before invoking. On `claude -p` failure
# the helper emits an error event + emit_done "failed" + `exit 0` (so the
# script terminates with the role marked failed). Because the pipeline is
# wrapped in `{ ... } || true`, `set -e` does not race the failure branch:
# `${PIPESTATUS[0]}` reflects `timeout`'s exit code (e.g. 124 on timeout).
stream_claude() {
    local _out_var="$1"
    local _suffix="$2"
    local _prompt="$3"
    mkdir -p /transcripts
    local _t="/transcripts/${brief_id}${_suffix}.jsonl"
    {
        HOME=/root timeout "$CLAUDE_P_TIMEOUT" claude -p \
            --output-format stream-json --verbose \
            "$_prompt" 2>&1 \
          | tee "$_t" \
          | while IFS= read -r _line; do
                if printf '%s' "$_line" | jq -e . >/dev/null 2>&1; then
                    emit_event "$(jq -nc --argjson c "$_line" '{claude:$c}')"
                else
                    emit_event "$(jq -nc --arg s "$_line" '{claude_raw:$s}')"
                fi
            done
    } || true
    local _ec=${PIPESTATUS[0]}
    if [ "$_ec" -ne 0 ]; then
        emit_event "$(jq -nc --arg ec "$_ec" '{error:"claude -p failed",exit_code:$ec}')"
        emit_done "failed"
        exit 0
    fi
    # Defence in depth: claude -p reported success but the transcript file
    # is missing or empty. The likely cause is a host /transcripts/ bind
    # mount the container UID can't write (default install is root:root,
    # rootless podman maps container-root to a non-zero host UID via
    # subuid). tee swallows the EACCES while the upstream pipeline still
    # exits 0. Surface as an explicit event so the operator sees the real
    # failure mode instead of a bare exit-2 elsewhere.
    if [ ! -s "$_t" ]; then
        emit_event "$(jq -nc --arg p "$_t" '{error:"tee_or_transcript_write_failed",transcript_path:$p}')"
        emit_done "failed"
        exit 0
    fi
    # Reconstruct the assistant's final text from the transcript so callers
    # that previously consumed `$reply` / `$response` keep working.
    # The transcript carries exactly one `result` event; piping its `.result`
    # through `tail -1` would silently drop multi-line JSON content (claude
    # pretty-prints arrays of findings) down to a single trailing `]`,
    # which then trips the reviewer's `grep -m1 '\['` + `set -e` chain.
    local _r
    _r=$(jq -r 'select(.type=="result") | .result' "$_t" 2>/dev/null)
    if [ -z "$_r" ]; then
        _r=$(jq -r 'select(.type=="assistant") | .message.content[]? | select(.type=="text") | .text' "$_t" 2>/dev/null)
    fi
    printf -v "$_out_var" '%s' "$_r"
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
brief_id="$(jq -r '.brief.id' <<<"$bundle")"
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

stream_claude reply "" "$prompt"

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

prior_findings=$(jq -c '
  [ .team_context.messages[]?.payload.findings[]?
    | select(.severity == "blocker")
    | { message, prohibitions: (.prohibitions // []), requirements: (.requirements // []) }
  ]
' <<<"$bundle")
finding_count=$(jq 'length' <<<"$prior_findings")

rework_banner=""
if [ "$finding_count" -gt 0 ]; then
    feedback_block=$(jq -r '.[] |
        "- BLOCKER: \(.message)\n  Prohibitions: \(.prohibitions | join("; "))\n  Requirements: \(.requirements | join("; "))"
    ' <<<"$prior_findings")
    rework_banner=$(cat <<REWORK_EOF
**This is a REWORK iteration.**

A prior coder pass on this brief shipped a commit that is already on HEAD of this worktree. The reviewer flagged the following BLOCKER findings against that commit. Read the existing diff with \`git diff \${base_branch}...HEAD\`, identify the sites the findings name, and edit those sites to satisfy each requirement and avoid each prohibition. Do NOT replan from scratch and do NOT recreate files that already exist.

--- Prior reviewer findings ---
${feedback_block}
--- End findings ---
REWORK_EOF
)
    emit_event "$(jq -nc --argjson n "$finding_count" '{msg:"rework iteration — injecting prior findings into prompt",blocker_count:$n}')"
fi

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
printf 'export issue_body=%q\n' "$issue_body"    >> /tmp/brief_vars.sh
printf 'export acceptance=%q\n' "$acceptance"    >> /tmp/brief_vars.sh
printf 'export branch=%q\n' "$branch"            >> /tmp/brief_vars.sh

cat > /tmp/prompt.txt <<PROMPT
You are the coder role inside the agentry autonomous team, operating in the
container-local working directory /workspace. The repo is cloned at
branch "$base_branch"; you are on a fresh branch "$branch".

Your task is described in verb-structured form below. Follow it literally:
each verb (CREATE / UPDATE / REPLACE / DELETE / MOVE) names a transformation
on a specific file:line target. Do NOT invent additional changes.

${rework_banner}

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

stream_claude reply ".coder" "$(cat /tmp/prompt.txt)"

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

# Self-review — LLM checks verb-completeness before commit.
# Skips silently if issue body has no verb syntax (legacy free-form briefs).
# On malformed claude output, warns and falls through to commit — this is
# a cheap pre-filter, not a hard gate; reviewer-claude is the backstop.
if printf '%s' "$issue_body" | grep -qE '^(### [0-9]+\. |CREATE |UPDATE |REPLACE |DELETE |MOVE )'; then
    emit_event '{"msg":"running self-review (verb completeness)"}'
    git diff --cached > /tmp/staged.patch
    cat > /tmp/self_rev.txt <<PROMPT
You are a self-review check for the agentry project. Below is the TASK
BODY (with explicit verbs — CREATE/UPDATE/REPLACE/DELETE/MOVE) and the
STAGED DIFF you are about to commit.

TASK BODY:
$(printf '%s' "$issue_body" | head -c 3000)

STAGED DIFF:
$(cat /tmp/staged.patch)

For each verb declared in the task body, check whether it has been applied
in the diff at the named location. Output EXACTLY a JSON object — no
markdown fences, no prose:

{
  "all_applied": true,
  "unapplied": []
}

If any verb is missing, set all_applied:false and list each missing verb
as a short description (max 200 chars each, max 6 entries).

Your response, right now, starting with { and ending with }:
PROMPT
    # Self-review tolerates failure: instead of `exit 0` (which `stream_claude`
    # does on hard failure), wrap so a transient claude error degrades to
    # "all applied" rather than killing the role. Skip stream_claude here
    # because we need the soft-fail; emit the call directly under the same
    # set -e + pipefail guard pattern.
    mkdir -p /transcripts
    SR_TRANSCRIPT="/transcripts/${brief_id}.self-review.jsonl"
    {
        HOME=/root timeout "$CLAUDE_P_TIMEOUT" claude -p \
            --output-format stream-json --verbose \
            "$(cat /tmp/self_rev.txt)" 2>&1 \
          | tee "$SR_TRANSCRIPT" \
          | while IFS= read -r _line; do
                if printf '%s' "$_line" | jq -e . >/dev/null 2>&1; then
                    emit_event "$(jq -nc --argjson c "$_line" '{claude:$c}')"
                else
                    emit_event "$(jq -nc --arg s "$_line" '{claude_raw:$s}')"
                fi
            done
    } || true
    sr_ec=${PIPESTATUS[0]}
    if [ "$sr_ec" -ne 0 ]; then
        emit_event "$(jq -nc --arg ec "$sr_ec" '{warn:"self-review claude call failed; proceeding",exit_code:$ec}')"
        sr='{"all_applied":true,"unapplied":[]}'
    else
        # Same regression class as PR #129: piping `.result` through `tail -1`
        # silently truncates pretty-printed multi-line JSON to a single trailing
        # `}`, which then trips the `grep -m1 '{'` + `set -e` chain below. The
        # result event is unique per transcript, so no `tail` is needed.
        sr=$(jq -r 'select(.type=="result") | .result' "$SR_TRANSCRIPT" 2>/dev/null)
        if [ -z "$sr" ]; then
            sr=$(jq -r 'select(.type=="assistant") | .message.content[]? | select(.type=="text") | .text' "$SR_TRANSCRIPT" 2>/dev/null)
        fi
        [ -z "$sr" ] && sr='{"all_applied":true,"unapplied":[]}'
    fi
    cleaned=$(printf '%s' "$sr" | sed -e 's/^```json$//' -e 's/^```$//' -e '/^$/d' | tr -d '\r')
    start=$(printf '%s' "$cleaned" | grep -b -m1 '{' | head -1 | cut -d: -f1)
    end=$(printf '%s' "$cleaned" | grep -bo '}' | tail -1 | cut -d: -f1)
    if [ -n "$start" ] && [ -n "$end" ]; then
        payload=$(printf '%s' "$cleaned" | tail -c +$((start+1)) | head -c $((end-start+1)))
        if printf '%s' "$payload" | jq -e 'type == "object"' >/dev/null 2>&1; then
            all_applied=$(printf '%s' "$payload" | jq -r '.all_applied // true')
            if [ "$all_applied" = "false" ]; then
                printf '%s' "$payload" | jq -r '.unapplied[]?' | while read -r item; do
                    emit_finding_model "blocker" "coder-self-review" "completeness" "unapplied verb: $item"
                done
                emit_event '{"error":"self-review found unapplied verbs"}'
                emit_done "failed"; exit 0
            fi
            emit_event '{"msg":"self-review: all verbs applied"}'
        else
            emit_event '{"warn":"self-review response not a JSON object; proceeding"}'
        fi
    else
        emit_event '{"warn":"self-review response missing JSON braces; proceeding"}'
    fi
fi

# Pre-commit dead-pub gate: invoke the dead-pub-check binary if the host
# has bind-mounted it. Binary reads {diff, workspace_root} JSON on stdin,
# emits findings as JSONL on stdout, exits 0. Falls through silently if
# the binary is missing (warn-skip mount handles that side). Brief 1 of
# #134 replaces the prior bash pipeline (PR #133, hot-fixed in #135) —
# the binary is structurally immune to the empty-grep × set -euo pipefail
# failure class that bit PRs #129/#130/#135.
if command -v dead-pub-check >/dev/null 2>&1; then
    emit_event '{"msg":"running dead-pub-check"}'
    diff_text=$(git diff --cached -U0)
    findings=$(jq -nc --arg d "$diff_text" --arg w "/workspace" '{diff:$d, workspace_root:$w}' \
        | dead-pub-check 2>/tmp/dpc.err) || {
            emit_event "$(jq -nc --arg err "$(tail -c 4096 /tmp/dpc.err)" '{warn:"dead-pub-check failed",detail:$err}')"
            findings=""
        }
    while IFS= read -r line; do
        [ -z "$line" ] && continue
        sev=$(jq -r '.severity' <<<"$line" 2>/dev/null || echo "warn")
        cat=$(jq -r '.category' <<<"$line" 2>/dev/null || echo "dead-pub")
        msg=$(jq -r '.message' <<<"$line" 2>/dev/null || echo "<malformed finding>")
        if [ "$sev" = "warn" ]; then
            emit_finding "warn" "ra-query" "$cat" "$msg"
        else
            emit_event "$(jq -nc --arg s "$sev" --arg c "$cat" --arg m "$msg" '{msg:"dead_pub_info",severity:$s,category:$c,detail:$m}')"
        fi
    done <<< "$findings"
else
    emit_event '{"msg":"dead_pub_check_unavailable","detail":"binary not on PATH; coder gate skipped"}'
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
diff_stat=$(git diff --stat "${base_branch}"...HEAD 2>&1 | tail -1 || true)
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
/// is present. Currently executed after the mechanical reviewer by the
/// sequential scheduler; message_graph is parallel-capable (issue #13
/// will enable parallel execution).
const REVIEWER_CLAUDE_AGENTRY_SCRIPT: &str = r####"#!/usr/bin/env bash
set -euo pipefail
bundle="$(cat)"

brief_id=$(jq -r '.brief.id' <<<"$bundle")
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
if ! git diff "${base_branch}...HEAD" > /tmp/diff.patch 2>/tmp/diff.err; then
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

# Mechanical pre-pass: when the host-built ra-query binary is bind-mounted
# in, walk the .rs files touched by the diff and aggregate ra-query findings
# into a panel. The reviewer prompt receives a short summary so the LLM has
# anchor points for unwraps + complexity hot-spots without having to
# re-derive them from raw source. Tolerate a missing binary — operators may
# not have run `just ra-query-binary` yet.
panel_summary=""
if command -v ra-query >/dev/null 2>&1; then
    changed_files=$(grep -E '^\+\+\+ b/.*\.rs$' /tmp/diff.patch | sed 's|^\+\+\+ b/||' || true)
    panel='[]'
    while IFS= read -r f; do
        [ -z "$f" ] && continue
        [ -f "/workspace/$f" ] || continue
        u=$(ra-query unwraps "/workspace/$f" --severity high --format json 2>/dev/null || echo '{"functions":[]}')
        c=$(ra-query complexity "/workspace/$f" --threshold 15 --format json 2>/dev/null || echo '{"functions":[]}')
        panel=$(jq --argjson u "$u" --argjson c "$c" --arg f "$f" '. + [{file:$f, unwraps:$u, complexity:$c}]' <<<"$panel")
    done <<< "$changed_files"
    panel_tail=$(jq -c . <<<"$panel" | tail -c 8192)
    emit_event "$(jq -nc --arg out "$panel_tail" '{msg:"ra_query_review_panel",findings_json_tail:$out}')"
    panel_summary=$(jq -r '[.[] | select(((.unwraps.summary.total // 0) + (.complexity.summary.total // 0)) > 0) | "\(.file): unwraps=\((.unwraps.summary.total // 0)) complexity_hot=\((.complexity.summary.total // 0))"] | join("\n")' <<<"$panel" 2>/dev/null | head -c 512)
else
    emit_event '{"msg":"ra_query_unavailable","detail":"skipping reviewer pre-pass"}'
fi

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
PROMPT

if [ -n "$panel_summary" ]; then
    cat >> /tmp/rev_prompt.txt <<EOM

--- Mechanical findings from ra-query (unwraps>=high, complexity>=15) ---
${panel_summary}
--- End mechanical findings ---
EOM
fi

cat >> /tmp/rev_prompt.txt <<PROMPT

Output EXACTLY a JSON array of findings — nothing else. No markdown fences,
no prose, no preamble, no explanation. Each element:

{
  "severity": "blocker" | "warn",
  "category": "design" | "naming" | "clarity" | "invariant" | "other",
  "message": "one-sentence human-readable description (max 200 chars)",
  "prohibitions": ["..."],   // for blockers, what the rework must NOT do
  "requirements": ["..."]    // for blockers, what the rework MUST do
}

Guidance:
- \`blocker\` = ships-broken, security-risk, invariant-violation, wrong abstraction.
- \`warn\` = minor cleanup, non-load-bearing style.
- If the diff is acceptable as-is, output exactly: []
- Maximum 6 findings. Prefer a single Blocker over many Warns.
- Do not comment on fmt/clippy/test — those are mechanical-reviewer scope.
- For each Blocker, populate \`prohibitions\` (things the next coder pass
  MUST NOT do) and \`requirements\` (things the next coder pass MUST do).
  These anchor the rework so the coder does not solve the wrong problem.
- For Warns, both arrays SHOULD be empty — a warn is informational, not
  a rework constraint.

Scope guardrail (CRITICAL):
- ONLY flag changes INSIDE the DIFF. Pre-existing inconsistencies in the
  repo that the diff did not touch are OUT of scope for blocker-level
  findings.
- If you notice a pre-existing concern adjacent to the diff, you MAY emit
  at most ONE warn-level finding with category "scope" describing it, so
  it is on-record for a follow-up brief — but NEVER emit a blocker for
  something the diff did not change.
- The unit of review is "did THIS diff ship broken/unsafe/wrong?", not
  "is the whole repo now consistent?".

Verb-completeness check (CRITICAL):
- The TASK BODY above may contain explicit verbs: CREATE, UPDATE, REPLACE,
  DELETE, MOVE — usually headed as "### N. <VERB> <file:line>" or the bare
  form "<VERB> <file:line>".
- For EACH such verb in the body, verify the diff contains the corresponding
  change at the named location (file path and approximate area).
- An unapplied verb is a blocker with category "invariant" and message
  "unapplied verb: <short description of what was missed>".
- If the body contains no verb syntax (legacy free-form brief), skip this
  check and apply only the design/naming/clarity/invariant guidance above.
- Applied-but-incomplete counts as unapplied (e.g. the verb asked to change
  three sites and only two were touched — the remaining one is unapplied).

Role-spec audit (CRITICAL):
- If the diff adds or modifies an \`AgentRole\` (i.e. introduces a \`RoleName(...)\` registration in seed.rs or changes the fields of an existing one), verify each of:
  (a) \`permit_scope\` is minimal for the stated job — no fs:write outside the workspace, no net access unless justified, no git tools on roles that do not ship code.
  (b) \`tool_allowlist\` matches what the role's entrypoint actually does (a read-only role must not be allowed to write arbitrary streams).
  (c) the deny-list is explicit for the categories of tool the role does not need.
  (d) any \`binaries\` or \`mcp_servers\` named are justified by the role's job.
- Mismatches are blockers with category "invariant" and a message starting with "role-spec audit:".
- This complements (does not replace) the scope-guardrail and verb-completeness checks above.

Bootstrap-command audit (CRITICAL):
- If the diff modifies any role's \`extra_bootstrap\` shell strings, verify each shell command:
  (a) \`cargo install --git URL --bin <name>\` is rejected when the target is a workspace with multiple binaries — must use positional package name (e.g. \`cfdb-cli\`, \`application\`) or \`--package\`.
  (b) Bootstrap commands that may transiently fail must end with \`|| true\` for fault tolerance, matching the existing \`reviewer-mechanical-agentry\` quality-hygiene install pattern.
  Mismatch is a blocker with category "invariant".

Daemon-lifecycle ordering (CRITICAL):
- If the diff modifies the daemon's \`handle_brief\` shipping flow (workspace teardown, chain-trigger, terminal-handler), verify the ORDER:
  chain-trigger MUST read \`next_brief_refs\` and submit children to Redis BEFORE workspace destruction.
  Reason: planner-emitted child JSONs live IN the workspace; destroyed-before-read = lost children.
  Wrong order is a blocker with category "invariant".

State-machine emission idempotency (CRITICAL):
- If the diff adds or modifies a state-machine compose/finalize function (DOL \`compose_meta_verdict\`, future composers in recursive sub-planning, etc.), verify exactly-once semantics:
  guard the emission with SETNX on a Redis marker key, OR a Redis transaction, OR an equivalent atomic check.
  Concurrent terminal handlers can re-enter; without the gate, duplicate verdicts will fire (observed in A7v3: 3× duplicate failed-verdicts for one meta-brief).
  Missing idempotency gate is a blocker with category "invariant".

Your response, right now, starting with [ and ending with ]:
PROMPT

stream_claude response ".reviewer" "$(cat /tmp/rev_prompt.txt)"

# Tolerate (and strip) leading/trailing fences if claude adds them despite
# the instruction — common drift pattern.
cleaned=$(printf '%s' "$response" | sed -e 's/^```json$//' -e 's/^```$//' -e '/^$/d' | tr -d '\r')
# Find first [ and last ] — slice.
start=$(printf '%s' "$cleaned" | grep -b -m1 '\[' | head -1 | cut -d: -f1)
end=$(printf '%s' "$cleaned" | grep -bo '\]' | tail -1 | cut -d: -f1)
if [ -z "$start" ] || [ -z "$end" ]; then
    emit_event "$(jq -nc --arg r "$(printf '%s' "$cleaned" | head -c 300)" '{error:"claude response missing JSON array brackets",head:$r}')"
    # Salvage: prose-only reply (no JSON brackets at all). Wrap as a single
    # format_deviation finding so the LLM's reasoning reaches the verdict trail.
    salvaged_msg=$(printf '%s' "$cleaned" | head -c 4096)
    payload=$(jq -nc --arg m "$salvaged_msg" '[{file:null,line:null,severity:"error",origin:{kind:"model"},category:"format_deviation",message:$m,suggested_fix:null,prohibitions:[],requirements:[]}]')
    emit_event "$(jq -nc --arg m "$salvaged_msg" '{msg:"reviewer prose-reply salvaged as format_deviation",bytes:($m|length)}')"
else
    payload=$(printf '%s' "$cleaned" | tail -c +$((start+1)) | head -c $((end-start+1)))

    if ! printf '%s' "$payload" | jq -e 'type == "array"' >/dev/null 2>&1; then
        emit_event "$(jq -nc --arg r "$(printf '%s' "$payload" | head -c 300)" '{error:"claude response not a JSON array",head:$r}')"
        salvaged_msg=$(printf '%s' "$payload" | head -c 4096)
        payload=$(jq -nc --arg m "$salvaged_msg" '[{file:null,line:null,severity:"error",origin:{kind:"model"},category:"format_deviation",message:$m,suggested_fix:null,prohibitions:[],requirements:[]}]')
        emit_event "$(jq -nc --arg m "$salvaged_msg" '{msg:"reviewer prose-reply salvaged as format_deviation",bytes:($m|length)}')"
    fi
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
    prohibitions=$(jq -c '[.prohibitions[]?]' <<<"$finding")
    requirements=$(jq -c '[.requirements[]?]' <<<"$finding")
    emit_finding_model "$severity" "$agent_id" "$category" "$message" "$prohibitions" "$requirements"
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
"####;

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
            # Merge with retry on transient 405/409. When N parallel children
            # all turn CI-green within the same second, the forge serialises
            # merges: only the first lands cleanly. The losers see 405
            # ("Please try again later" — rate-limit / mergeability still
            # recomputing) or 409 ("refusing to merge unrelated histories" —
            # ref moved under us). Both clear once the leader's merge lands;
            # retry with backoff + jitter unblocks the squad. Other codes
            # (401, 422, ...) are non-transient — fail fast to preserve
            # diagnostics.
            merge_body='{"Do":"merge"}'
            merge_max_retries=6
            merge_attempt=0
            merge_http_code=""
            merge_detail=""
            while [ "$merge_attempt" -lt "$merge_max_retries" ]; do
                merge_attempt=$((merge_attempt + 1))
                merge_http_code=$(curl -sS -k -X POST \
                    "https://${forge_host}/api/v1/repos/${owner}/${repo_name}/pulls/${pr_number}/merge" \
                    -H "Authorization: token ${GITEA_TOKEN}" \
                    -H "Content-Type: application/json" \
                    -d "$merge_body" \
                    -o /tmp/merge.body -w '%{http_code}')
                merge_detail=$(cat /tmp/merge.body 2>/dev/null || echo "")
                if [ "$merge_http_code" = "200" ] || [ "$merge_http_code" = "204" ]; then
                    emit_event "$(jq -nc --argjson n "$pr_number" --arg u "$pr_url" --argjson a "$merge_attempt" \
                        '{msg:"merged",pr_number:$n,pr_url:$u,merge_attempt:$a}')"
                    emit_done "shipped"; exit 0
                fi
                if [ "$merge_http_code" = "405" ] || [ "$merge_http_code" = "409" ]; then
                    if [ "$merge_attempt" -lt "$merge_max_retries" ]; then
                        merge_backoff=$((10 * merge_attempt))
                        if [ "$merge_backoff" -gt 60 ]; then
                            merge_backoff=60
                        fi
                        merge_jitter=$((RANDOM % 10))
                        merge_sleep=$((merge_backoff + merge_jitter))
                        emit_event "$(jq -nc --arg code "$merge_http_code" --arg d "$merge_detail" --argjson a "$merge_attempt" --argjson s "$merge_sleep" \
                            '{msg:"merge transient failure — retrying",http_code:$code,detail:$d,merge_attempt:$a,sleep_seconds:$s}')"
                        sleep "$merge_sleep"
                        continue
                    fi
                    # transient but budget exhausted — falls through to post-loop branch
                    break
                fi
                # Non-transient: fail immediately, preserves diagnostics.
                emit_event "$(jq -nc --arg code "$merge_http_code" --arg d "$merge_detail" --argjson a "$merge_attempt" \
                    '{error:"merge API call failed (non-transient)",http_code:$code,detail:$d,merge_attempt:$a}')"
                emit_done "failed"; exit 0
            done
            emit_event "$(jq -nc --arg code "$merge_http_code" --arg d "$merge_detail" --argjson a "$merge_attempt" --argjson m "$merge_max_retries" \
                '{error:"merge retry budget exhausted (transient)",http_code:$code,detail:$d,merge_attempt:$a,merge_max_retries:$m}')"
            emit_done "failed"; exit 0
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

/// Null role for `agentry-null-v0` team. Emits one event then `done shipped`
/// and exits. Zero work — used as a shake-down for the role-introduction
/// pipeline (reviewer-claude `Role-spec audit` clause; permit broker; spawner
/// teardown). NOT a probe role's substrate-style probe; deliberately the
/// simplest possible AgentRole.
const NULL_AGENT_AGENTRY_SCRIPT: &str = r#"#!/usr/bin/env bash
set -euo pipefail
emit_event '{"msg":"null-agent shake-down","status":"ok"}'
emit_done "shipped"
"#;

/// Archaeologist role for the `agentry-discovery-v0` team. First stage of the
/// upcoming `agentry-planner-v0` pipeline (#49). Runs `cfdb extract` and
/// `graph-specs check --json`, optionally evaluates seed cypher queries, then
/// asks `claude -p` to synthesize a structured `discovery.json` consumed by
/// the planner. Mechanical-plus-narrative: cfdb + graph-specs are factual,
/// claude produces summary + candidate list.
const ARCHAEOLOGIST_CLAUDE_AGENTRY_SCRIPT: &str = r##"#!/usr/bin/env bash
set -euo pipefail
bundle="$(cat)"

brief_id=$(jq -r '.brief.id' <<<"$bundle")
intent=$(jq -r '.brief.payload.intent // ""' <<<"$bundle")
success_criteria=$(jq -r '.brief.payload.success_criteria // ""' <<<"$bundle")
discovery_seeds=$(jq -c '.brief.payload.discovery_seeds // []' <<<"$bundle")

if [ ! -d /workspace/.git ] && [ ! -f /workspace/.git ]; then
    emit_event '{"error":"workspace missing — no .git found at /workspace"}'
    emit_done "failed"; exit 0
fi

cd /workspace
export HOME=/root

emit_event '{"msg":"running cfdb extract"}'
if ! cfdb extract --workspace . --db .cfdb/db-discovery --keyspace agentry > /tmp/cfdb-extract.log 2>&1; then
    err=$(tail -30 /tmp/cfdb-extract.log)
    emit_event "$(jq -nc --arg err "$err" '{error:"cfdb extract failed",detail:$err}')"
    emit_done "failed"; exit 0
fi

# Pull node/edge counts from the canonical "extract: N nodes, M edges" log line.
counts_line=$(grep -E 'extract: [0-9]+ nodes' /tmp/cfdb-extract.log | tail -1 || true)
nodes=$(printf '%s' "$counts_line" | sed -nE 's/.*extract: ([0-9]+) nodes.*/\1/p')
edges=$(printf '%s' "$counts_line" | sed -nE 's/.*nodes, ([0-9]+) edges.*/\1/p')
nodes=${nodes:-0}
edges=${edges:-0}
emit_event "$(jq -nc --argjson n "$nodes" --argjson e "$edges" '{msg:"cfdb extract done",nodes:$n,edges:$e}')"

# graph-specs check is intentionally non-fatal: violations are signal for the
# discovery, not a stop. Capture stdout+stderr.
graph_specs_out=$(graph-specs check --specs specs/concepts/ --code crates/ --json 2>&1 || true)
emit_event "$(jq -nc --arg head "$(printf '%s' "$graph_specs_out" | head -c 500)" '{msg:"graph-specs done",head:$head}')"

# Optional seed cypher queries against the just-built db.
seed_results='[]'
seed_count=$(jq 'length' <<<"$discovery_seeds")
if [ "$seed_count" -gt 0 ]; then
    i=0
    while [ "$i" -lt "$seed_count" ]; do
        q=$(jq -r ".[$i]" <<<"$discovery_seeds")
        rows=$(cfdb query --db .cfdb/db-discovery --keyspace agentry "$q" 2>/tmp/cfdb-q.err || echo '[]')
        if ! printf '%s' "$rows" | jq empty 2>/dev/null; then
            rows='[]'
        fi
        seed_results=$(jq -nc --argjson cur "$seed_results" --arg q "$q" --argjson r "$rows" \
            '$cur + [{query:$q, rows:$r}]')
        i=$((i+1))
    done
fi

cat > /tmp/arch_prompt.txt <<PROMPT
You are the archaeologist role for the agentry project. Synthesize a
discovery.json for downstream planner consumption based on the inputs below.

INTENT:
$intent

SUCCESS CRITERIA:
$success_criteria

CFDB EXTRACT SUMMARY:
nodes=$nodes, edges=$edges

GRAPH-SPECS OUTPUT (first 4000 chars):
$(printf '%s' "$graph_specs_out" | head -c 4000)

SEED-QUERY RESULTS (JSON):
$seed_results

Output EXACTLY one JSON object — no markdown fences, no prose. Schema:

{
  "intent": "<copied verbatim from INTENT above>",
  "summary": "<1-3 sentence narrative about workspace state relative to intent>",
  "raw_facts": {
    "cfdb": {"nodes": $nodes, "edges": $edges},
    "graph_specs_violations": [<pass-through of any violations parsed from GRAPH-SPECS OUTPUT, or []>],
    "seed_queries": $seed_results
  },
  "candidates": [
    {"target": "<qname or file:line>", "kind": "<reuse|extend|create|fix>", "rationale": "<short>"}
  ],
  "success_criteria": "<copied verbatim from SUCCESS CRITERIA above, or empty string>"
}

Your response, right now, starting with { and ending with }:
PROMPT

emit_event "$(jq -nc --arg len "$(wc -c < /tmp/arch_prompt.txt)" '{msg:"calling claude -p",prompt_bytes:$len}')"

stream_claude response ".archaeologist" "$(cat /tmp/arch_prompt.txt)"

# Same fence-stripping + brace-slice pattern as REVIEWER_CLAUDE_AGENTRY_SCRIPT,
# but for an object ({...}) instead of an array ([...]).
cleaned=$(printf '%s' "$response" | sed -e 's/^```json$//' -e 's/^```$//' -e '/^$/d' | tr -d '\r')
start=$(printf '%s' "$cleaned" | grep -b -m1 '{' | head -1 | cut -d: -f1)
end=$(printf '%s' "$cleaned" | grep -bo '}' | tail -1 | cut -d: -f1)
if [ -z "$start" ] || [ -z "$end" ]; then
    emit_event "$(jq -nc --arg r "$(printf '%s' "$cleaned" | head -c 300)" '{error:"claude response missing JSON object braces",head:$r}')"
    emit_done "failed"; exit 0
fi
payload=$(printf '%s' "$cleaned" | tail -c +$((start+1)) | head -c $((end-start+1)))

if ! printf '%s' "$payload" | jq -e 'type == "object"' >/dev/null 2>&1; then
    emit_event "$(jq -nc --arg r "$(printf '%s' "$payload" | head -c 300)" '{error:"claude response not a JSON object",head:$r}')"
    emit_done "failed"; exit 0
fi
if ! printf '%s' "$payload" | jq empty 2>/dev/null; then
    emit_event "$(jq -nc --arg r "$(printf '%s' "$payload" | head -c 300)" '{error:"claude response invalid JSON",head:$r}')"
    emit_done "failed"; exit 0
fi

printf '%s' "$payload" > /workspace/discovery.json
bytes=$(wc -c < /workspace/discovery.json)
emit_event "$(jq -nc --arg path "/workspace/discovery.json" --argjson bytes "$bytes" '{msg:"discovery.json written",path:$path,bytes:$bytes}')"
emit_done "shipped"
"##;

/// Planner role for the `agentry-planner-v0` team. Reads the
/// `discovery.json` synthesized by the upstream archaeologist, asks
/// `claude -p` to decompose the meta-brief intent into a JSON ARRAY of
/// child-brief descriptors, materializes each as a Brief JSON file under
/// `/workspace/planner-children/`, then emits a single outbox `Message`
/// whose payload carries `next_brief_refs` — a list of ABSOLUTE host paths
/// to those child files. The daemon's chain-trigger (extended to scan
/// accumulated role-outbox messages) auto-dispatches each child via
/// `submit_brief` once this brief ships. Planner never calls the
/// orchestrator CLI directly and never touches the forge.
const PLANNER_CLAUDE_AGENTRY_SCRIPT: &str = r##"#!/usr/bin/env bash
set -euo pipefail
bundle="$(cat)"

brief_id=$(jq -r '.brief.id' <<<"$bundle")
intent=$(jq -r '.brief.payload.intent // ""' <<<"$bundle")
success_criteria=$(jq -r '.brief.payload.success_criteria // ""' <<<"$bundle")
child_topology=$(jq -r '.brief.payload.child_topology // "agentry-self-host-v0"' <<<"$bundle")
max_children=$(jq -r '.brief.payload.max_children // 10' <<<"$bundle")
base_branch=$(jq -r '.brief.payload.base_branch // "develop"' <<<"$bundle")
target_repo=$(jq -r '.brief.payload.target_repo // "yg/agentry"' <<<"$bundle")

# Children's brief JSON files live on the shared workspace under the brief's
# host directory. The daemon's chain-trigger reads them off-container, so the
# paths emitted in next_brief_refs MUST be absolute host paths.
host_workspace="/var/mnt/workspaces/agentry-work/briefs/${brief_id}"

if [ ! -f /workspace/discovery.json ]; then
    emit_event '{"error":"discovery.json missing — upstream archaeologist must produce it at /workspace/discovery.json"}'
    emit_done "failed"; exit 0
fi

mkdir -p /workspace/planner-children

# Bound the inline discovery slice in the prompt to ~50KB.
discovery_size=$(wc -c < /workspace/discovery.json)
if [ "$discovery_size" -gt 51200 ]; then
    discovery_excerpt=$(head -c 51200 /workspace/discovery.json)
    discovery_truncated="true"
else
    discovery_excerpt=$(cat /workspace/discovery.json)
    discovery_truncated="false"
fi

cat > /tmp/planner_prompt.txt <<PROMPT
You are the planner role for the agentry project. Decompose the META-BRIEF
intent into a JSON ARRAY of child briefs. Each child must be a focused,
verifiable transformation expressed as verbs (CREATE/UPDATE/REPLACE/DELETE/MOVE)
on specific file:line targets — NOT freeform "fix this issue" prose.

META-BRIEF INTENT:
$intent

SUCCESS CRITERIA:
$success_criteria

DISCOVERY (size=${discovery_size} bytes, truncated=${discovery_truncated}):
$discovery_excerpt

CHILD BOILERPLATE (apply to every element):
- target_repo: $target_repo
- base_branch: $base_branch
- budget.max_wall_seconds: 900
- escalation: autonomous

TOPOLOGY SELECTION — pick per child by task signature:
- agentry-spec-edit-v0  → specs/* or docs/* changes only, no Rust code touched
- agentry-bugfix-v0     → sub-30-LOC bug fix in Rust, no new types/traits, no spec change
- agentry-self-host-v0  → everything else (default; new features, schema changes, multi-file refactors)

Output EXACTLY one JSON array — no markdown fences, no prose. Cap at
$max_children elements. Schema per element:

{
  "title": "<short verb-payload title>",
  "topology": "agentry-self-host-v0" | "agentry-bugfix-v0" | "agentry-spec-edit-v0",
  "verbs": "<full verb-payload markdown using CREATE/UPDATE/REPLACE/DELETE/MOVE>",
  "acceptance": "<bash command, e.g. cargo fmt --check && cargo clippy --workspace --all-targets -- -D warnings && cargo test --workspace>",
  "estimated_files": ["<crate>:<file>"]
}

Your response, right now, starting with [ and ending with ]:
PROMPT

emit_event "$(jq -nc --arg len "$(wc -c < /tmp/planner_prompt.txt)" '{msg:"calling claude -p",prompt_bytes:$len}')"

stream_claude response ".planner" "$(cat /tmp/planner_prompt.txt)"

# Same fence-stripping + bracket-slice pattern as REVIEWER_CLAUDE_AGENTRY_SCRIPT.
cleaned=$(printf '%s' "$response" | sed -e 's/^```json$//' -e 's/^```$//' -e '/^$/d' | tr -d '\r')
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
if [ "$count" -gt "$max_children" ]; then
    payload=$(printf '%s' "$payload" | jq --argjson n "$max_children" '.[:$n]')
    count=$max_children
fi

submitted_at=$(date -Iseconds)
host_paths='[]'
i=0
while [ "$i" -lt "$count" ]; do
    elem=$(printf '%s' "$payload" | jq -c ".[$i]")
    title=$(printf '%s' "$elem" | jq -r '.title // ""')
    verbs=$(printf '%s' "$elem" | jq -r '.verbs // ""')
    acceptance=$(printf '%s' "$elem" | jq -r '.acceptance // ""')
    elem_topology=$(printf '%s' "$elem" | jq -r '.topology // empty')
    if [ -z "$elem_topology" ] || [ "$elem_topology" = "null" ]; then
        elem_topology="$child_topology"
    fi

    pr_title="auto(planner-${brief_id}): ${title}"
    pr_body="Authored by planner-claude-agentry from meta-brief ${brief_id}. Verbs:

${verbs}"

    child_path="/workspace/planner-children/child-${i}.json"
    jq -nc \
        --arg id "brf_planner_${brief_id}_child_${i}" \
        --arg topology "$elem_topology" \
        --arg title "$title" \
        --arg verbs "$verbs" \
        --arg acceptance "$acceptance" \
        --arg target_repo "$target_repo" \
        --arg base_branch "$base_branch" \
        --arg pr_title "$pr_title" \
        --arg pr_body "$pr_body" \
        --arg parent "$brief_id" \
        --arg submitted_by "planner-claude-agentry-${brief_id}" \
        --arg submitted_at "$submitted_at" \
        '{
            id: $id,
            project: null,
            topology: { name: $topology, version: 1 },
            payload: {
                issue_number: 0,
                issue_title: $title,
                issue_body: $verbs,
                acceptance: $acceptance,
                target_repo: $target_repo,
                base_branch: $base_branch,
                pr_title: $pr_title,
                pr_body: $pr_body
            },
            budget: { max_wall_seconds: 900 },
            escalation: "autonomous",
            parent_brief: $parent,
            submitted_by: $submitted_by,
            submitted_at: $submitted_at
        }' > "$child_path"

    host_paths=$(jq -nc --argjson cur "$host_paths" \
        --arg p "${host_workspace}/planner-children/child-${i}.json" \
        '$cur + [$p]')
    i=$((i+1))
done

# Sentinel target `_chain_trigger`: there is no role by that name on the
# planner topology — the daemon's chain-trigger scans every accumulated
# outbox payload for next_brief_refs regardless of `to`, so this Message
# carries the dispatch list without targeting any sibling role.
emit_message "_chain_trigger" "$(jq -nc --argjson refs "$host_paths" '{next_brief_refs:$refs}')"
emit_event "$(jq -nc --argjson n "$count" --arg m "/workspace/planner-children/" '{msg:"planner produced N children",count:$n,manifest:$m}')"
emit_done "shipped"
"##;

/// Build the planner-claude-agentry role. Extracted from `seed_m0` so the
/// permit-scope invariants can be asserted in a unit test without rebuilding
/// the entire seed flow.
fn build_planner_claude_agentry_role(home: &str, claude_settings_path: &str) -> AgentRole {
    AgentRole {
        name: RoleName("planner-claude-agentry".into()),
        version: 1,
        model: Some("claude-max".into()),
        system_prompt: None,
        // Same toolchain as archaeologist for a uniform claude-mounted base,
        // but planner installs nothing and runs no compilation.
        image: "docker.io/library/rust:1.93".into(),
        substrate_class: SubstrateClass::Podman,
        package_manager: PackageManager::Apt,
        entrypoint_script: format!("{BASH_PRELUDE}{PLANNER_CLAUDE_AGENTRY_SCRIPT}"),
        exitpoint_script: None,
        binaries: vec![],
        mcp_servers: vec![],
        tool_allowlist: ToolAllowlist(vec![]),
        // Strictly tighter than archaeologist: no agency.lab (no cargo install),
        // no agentry-sccache-redis (no compilation).
        permit_scope: PermitScope(vec![
            "fs:read:/workspace/**".into(),
            "fs:write:/workspace/**".into(),
            "net:allow:api.anthropic.com".into(),
        ]),
        // No GITEA_TOKEN — planner does not touch the forge.
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
                source: claude_settings_path.into(),
                target: "/root/.claude/settings.json".into(),
                readonly: true,
            },
            Mount {
                source: "/var/lib/agentry/transcripts".into(),
                target: "/transcripts".into(),
                readonly: false,
            },
        ],
        workspace_mount: Some(WorkspaceMount {
            container_path: "/workspace".into(),
            readonly: false,
        }),
        sccache: false,
    }
}

/// Build the archaeologist-claude-agentry role. Extracted from `seed_m0` so
/// the permit-scope + tool-allowlist invariants can be asserted in a unit test
/// without rebuilding the entire seed flow.
fn build_archaeologist_claude_agentry_role(home: &str, claude_settings_path: &str) -> AgentRole {
    AgentRole {
        name: RoleName("archaeologist-claude-agentry".into()),
        version: 1,
        model: Some("claude-max".into()),
        system_prompt: None,
        // Same toolchain as coder-claude-agentry — needed for the cfdb +
        // graph-specs `cargo install` in extra_bootstrap.
        image: "docker.io/library/rust:1.93".into(),
        substrate_class: SubstrateClass::Podman,
        package_manager: PackageManager::Apt,
        entrypoint_script: format!("{BASH_PRELUDE}{ARCHAEOLOGIST_CLAUDE_AGENTRY_SCRIPT}"),
        exitpoint_script: None,
        // cfdb + graph-specs come via extra_bootstrap cargo install (same
        // pattern as quality-hygiene in coder-claude-agentry).
        binaries: vec![],
        mcp_servers: vec![],
        tool_allowlist: ToolAllowlist(vec![]),
        permit_scope: PermitScope(vec![
            "fs:read:/workspace/**".into(),
            "fs:write:/workspace/**".into(),
            "net:allow:api.anthropic.com".into(),
            "net:allow:agency.lab".into(),
            "net:allow:agentry-sccache-redis".into(),
        ]),
        passthru_env: vec!["GITEA_TOKEN".into()],
        // cfdb rev `02c5a45` and graph-specs-rust rev `ecaedb9` mirror the
        // pinned revs in the workspace's `.cfdb/cfdb.rev` and
        // `.cfdb/graph-specs.rev` files used by `scripts/arch-check.sh`.
        // A future brief can wire them through dynamically.
        extra_bootstrap: vec![
            "rustup component add rustfmt clippy".into(),
            "git config --global http.sslVerify false".into(),
            "CARGO_NET_GIT_FETCH_WITH_CLI=true cargo install --git https://oauth2:${GITEA_TOKEN}@agency.lab:3000/yg/cfdb.git --rev 02c5a45 --root /usr/local --locked --quiet cfdb-cli || true".into(),
            "CARGO_NET_GIT_FETCH_WITH_CLI=true cargo install --git https://oauth2:${GITEA_TOKEN}@agency.lab:3000/yg/graph-specs-rust.git --rev ecaedb9 --root /usr/local --locked --quiet application || true".into(),
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
                source: claude_settings_path.into(),
                target: "/root/.claude/settings.json".into(),
                readonly: true,
            },
            Mount {
                source: "/var/lib/agentry/transcripts".into(),
                target: "/transcripts".into(),
                readonly: false,
            },
        ],
        workspace_mount: Some(WorkspaceMount {
            container_path: "/workspace".into(),
            readonly: false,
        }),
        // Real cargo compilation in extra_bootstrap — share build cache with
        // coder/reviewer roles via the shared sccache-redis container.
        sccache: true,
    }
}

/// Build the reviewer-claude-agentry role. Extracted from `seed_m0` so the
/// bind-mounts (claude, credentials, settings, transcripts, ra-query) can be
/// asserted in unit tests. ra-query is operator-installed via
/// `just ra-query-binary` and bind-mounted at /usr/local/bin/ra-query; the
/// entrypoint script's pre-pass tolerates a missing binary.
fn build_reviewer_claude_agentry_role(home: &str, claude_settings_path: &str) -> AgentRole {
    AgentRole {
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
                source: claude_settings_path.into(),
                target: "/root/.claude/settings.json".into(),
                readonly: true,
            },
            Mount {
                source: "/var/lib/agentry/transcripts".into(),
                target: "/transcripts".into(),
                readonly: false,
            },
            Mount {
                source: format!("{home}/.local/bin/ra-query"),
                target: "/usr/local/bin/ra-query".into(),
                readonly: true,
            },
        ],
        // Read-only workspace — LLM reviewer does not mutate the coder's tree.
        workspace_mount: Some(WorkspaceMount {
            container_path: "/workspace".into(),
            readonly: true,
        }),
        sccache: false,
    }
}

/// Build the ac-verifier-claude-agentry role. Slotted between the coder and
/// the reviewer pair in `agentry-self-host-v0`. Reads the brief's
/// `acceptance_criteria` (Vec<String>) + the coder's git diff, asks claude
/// for a per-AC verdict, and emits one blocker `Finding` per failed AC.
/// Degrades to `done shipped` whenever AC list is missing/empty, the
/// ac-verifier binary is not on PATH, or claude returns invalid JSON —
/// reviewer-claude is the architectural backstop.
fn build_ac_verifier_claude_agentry_role(home: &str, claude_settings_path: &str) -> AgentRole {
    AgentRole {
        name: RoleName("ac-verifier-claude-agentry".into()),
        version: 1,
        model: Some("claude-max".into()),
        system_prompt: None,
        // No rust toolchain — the binary is bind-mounted from the host.
        image: "docker.io/library/debian:bookworm-slim".into(),
        substrate_class: SubstrateClass::Podman,
        package_manager: PackageManager::Apt,
        entrypoint_script: format!("{BASH_PRELUDE}{AC_VERIFIER_CLAUDE_AGENTRY_SCRIPT}"),
        exitpoint_script: None,
        binaries: vec!["git".into(), "ca-certificates".into()],
        mcp_servers: vec![],
        tool_allowlist: ToolAllowlist(vec![]),
        permit_scope: PermitScope(vec![
            "fs:read:/workspace/**".into(),
            "net:allow:api.anthropic.com".into(),
            "net:allow:agency.lab".into(),
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
                source: claude_settings_path.into(),
                target: "/root/.claude/settings.json".into(),
                readonly: true,
            },
            Mount {
                source: "/var/lib/agentry/transcripts".into(),
                target: "/transcripts".into(),
                readonly: false,
            },
            Mount {
                source: format!("{home}/.local/bin/ac-verifier"),
                target: "/usr/local/bin/ac-verifier".into(),
                readonly: true,
            },
        ],
        workspace_mount: Some(WorkspaceMount {
            container_path: "/workspace".into(),
            readonly: true,
        }),
        sccache: false,
    }
}

/// Build the coder-claude-agentry role. Extracted from `seed_m0` so the
/// bind-mounts (claude, credentials, settings, transcripts, ra-query) can be
/// asserted in unit tests. ra-query is operator-installed via
/// `just ra-query-binary` and bind-mounted at /usr/local/bin/ra-query; the
/// exitpoint's pre-commit dead-pub gate tolerates a missing binary.
fn build_coder_claude_agentry_role(home: &str, claude_settings_path: &str) -> AgentRole {
    AgentRole {
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
            "CARGO_NET_GIT_FETCH_WITH_CLI=true cargo install --git https://oauth2:${GITEA_TOKEN}@agency.lab:3000/yg/cfdb.git --rev 02c5a45 --root /usr/local --locked --quiet cfdb-cli || true".into(),
            "CARGO_NET_GIT_FETCH_WITH_CLI=true cargo install --git https://oauth2:${GITEA_TOKEN}@agency.lab:3000/yg/graph-specs-rust.git --rev ecaedb9 --root /usr/local --locked --quiet application || true".into(),
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
                source: claude_settings_path.into(),
                target: "/root/.claude/settings.json".into(),
                readonly: true,
            },
            Mount {
                source: "/var/lib/agentry/transcripts".into(),
                target: "/transcripts".into(),
                readonly: false,
            },
            Mount {
                source: format!("{home}/.local/bin/ra-query"),
                target: "/usr/local/bin/ra-query".into(),
                readonly: true,
            },
            Mount {
                source: format!("{home}/.local/bin/dead-pub-check"),
                target: "/usr/local/bin/dead-pub-check".into(),
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
    }
}

/// Verifier role for the `agentry-verify-v0` team. The DOL composer
/// (daemon-side, see `daemon.rs::on_all_children_resolved`) auto-dispatches a
/// verifier brief whenever a meta-brief's children all reach terminal verdict
/// AND the meta-brief carried a `success_criteria`. The verifier runs the
/// criterion as a shell command on a read-only snapshot of the workspace and
/// emits `done shipped` / `done failed`. The daemon composes that with the
/// children's verdicts to produce the meta-brief's terminal verdict.
const VERIFIER_CLAUDE_AGENTRY_SCRIPT: &str = r##"#!/usr/bin/env bash
set -uo pipefail
bundle="$(cat)"
criterion=$(jq -r '.brief.payload.success_criteria // ""' <<<"$bundle")
verifies=$(jq -r '.brief.payload.verifies_brief_id // ""' <<<"$bundle")

if [ -z "$criterion" ]; then
    emit_event '{"error":"verifier missing success_criteria in payload"}'
    emit_done "failed"; exit 0
fi

cd /workspace
emit_event "$(jq -nc --arg c "$criterion" --arg v "$verifies" '{msg:"running success_criteria",criterion:$c,verifies:$v}')"

if bash -c "$criterion" > /tmp/criterion.out 2>&1; then
    out=$(tail -c 4096 /tmp/criterion.out)
    emit_event "$(jq -nc --arg o "$out" '{msg:"criterion passed",output:$o}')"
    emit_done "shipped"
else
    rc=$?
    out=$(tail -c 4096 /tmp/criterion.out)
    emit_event "$(jq -nc --arg o "$out" --argjson rc "$rc" '{msg:"criterion failed",exit_code:$rc,output:$o}')"
    emit_done "failed"
fi
"##;

/// Entrypoint for `ac-verifier-claude-agentry`. Reads
/// `brief.payload.acceptance_criteria` (Vec<String>), captures the coder's
/// diff against `base_branch`, builds the binary's stdin JSON, and runs
/// `timeout $CLAUDE_P_TIMEOUT ac-verifier`. Degrades to `done shipped` when
/// AC list is empty/missing or the binary is not on PATH — reviewer-claude
/// is the architectural backstop.
const AC_VERIFIER_CLAUDE_AGENTRY_SCRIPT: &str = r##"#!/usr/bin/env bash
set -uo pipefail
bundle="$(cat)"

agent_id=$(jq -r '.permit.agent_id' <<<"$bundle")
base_branch=$(jq -r '.brief.payload.base_branch // "develop"' <<<"$bundle")
verb_body=$(jq -r '.brief.payload.issue_body // ""' <<<"$bundle")
acs_json=$(jq -c '.brief.payload.acceptance_criteria // null' <<<"$bundle")

# Fast path: no acceptance_criteria carried on this brief — short-circuit
# without spending any claude tokens. Reviewer-claude still runs.
if [ "$acs_json" = "null" ] || [ "$(jq 'length' <<<"$acs_json")" -eq 0 ]; then
    emit_event '{"msg":"no acceptance_criteria in payload — skipping ac-verifier"}'
    emit_done "shipped"; exit 0
fi

if [ ! -d /workspace/.git ] && [ ! -f /workspace/.git ]; then
    emit_event '{"error":"workspace is not a git repo — coder did not produce it"}'
    emit_done "shipped"; exit 0
fi

cd /workspace
if ! git fetch origin "$base_branch" >/tmp/acv_fetch.err 2>&1; then
    err=$(tail -20 /tmp/acv_fetch.err)
    emit_event "$(jq -nc --arg err "$err" '{msg:"git fetch failed — degrading to shipped",detail:$err}')"
    emit_done "shipped"; exit 0
fi
if ! git diff "origin/${base_branch}..HEAD" > /tmp/acv_diff.patch 2>/tmp/acv_diff.err; then
    err=$(tail -20 /tmp/acv_diff.err)
    emit_event "$(jq -nc --arg err "$err" '{msg:"git diff failed — degrading to shipped",detail:$err}')"
    emit_done "shipped"; exit 0
fi
diff_text=$(cat /tmp/acv_diff.patch)

if ! command -v ac-verifier >/dev/null 2>&1; then
    emit_event '{"warn":"ac_verifier_unavailable","detail":"ac-verifier binary not on PATH; reviewer-claude is the backstop"}'
    emit_done "shipped"; exit 0
fi

bundle_json=$(jq -nc --argjson acs "$acs_json" --arg diff "$diff_text" --arg vb "$verb_body" \
    '{acceptance_criteria:$acs, diff:$diff, verb_body:$vb}')

if ! outcome_json=$(timeout "$CLAUDE_P_TIMEOUT" ac-verifier <<<"$bundle_json" 2>/tmp/acv.err); then
    rc=$?
    err=$(tail -c 2048 /tmp/acv.err)
    emit_event "$(jq -nc --argjson rc "$rc" --arg err "$err" '{msg:"ac-verifier invocation failed — degrading to shipped",exit_code:$rc,detail:$err}')"
    emit_done "shipped"; exit 0
fi

outcome=$(jq -r '.outcome // "shipped"' <<<"$outcome_json")
if [ "$outcome" = "rework" ]; then
    findings_count=$(jq '.findings | length' <<<"$outcome_json")
    emit_event "$(jq -nc --argjson n "$findings_count" '{msg:"ac-verifier rework",findings_count:$n}')"
    i=0
    while [ "$i" -lt "$findings_count" ]; do
        sev=$(jq -r ".findings[$i].severity" <<<"$outcome_json")
        cat=$(jq -r ".findings[$i].category" <<<"$outcome_json")
        msg=$(jq -r ".findings[$i].message" <<<"$outcome_json")
        emit_finding_model "$sev" "$agent_id" "$cat" "$msg"
        i=$((i+1))
    done
    emit_done "rework_needed"
else
    emit_event '{"msg":"ac-verifier shipped — all acceptance criteria met or unverifiable"}'
    emit_done "shipped"
fi
"##;

const AUDITOR_CLAUDE_AGENTRY_SCRIPT: &str = r##"#!/usr/bin/env bash
set -uo pipefail
bundle=$(cat); brief_id=$(jq -r '.brief.id' <<<"$bundle")
cd /workspace || { emit_event '{"error":"cd /workspace failed"}'; emit_done "failed"; exit 0; }
emit_event '{"msg":"auditor starting"}'
clippy_out=$(cargo clippy --workspace --all-targets -- -D warnings 2>&1 | tail -c 8192 || true)
emit_event "$(jq -nc --arg out "$clippy_out" '{msg:"clippy_report",out:$out}')"
build_out=$(RUSTFLAGS="-Dwarnings" cargo build --workspace 2>&1 | tail -c 8192 || true)
emit_event "$(jq -nc --arg out "$build_out" '{msg:"build_report",out:$out}')"
test_out=$(cargo test --workspace 2>&1 | tail -c 8192 || true)
emit_event "$(jq -nc --arg out "$test_out" '{msg:"test_report",out:$out}')"
udeps_json=$(cargo +nightly udeps --workspace --output json 2>/dev/null || echo '{}')
emit_event "$(jq -nc --arg out "$(echo "$udeps_json" | tail -c 4096)" '{msg:"udeps_report",out:$out}')"
findings='[]'
if command -v ra-query >/dev/null 2>&1; then
    total_critical=0
    while IFS= read -r f; do
        [ -f "$f" ] || continue
        out=$(ra-query unwraps "$f" --severity critical --format json 2>/dev/null || echo '{"functions":[]}')
        cnt=$(echo "$out" | jq '[.functions[]?.unwraps[]?] | length')
        if [ "$cnt" -gt 0 ]; then
            findings=$(echo "$findings" | jq --argjson r "$out" --arg p "$f" '. + [{file:$p, critical_count:($r.functions|map(.unwraps|length)|add // 0), result:$r}]')
            total_critical=$((total_critical + cnt))
        fi
    done < <(find crates -name '*.rs' -not -path '*/tests/*' -not -name 'tests.rs' -not -path '*/target/*')
    emit_event "$(jq -nc --argjson cnt "$total_critical" --arg out "$(echo "$findings" | jq -c . | tail -c 8192)" '{msg:"unwraps_report",critical_total:$cnt,findings_json_tail:$out}')"
else
    emit_event '{"msg":"ra_query_unavailable","detail":"skipping unwraps stage"}'
fi
if command -v ra-query >/dev/null 2>&1; then
    cfindings='[]'; total_complex=0
    while IFS= read -r f; do
        [ -f "$f" ] || continue
        cout=$(ra-query complexity "$f" --threshold 15 --format json 2>/dev/null || echo '{"functions":[]}')
        ccnt=$(echo "$cout" | jq '[.functions[]?] | length')
        if [ "$ccnt" -gt 0 ]; then
            cfindings=$(echo "$cfindings" | jq --argjson r "$cout" --arg p "$f" '. + [{file:$p, complex_count:$ccnt, result:$r}]')
            total_complex=$((total_complex + ccnt))
        fi
    done < <(find crates -name '*.rs' -not -path '*/tests/*' -not -name 'tests.rs' -not -path '*/target/*')
    emit_event "$(jq -nc --argjson cnt "$total_complex" --arg out "$(echo "$cfindings" | jq -c . | tail -c 8192)" '{msg:"complexity_report",complex_total:$cnt,findings_json_tail:$out}')"
else
    emit_event '{"msg":"ra_query_unavailable_complexity","detail":"skipping complexity stage"}'
fi
mkdir -p /workspace/audit-children
host_workspace="/var/mnt/workspaces/agentry-work/briefs/${brief_id}"
refs='[]'
top_unwrap_files=$(echo "$findings" | jq -c 'sort_by(-.critical_count) | .[:3]')
unwrap_k=$(echo "$top_unwrap_files" | jq 'length')
j=0
while [ "$j" -lt "$unwrap_k" ]; do
  ufile=$(echo "$top_unwrap_files" | jq -r ".[$j].file")
  base=$(basename "$ufile")
  child="/workspace/audit-children/child-unwrap-${j}.json"
  jq -nc \
    --arg id "brf_self_heal_${brief_id}_unwrap_${j}" \
    --arg parent "$brief_id" \
    --arg ufile "$ufile" \
    --arg base "$base" \
    --argjson finding "$(echo "$top_unwrap_files" | jq ".[$j]")" \
    --argjson rank "$j" \
    '($finding.result.functions // []
        | map(. as $fn | ($fn.unwraps // [])
            | map("  - " + ($fn.name // "?") + " at " + (.file // "?") + ":" + ((.line // 0) | tostring) + " — critical — " + (.reason // "no reason")))
        | flatten | join("\n")) as $sites
     | {id:$id, project:null,
        topology:{name:"agentry-self-host-v0",version:1},
        payload:{
          issue_number:0,
          issue_title:("fix(unwraps): replace critical unwraps in " + $base),
          issue_body:("Replace critical unwraps in " + $ufile + ".\n\nSites:\n" + $sites + "\n\nFor each site choose the right replacement: ? if the function returns Result/Option, expect(\"<context>\") if the invariant truly holds and you can articulate why, unwrap_or / unwrap_or_else / ok_or if a fallback is appropriate. Do NOT silently swallow errors. Do NOT add bare expect(\"\") or expect(\"this should not fail\") — provide real context."),
          acceptance:"cargo fmt --check && cargo clippy --workspace --all-targets -- -D warnings && cargo test --workspace && scripts/arch-check.sh",
          target_repo:"yg/agentry",
          base_branch:"develop",
          pr_title:("fix(unwraps): replace critical unwraps in " + $base),
          pr_body:("Auto-dispatched by auditor (ra-query unwraps --severity critical, file ranked top-" + ($rank|tostring) + " by critical count).")
        },
        budget:{max_wall_seconds:1500},
        escalation:"autonomous",
        parent_brief:$parent,
        submitted_by:"auditor-self-heal",
        submitted_at:(now|todate)}' > "$child"
  refs=$(echo "$refs" | jq -c --arg p "${host_workspace}/audit-children/child-unwrap-${j}.json" '. + [$p]')
  j=$((j+1))
done
pairs=$(echo "$udeps_json" | jq -c '[.unused_deps // {} | to_entries[] | .key as $k | ((.value.normal // []) + (.value.development // []) + (.value.build // []))[] as $d | {crate:($k|split(" ")[0]), dep:$d}]')
count=$(echo "$pairs" | jq 'length')
i=0
while [ "$i" -lt "$count" ]; do
  crate=$(echo "$pairs" | jq -r ".[$i].crate"); dep=$(echo "$pairs" | jq -r ".[$i].dep")
  child="/workspace/audit-children/child-${i}.json"
  jq -nc --arg id "brf_self_heal_${brief_id}_udep_${i}" --arg crate "$crate" --arg dep "$dep" --arg parent "$brief_id" \
    '{id:$id, project:null, topology:{name:"agentry-bugfix-v0",version:1},
      payload:{issue_number:0, issue_title:("fix(deps): remove unused "+$dep+" from "+$crate),
        issue_body:("DELETE `"+$dep+".workspace = true` from crates/"+$crate+"/Cargo.toml. cargo-udeps reports unused."),
        acceptance:"cargo fmt --check && cargo clippy --workspace --all-targets -- -D warnings && RUSTFLAGS=\"-Dwarnings\" cargo build --workspace && cargo test --workspace",
        target_repo:"yg/agentry", base_branch:"develop",
        pr_title:("fix(deps): remove unused "+$dep+" from "+$crate),
        pr_body:"Auto-dispatched by auditor."},
      budget:{max_wall_seconds:900}, escalation:"autonomous", parent_brief:$parent,
      submitted_by:"auditor-self-heal", submitted_at:(now|todate)}' > "$child"
  refs=$(echo "$refs" | jq -c --arg p "${host_workspace}/audit-children/child-${i}.json" '. + [$p]')
  i=$((i+1))
done
[ "$(echo "$refs" | jq 'length')" -gt 0 ] && emit_message "_chain_trigger" "$(jq -nc --argjson r "$refs" '{next_brief_refs:$r}')"
emit_done "shipped"
"##;

/// Build the auditor-claude-agentry role. Extracted from `seed_m0` so the
/// permit-scope, passthru-env, and extra_bootstrap invariants can be asserted
/// in unit tests. The auditor compiles workspace, runs cargo-udeps and
/// `ra-query unwraps --severity critical`, and chain-triggers self-heal briefs
/// for unused-deps findings. ra-query unwrap findings auto-dispatch fix-child briefs for the top-K=3 files by critical_count (full agentry-self-host-v0 pipeline because unwrap fixes require judgment). Complexity findings remain report-only.
fn build_auditor_claude_agentry_role() -> AgentRole {
    AgentRole {
        name: RoleName("auditor-claude-agentry".into()),
        version: 1,
        model: None,
        system_prompt: None,
        image: "docker.io/library/rust:1.93".into(),
        substrate_class: SubstrateClass::Podman,
        package_manager: PackageManager::Apt,
        entrypoint_script: format!("{BASH_PRELUDE}{AUDITOR_CLAUDE_AGENTRY_SCRIPT}"),
        exitpoint_script: None,
        binaries: vec![],
        mcp_servers: vec![],
        tool_allowlist: ToolAllowlist(vec![]),
        permit_scope: PermitScope(vec![
            "fs:read:/workspace/**".into(),
            "fs:write:/workspace/**".into(),
            "net:allow:agentry-sccache-redis".into(),
            "net:allow:static.rust-lang.org".into(),
            "net:allow:crates.io".into(),
            "net:allow:index.crates.io".into(),
            "net:allow:static.crates.io".into(),
            "net:allow:agency.lab".into(),
        ]),
        passthru_env: vec!["GITEA_TOKEN".into()],
        extra_bootstrap: vec![
            "rustup component add rustfmt clippy || true".into(),
            "rustup toolchain install nightly --profile minimal || true".into(),
            "cargo +nightly install cargo-udeps --locked --quiet || true".into(),
            "CARGO_NET_GIT_FETCH_WITH_CLI=true cargo install --git https://oauth2:${GITEA_TOKEN}@agency.lab:3000/yg/ra-query.git --rev 2200414 --root /usr/local --locked --quiet ra-query || true".into(),
        ],
        mounts: vec![],
        workspace_mount: Some(WorkspaceMount {
            container_path: "/workspace".into(),
            readonly: false,
        }),
        sccache: true,
    }
}

/// Build the verifier-claude-agentry role. Despite the `claude` in the name —
/// kept for symmetry with the other agentry-* roles — the verifier never
/// invokes claude; it just runs `success_criteria` as a shell command on a
/// read-only snapshot of the workspace. Strictest permits in the registry:
/// fs:read on /workspace, fs:write on /tmp only, no net, no git, no claude.
fn build_verifier_claude_agentry_role() -> AgentRole {
    AgentRole {
        name: RoleName("verifier-claude-agentry".into()),
        version: 1,
        // No claude needed — criterion is shell.
        model: None,
        system_prompt: None,
        // Same rust-bookworm base as archaeologist so criteria can run cargo,
        // ripgrep, jq, etc. without per-criterion bootstrap.
        image: "docker.io/library/rust:1.93".into(),
        substrate_class: SubstrateClass::Podman,
        package_manager: PackageManager::Apt,
        entrypoint_script: format!("{BASH_PRELUDE}{VERIFIER_CLAUDE_AGENTRY_SCRIPT}"),
        exitpoint_script: None,
        binaries: vec![],
        mcp_servers: vec![],
        tool_allowlist: ToolAllowlist(vec![]),
        // Strictly read-only on workspace (criterion shouldn't mutate). /tmp
        // write for criterion temp files. No net, no git, no claude.
        permit_scope: PermitScope(vec![
            "fs:read:/workspace/**".into(),
            "fs:write:/tmp/**".into(),
        ]),
        passthru_env: vec![],
        extra_bootstrap: vec![],
        mounts: vec![],
        workspace_mount: Some(WorkspaceMount {
            container_path: "/workspace".into(),
            readonly: true,
        }),
        sccache: false,
    }
}

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
            Mount {
                source: "/var/lib/agentry/transcripts".into(),
                target: "/transcripts".into(),
                readonly: false,
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
    let coder_claude_agentry = build_coder_claude_agentry_role(&home, &claude_settings_path);
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
        permit_scope: PermitScope(vec![
            "fs:read:/workspace/**".into(),
            "net:allow:agency.lab".into(),
        ]),
        passthru_env: vec!["GITEA_TOKEN".into()],
        extra_bootstrap: vec![
            "rustup component add rustfmt clippy".into(),
            "git config --global http.sslVerify false".into(),
            "CARGO_NET_GIT_FETCH_WITH_CLI=true cargo install --git https://oauth2:${GITEA_TOKEN}@agency.lab:3000/yg/cfdb.git --rev 02c5a45 --root /usr/local --locked --quiet cfdb-cli || true".into(),
            "CARGO_NET_GIT_FETCH_WITH_CLI=true cargo install --git https://oauth2:${GITEA_TOKEN}@agency.lab:3000/yg/graph-specs-rust.git --rev ecaedb9 --root /usr/local --locked --quiet application || true".into(),
        ],
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
    let reviewer_claude_agentry = build_reviewer_claude_agentry_role(&home, &claude_settings_path);
    let ac_verifier_claude_agentry =
        build_ac_verifier_claude_agentry_role(&home, &claude_settings_path);
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
    // ---- agentry-null-v0 team (shake-down for role-introduction pipeline) ----
    // null-agent emits one event then `done shipped`. Zero work — exercises
    // the reviewer-claude Role-spec audit clause, permit broker, and spawner
    // teardown on a deliberately minimal-permitted, well-formed role before
    // real planner roles (archaeologist, planner, verifier) land.
    let null_agent_agentry = AgentRole {
        name: RoleName("null-agent-agentry".into()),
        version: 1,
        model: None,
        system_prompt: None,
        image: ALPINE.into(),
        substrate_class: SubstrateClass::Podman,
        package_manager: PackageManager::Apk,
        entrypoint_script: format!("{BASH_PRELUDE}{NULL_AGENT_AGENTRY_SCRIPT}"),
        exitpoint_script: None,
        binaries: vec![],
        mcp_servers: vec![],
        tool_allowlist: ToolAllowlist::default(),
        permit_scope: PermitScope::default(),
        passthru_env: vec![],
        extra_bootstrap: vec![],
        mounts: vec![],
        workspace_mount: None,
        sccache: false,
    };
    let agentry_null_v0 = TeamTopology {
        name: TeamName("agentry-null-v0".into()),
        version: 1,
        roles: vec![null_agent_agentry.name.clone()],
        message_graph: Vec::<MessageEdge>::new(),
        terminal_role: null_agent_agentry.name.clone(),
        max_retries: 0,
    };

    // ---- agentry-discovery-v0 team (first stage of the planner pipeline) ----
    // archaeologist-claude-agentry runs cfdb extract + graph-specs check, then
    // synthesizes a discovery.json via `claude -p`.
    let archaeologist_claude_agentry =
        build_archaeologist_claude_agentry_role(&home, &claude_settings_path);
    let agentry_discovery_v0 = TeamTopology {
        name: TeamName("agentry-discovery-v0".into()),
        version: 1,
        roles: vec![archaeologist_claude_agentry.name.clone()],
        message_graph: Vec::<MessageEdge>::new(),
        terminal_role: archaeologist_claude_agentry.name.clone(),
        max_retries: 0,
    };

    // ---- agentry-planner-v0 team (autonomous decomposition) ----
    // archaeologist → planner: archaeologist writes /workspace/discovery.json,
    // planner reads it and emits an outbox Message whose payload carries
    // `next_brief_refs` — a list of absolute host paths to child brief JSONs.
    // The daemon's chain-trigger auto-dispatches each child via submit_brief
    // once the planner ships.
    let planner_claude_agentry = build_planner_claude_agentry_role(&home, &claude_settings_path);
    let agentry_planner_v0 = TeamTopology {
        name: TeamName("agentry-planner-v0".into()),
        version: 1,
        roles: vec![
            archaeologist_claude_agentry.name.clone(),
            planner_claude_agentry.name.clone(),
        ],
        // discovery.json is on the shared workspace, not message-borne — the
        // edge exists only to gate the planner on the archaeologist shipping.
        message_graph: vec![MessageEdge {
            from: archaeologist_claude_agentry.name.clone(),
            to: planner_claude_agentry.name.clone(),
            permit_overrides_from: None,
        }],
        terminal_role: planner_claude_agentry.name.clone(),
        max_retries: 0,
    };

    // ---- agentry-verify-v0 team (DOL verifier — runs success_criteria) ----
    // Daemon-Orchestrated Lifecycle: when all children of a meta-brief reach
    // terminal verdict, the daemon auto-dispatches a verifier brief that runs
    // the meta-brief's success_criteria. The verifier's verdict composes with
    // the children's verdicts to produce the meta-brief's terminal verdict.
    let verifier_claude_agentry = build_verifier_claude_agentry_role();
    let agentry_verify_v0 = TeamTopology {
        name: TeamName("agentry-verify-v0".into()),
        version: 1,
        roles: vec![verifier_claude_agentry.name.clone()],
        message_graph: Vec::<MessageEdge>::new(),
        terminal_role: verifier_claude_agentry.name.clone(),
        max_retries: 0,
    };

    let agentry_self_host_v0 = TeamTopology {
        name: TeamName("agentry-self-host-v0".into()),
        version: 1,
        roles: vec![
            coder_claude_agentry.name.clone(),
            ac_verifier_claude_agentry.name.clone(),
            reviewer_mechanical_agentry.name.clone(),
            reviewer_claude_agentry.name.clone(),
            shipper_agentry.name.clone(),
            ci_watcher_agentry.name.clone(),
        ],
        // Rework loop enabled — max_retries=2 gives the coder two chances to
        // fix findings emitted by the reviewer before the team resolves Failed.
        message_graph: vec![
            // ORDERING INVARIANT: coder→reviewer edges are listed BEFORE
            // ac-verifier→reviewer edges so the daemon's
            // `team.incoming(reviewer).first()` rework lookup rewinds to the
            // coder, not to the (non-corrective) ac-verifier. Do not reorder.
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
            // Coder fans out to ac-verifier as well; ac-verifier short-circuits
            // failed-AC reworks BEFORE reviewer-claude is spent.
            MessageEdge {
                from: coder_claude_agentry.name.clone(),
                to: ac_verifier_claude_agentry.name.clone(),
                permit_overrides_from: None,
            },
            // Dual-inbound trick: ac-verifier also signals each reviewer so the
            // sequential flow holds, but rework still rewinds to coder above.
            MessageEdge {
                from: ac_verifier_claude_agentry.name.clone(),
                to: reviewer_mechanical_agentry.name.clone(),
                permit_overrides_from: None,
            },
            MessageEdge {
                from: ac_verifier_claude_agentry.name.clone(),
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

    let agentry_bugfix_v0 = TeamTopology {
        name: TeamName("agentry-bugfix-v0".into()),
        version: 1,
        roles: vec![
            coder_claude_agentry.name.clone(),
            reviewer_mechanical_agentry.name.clone(),
            shipper_agentry.name.clone(),
            ci_watcher_agentry.name.clone(),
        ],
        message_graph: vec![
            MessageEdge {
                from: coder_claude_agentry.name.clone(),
                to: reviewer_mechanical_agentry.name.clone(),
                permit_overrides_from: None,
            },
            MessageEdge {
                from: reviewer_mechanical_agentry.name.clone(),
                to: shipper_agentry.name.clone(),
                permit_overrides_from: None,
            },
            MessageEdge {
                from: shipper_agentry.name.clone(),
                to: ci_watcher_agentry.name.clone(),
                permit_overrides_from: None,
            },
        ],
        terminal_role: ci_watcher_agentry.name.clone(),
        max_retries: 2,
    };

    let agentry_spec_edit_v0 = TeamTopology {
        name: TeamName("agentry-spec-edit-v0".into()),
        version: 1,
        roles: vec![
            coder_claude_agentry.name.clone(),
            shipper_agentry.name.clone(),
            ci_watcher_agentry.name.clone(),
        ],
        message_graph: vec![
            MessageEdge {
                from: coder_claude_agentry.name.clone(),
                to: shipper_agentry.name.clone(),
                permit_overrides_from: None,
            },
            MessageEdge {
                from: shipper_agentry.name.clone(),
                to: ci_watcher_agentry.name.clone(),
                permit_overrides_from: None,
            },
        ],
        terminal_role: ci_watcher_agentry.name.clone(),
        max_retries: 1,
    };

    let auditor_claude_agentry = build_auditor_claude_agentry_role();
    let agentry_self_audit_v0 = TeamTopology {
        name: TeamName("agentry-self-audit-v0".into()),
        version: 1,
        roles: vec![auditor_claude_agentry.name.clone()],
        message_graph: Vec::<MessageEdge>::new(),
        terminal_role: auditor_claude_agentry.name.clone(),
        max_retries: 0,
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
    redis_io::save_role(&mut conn, &ac_verifier_claude_agentry).await?;
    redis_io::save_role(&mut conn, &shipper_agentry).await?;
    redis_io::save_role(&mut conn, &ci_watcher_agentry).await?;
    redis_io::save_team(&mut conn, &agentry_self_host_v0).await?;
    redis_io::save_team(&mut conn, &agentry_bugfix_v0).await?;
    redis_io::save_team(&mut conn, &agentry_spec_edit_v0).await?;
    redis_io::save_role(&mut conn, &auditor_claude_agentry).await?;
    redis_io::save_team(&mut conn, &agentry_self_audit_v0).await?;
    redis_io::save_role(&mut conn, &null_agent_agentry).await?;
    redis_io::save_team(&mut conn, &agentry_null_v0).await?;
    redis_io::save_role(&mut conn, &archaeologist_claude_agentry).await?;
    redis_io::save_team(&mut conn, &agentry_discovery_v0).await?;
    redis_io::save_role(&mut conn, &planner_claude_agentry).await?;
    redis_io::save_team(&mut conn, &agentry_planner_v0).await?;
    redis_io::save_role(&mut conn, &verifier_claude_agentry).await?;
    redis_io::save_team(&mut conn, &agentry_verify_v0).await?;

    tracing::info!(
        "seeded: roles [echo, workspace-probe, sccache-probe, timeout-probe, naughty, speaker, listener, grok-echo, claude-echo, synthesizer, narrowed-coder, shipper, coder-claude-agentry, ac-verifier-claude-agentry, reviewer-mechanical-agentry, shipper-agentry, ci-watcher-agentry, reviewer-claude-agentry, null-agent-agentry, archaeologist-claude-agentry, planner-claude-agentry, verifier-claude-agentry] (inline entrypoint scripts); teams [echo, workspace-probe, sccache-probe, timeout-probe, naughty, speaker-listener, grok-echo, claude-echo, narrowed-team, shipper-solo-team, agentry-self-host-v0, agentry-null-v0, agentry-discovery-v0, agentry-planner-v0, agentry-verify-v0]"
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn claude_p_timeout_is_env_overridable_in_bash_prelude() {
        assert!(BASH_PRELUDE.contains("CLAUDE_P_TIMEOUT=\"${CLAUDE_P_TIMEOUT:-1200}\""));
        assert!(!BASH_PRELUDE.contains("timeout 600"));
    }

    #[test]
    fn reviewer_claude_prompt_includes_role_spec_audit_clause() {
        assert!(
            REVIEWER_CLAUDE_AGENTRY_SCRIPT.contains("Role-spec audit (CRITICAL)"),
            "reviewer-claude prompt must include the Role-spec audit critical clause"
        );
        assert!(
            REVIEWER_CLAUDE_AGENTRY_SCRIPT.contains("permit_scope"),
            "Role-spec audit clause must reference permit_scope"
        );
        assert!(
            REVIEWER_CLAUDE_AGENTRY_SCRIPT.contains("tool_allowlist"),
            "Role-spec audit clause must reference tool_allowlist"
        );
    }

    #[test]
    fn reviewer_claude_prompt_includes_bootstrap_command_audit_clause() {
        assert!(
            REVIEWER_CLAUDE_AGENTRY_SCRIPT.contains("Bootstrap-command audit (CRITICAL)"),
            "reviewer-claude prompt must include the Bootstrap-command audit critical clause"
        );
    }

    #[test]
    fn reviewer_claude_prompt_includes_daemon_lifecycle_ordering_clause() {
        assert!(
            REVIEWER_CLAUDE_AGENTRY_SCRIPT.contains("Daemon-lifecycle ordering (CRITICAL)"),
            "reviewer-claude prompt must include the Daemon-lifecycle ordering critical clause"
        );
    }

    #[test]
    fn reviewer_claude_prompt_includes_state_machine_emission_idempotency_clause() {
        assert!(
            REVIEWER_CLAUDE_AGENTRY_SCRIPT.contains("State-machine emission idempotency (CRITICAL)"),
            "reviewer-claude prompt must include the State-machine emission idempotency critical clause"
        );
    }

    #[test]
    fn reviewer_script_salvages_missing_brackets() {
        assert!(
            REVIEWER_CLAUDE_AGENTRY_SCRIPT
                .contains("reviewer prose-reply salvaged as format_deviation"),
            "reviewer-claude script must salvage prose replies as format_deviation findings"
        );
    }

    #[test]
    fn reviewer_script_emits_format_deviation_finding() {
        assert!(
            REVIEWER_CLAUDE_AGENTRY_SCRIPT.contains("category:\"format_deviation\""),
            "reviewer-claude script must synthesize a finding with category format_deviation"
        );
    }

    #[test]
    fn reviewer_script_no_failed_emit_on_format_deviation() {
        let s = REVIEWER_CLAUDE_AGENTRY_SCRIPT;
        for marker in [
            "claude response not a JSON array",
            "claude response missing JSON array brackets",
        ] {
            let after = s
                .split_once(marker)
                .unwrap_or_else(|| panic!("marker {marker:?} must appear in reviewer script"))
                .1;
            let window = &after[..after.len().min(500)];
            assert!(
                !window.contains("emit_done \"failed\""),
                "reviewer salvage path after {marker:?} must not emit_done \"failed\"; window was: {window}"
            );
        }
    }

    #[test]
    fn reviewer_role_bind_mounts_ra_query() {
        let role = build_reviewer_claude_agentry_role(
            "/var/home/test",
            "/var/home/test/.config/agentry/claude-container-settings.json",
        );
        assert!(
            role.mounts
                .iter()
                .any(|m| m.target == "/usr/local/bin/ra-query" && m.readonly),
            "reviewer-claude must bind-mount ra-query read-only at /usr/local/bin/ra-query"
        );
    }

    #[test]
    fn coder_role_has_ra_query_mount() {
        let role = build_coder_claude_agentry_role(
            "/var/home/test",
            "/var/home/test/.config/agentry/claude-container-settings.json",
        );
        assert!(
            role.mounts
                .iter()
                .any(|m| m.target == "/usr/local/bin/ra-query" && m.readonly),
            "coder-claude must bind-mount ra-query read-only at /usr/local/bin/ra-query"
        );
    }

    #[test]
    fn reviewer_script_runs_ra_query_pre_pass() {
        let s = REVIEWER_CLAUDE_AGENTRY_SCRIPT;
        assert!(
            s.contains("ra-query unwraps"),
            "reviewer pre-pass must invoke ra-query unwraps"
        );
        assert!(
            s.contains("ra-query complexity"),
            "reviewer pre-pass must invoke ra-query complexity"
        );
        assert!(
            s.contains("ra_query_review_panel"),
            "reviewer pre-pass must emit ra_query_review_panel event"
        );
    }

    #[test]
    fn reviewer_script_tolerates_missing_ra_query() {
        let s = REVIEWER_CLAUDE_AGENTRY_SCRIPT;
        assert!(
            s.contains("command -v ra-query"),
            "reviewer pre-pass must guard ra-query with command -v"
        );
        assert!(
            s.contains("ra_query_unavailable"),
            "reviewer pre-pass must emit ra_query_unavailable when binary is missing"
        );
    }

    #[test]
    fn reviewer_script_injects_panel_summary_into_prompt() {
        let s = REVIEWER_CLAUDE_AGENTRY_SCRIPT;
        assert!(
            s.contains("Mechanical findings from ra-query"),
            "reviewer prompt must include the mechanical-findings header"
        );
        assert!(
            s.contains("--- End mechanical findings ---"),
            "reviewer prompt must include the mechanical-findings footer"
        );
    }

    #[test]
    fn null_agent_agentry_script_minimal() {
        assert!(
            NULL_AGENT_AGENTRY_SCRIPT.contains("emit_done \"shipped\""),
            "null-agent script must emit shipped"
        );
        assert!(
            NULL_AGENT_AGENTRY_SCRIPT.contains("emit_event"),
            "null-agent script must emit at least one event"
        );
        // Defensive: forbid claude / git / curl / cargo references in the body.
        for forbidden in ["claude", "git ", "curl", "cargo"] {
            assert!(
                !NULL_AGENT_AGENTRY_SCRIPT.contains(forbidden),
                "null-agent script must not contain {}",
                forbidden
            );
        }
    }

    #[test]
    fn bash_prelude_defines_stream_claude_with_pipefail_guard() {
        let p = BASH_PRELUDE;
        assert!(
            p.contains("stream_claude()"),
            "prelude must define stream_claude helper"
        );
        assert!(
            p.contains("--output-format stream-json --verbose"),
            "stream_claude must invoke claude -p with stream-json + verbose"
        );
        assert!(
            p.contains("} || true"),
            "stream_claude must wrap pipeline in `}} || true` so set -e does not race the failure branch"
        );
        assert!(
            p.contains("PIPESTATUS[0]"),
            "stream_claude must capture timeout's exit code via PIPESTATUS[0], not $?"
        );
        assert!(
            p.contains("/transcripts/${brief_id}"),
            "stream_claude must tee to /transcripts/${{brief_id}}<suffix>.jsonl"
        );
        // Regression: piping `.result` through `tail -1` silently drops
        // multi-line JSON content (claude pretty-prints findings arrays)
        // down to a trailing `]`, which then trips the reviewer's
        // `grep -m1 '\['` + `set -e` chain. The result event is unique per
        // transcript, so no `tail` is needed.
        assert!(
            !p.contains("select(.type==\"result\") | .result' \"$_t\" 2>/dev/null | tail"),
            "stream_claude must NOT pipe .result through tail (truncates multi-line JSON)"
        );
        assert!(
            !p.contains("select(.type==\"assistant\") | .message.content[]? | select(.type==\"text\") | .text' \"$_t\" 2>/dev/null | tail"),
            "stream_claude must NOT pipe assistant text through tail (truncates multi-line content)"
        );
        // Defence in depth: the tee/transcript-write failure mode is the
        // operator-trap behind brf_work_94 + brf_work_126 silent exit-2.
        // A future cleanup of stream_claude must NOT silently delete the
        // explicit `! -s "$_t"` empty-transcript guard or its trace event.
        assert!(
            p.contains("tee_or_transcript_write_failed"),
            "stream_claude must emit `tee_or_transcript_write_failed` when transcript is missing/empty"
        );
        assert!(
            p.contains("! -s \"$_t\""),
            "stream_claude must guard against an empty transcript via `! -s \"$_t\"`"
        );
    }

    #[test]
    fn all_claude_call_sites_use_stream_claude() {
        // Every script that previously did `reply=$(... claude -p ...)` /
        // `response=$(... claude -p ...)` must now go through stream_claude
        // so the streaming + pipefail guard is uniform. Self-review is the
        // one exception (intentional soft-fail; uses inline pipeline that
        // mirrors stream_claude's guard).
        for (name, s) in [
            ("CLAUDE_SCRIPT", CLAUDE_SCRIPT),
            ("CODER_CLAUDE_AGENTRY_SCRIPT", CODER_CLAUDE_AGENTRY_SCRIPT),
            (
                "REVIEWER_CLAUDE_AGENTRY_SCRIPT",
                REVIEWER_CLAUDE_AGENTRY_SCRIPT,
            ),
            (
                "ARCHAEOLOGIST_CLAUDE_AGENTRY_SCRIPT",
                ARCHAEOLOGIST_CLAUDE_AGENTRY_SCRIPT,
            ),
            (
                "PLANNER_CLAUDE_AGENTRY_SCRIPT",
                PLANNER_CLAUDE_AGENTRY_SCRIPT,
            ),
        ] {
            assert!(
                s.contains("stream_claude "),
                "{name} must call stream_claude (no buffered claude -p)"
            );
            assert!(
                !s.contains("reply=$(HOME=/root timeout")
                    && !s.contains("response=$(HOME=/root timeout"),
                "{name} must not retain the buffered `reply=$(... claude -p ...)` pattern"
            );
        }
    }

    #[test]
    fn coder_exitpoint_self_review_uses_pipefail_guard() {
        // Self-review is intentionally soft-fail (degrades to all_applied:true
        // on claude error) so it does not use stream_claude — but it MUST use
        // the same pipeline guard so set -e + pipefail does not kill the role
        // before the failure branch runs.
        let s = CODER_CLAUDE_AGENTRY_EXITPOINT;
        assert!(
            s.contains("--output-format stream-json --verbose"),
            "self-review must use stream-json output"
        );
        assert!(
            s.contains("PIPESTATUS[0]"),
            "self-review must capture exit via PIPESTATUS[0]"
        );
        assert!(
            s.contains(".self-review.jsonl"),
            "self-review transcript filename suffix"
        );
        // Same regression class as PR #129's stream_claude fix: piping `.result`
        // through `tail -1` truncates multi-line JSON to a trailing `}` and
        // the downstream `grep -m1 '{'` then misses, tripping pipefail+set -e.
        assert!(
            !s.contains(
                "select(.type==\"result\") | .result' \"$SR_TRANSCRIPT\" 2>/dev/null | tail"
            ),
            "self-review must NOT pipe .result through tail (truncates multi-line JSON)"
        );
        assert!(
            !s.contains("select(.type==\"assistant\") | .message.content[]? | select(.type==\"text\") | .text' \"$SR_TRANSCRIPT\" 2>/dev/null | tail"),
            "self-review assistant-text fallback must NOT pipe through tail"
        );
    }

    #[test]
    fn coder_exitpoint_has_dead_pub_gate() {
        // Lock down the pre-commit dead-pub gate so a future cleanup can't
        // silently delete it. Brief 1 of #134 ported the bash pipeline to a
        // Rust binary (`dead-pub-check`); a missing binary degrades to a
        // `dead_pub_check_unavailable` event without failing the role.
        let s = CODER_CLAUDE_AGENTRY_EXITPOINT;
        assert!(
            s.contains("dead-pub-check"),
            "coder exitpoint must invoke the dead-pub-check binary"
        );
        assert!(
            s.contains("running dead-pub-check"),
            "coder exitpoint must announce the dead-pub gate before running it"
        );
        assert!(
            s.contains("dead_pub_check_unavailable"),
            "coder exitpoint must emit dead_pub_check_unavailable when the binary is missing"
        );
    }

    #[test]
    fn coder_role_has_dead_pub_check_mount() {
        let role = build_coder_claude_agentry_role(
            "/var/home/test",
            "/var/home/test/.config/agentry/claude-container-settings.json",
        );
        assert!(
            role.mounts
                .iter()
                .any(|m| m.target == "/usr/local/bin/dead-pub-check" && m.readonly),
            "coder-claude must bind-mount dead-pub-check read-only at /usr/local/bin/dead-pub-check"
        );
    }

    #[test]
    fn ac_verifier_script_invokes_binary() {
        assert!(
            AC_VERIFIER_CLAUDE_AGENTRY_SCRIPT.contains("ac-verifier"),
            "ac-verifier script must invoke the ac-verifier binary"
        );
    }

    #[test]
    fn ac_verifier_script_reads_acceptance_criteria_payload_key() {
        assert!(
            AC_VERIFIER_CLAUDE_AGENTRY_SCRIPT.contains("acceptance_criteria"),
            "ac-verifier script must read brief.payload.acceptance_criteria"
        );
    }

    #[test]
    fn ac_verifier_script_handles_missing_binary() {
        assert!(
            AC_VERIFIER_CLAUDE_AGENTRY_SCRIPT.contains("ac_verifier_unavailable"),
            "ac-verifier script must emit ac_verifier_unavailable when the binary is missing"
        );
    }

    #[test]
    fn ac_verifier_role_bind_mounts_ac_verifier_binary() {
        let role = build_ac_verifier_claude_agentry_role("/h", "/c");
        assert!(
            role.mounts
                .iter()
                .any(|m| m.target == "/usr/local/bin/ac-verifier" && m.readonly),
            "ac-verifier role must bind-mount ac-verifier read-only at /usr/local/bin/ac-verifier"
        );
    }

    #[test]
    fn ac_verifier_role_bind_mounts_claude_binary() {
        let role = build_ac_verifier_claude_agentry_role("/h", "/c");
        assert!(
            role.mounts
                .iter()
                .any(|m| m.target == "/usr/local/bin/claude" && m.readonly),
            "ac-verifier role must bind-mount claude read-only at /usr/local/bin/claude"
        );
    }

    #[test]
    fn agentry_self_host_v0_topology_has_ac_verifier_with_correct_edges() {
        // Mirror of the agentry-self-host-v0 topology block in seed_m0 — built
        // here so the dual-inbound ordering invariant is covered without
        // touching Redis. Keep in sync with seed_m0.
        let coder = build_coder_claude_agentry_role("/h", "/c");
        let ac_verifier = build_ac_verifier_claude_agentry_role("/h", "/c");
        let reviewer_claude = build_reviewer_claude_agentry_role("/h", "/c");
        // Synthesize the names — mechanical reviewer + shipper + ci-watcher
        // are inline AgentRole literals in seed_m0; we only need their names
        // to assert edge presence.
        let reviewer_mechanical = RoleName("reviewer-mechanical-agentry".into());
        let shipper = RoleName("shipper-agentry".into());
        let ci_watcher = RoleName("ci-watcher-agentry".into());

        let topology = TeamTopology {
            name: TeamName("agentry-self-host-v0".into()),
            version: 1,
            roles: vec![
                coder.name.clone(),
                ac_verifier.name.clone(),
                reviewer_mechanical.clone(),
                reviewer_claude.name.clone(),
                shipper.clone(),
                ci_watcher.clone(),
            ],
            message_graph: vec![
                MessageEdge {
                    from: coder.name.clone(),
                    to: reviewer_mechanical.clone(),
                    permit_overrides_from: None,
                },
                MessageEdge {
                    from: coder.name.clone(),
                    to: reviewer_claude.name.clone(),
                    permit_overrides_from: None,
                },
                MessageEdge {
                    from: coder.name.clone(),
                    to: ac_verifier.name.clone(),
                    permit_overrides_from: None,
                },
                MessageEdge {
                    from: ac_verifier.name.clone(),
                    to: reviewer_mechanical.clone(),
                    permit_overrides_from: None,
                },
                MessageEdge {
                    from: ac_verifier.name.clone(),
                    to: reviewer_claude.name.clone(),
                    permit_overrides_from: None,
                },
                MessageEdge {
                    from: reviewer_mechanical.clone(),
                    to: shipper.clone(),
                    permit_overrides_from: None,
                },
                MessageEdge {
                    from: reviewer_claude.name.clone(),
                    to: shipper.clone(),
                    permit_overrides_from: None,
                },
                MessageEdge {
                    from: shipper.clone(),
                    to: ci_watcher.clone(),
                    permit_overrides_from: None,
                },
            ],
            terminal_role: ci_watcher.clone(),
            max_retries: 2,
        };

        assert!(
            topology.roles.contains(&ac_verifier.name),
            "ac-verifier-claude-agentry must be in roles"
        );

        let edge_idx = |from: &RoleName, to: &RoleName| -> Option<usize> {
            topology
                .message_graph
                .iter()
                .position(|e| e.from == *from && e.to == *to)
        };

        let coder_to_acv = edge_idx(&coder.name, &ac_verifier.name);
        let acv_to_rev_mech = edge_idx(&ac_verifier.name, &reviewer_mechanical);
        let acv_to_rev_claude = edge_idx(&ac_verifier.name, &reviewer_claude.name);
        let coder_to_rev_mech = edge_idx(&coder.name, &reviewer_mechanical);

        assert!(coder_to_acv.is_some(), "coder→ac-verifier edge must exist");
        assert!(
            acv_to_rev_mech.is_some(),
            "ac-verifier→reviewer-mechanical edge must exist"
        );
        assert!(
            acv_to_rev_claude.is_some(),
            "ac-verifier→reviewer-claude edge must exist"
        );
        assert!(
            coder_to_rev_mech.is_some(),
            "coder→reviewer-mechanical edge must exist (rework target)"
        );

        // Dual-inbound ordering invariant: coder→reviewer-mechanical MUST
        // appear BEFORE ac-verifier→reviewer-mechanical so the daemon's
        // `team.incoming(reviewer).first()` rework lookup rewinds to the
        // coder, not the (non-corrective) ac-verifier.
        let coder_pos =
            coder_to_rev_mech.expect("coder→reviewer-mechanical edge already asserted present");
        let acv_pos =
            acv_to_rev_mech.expect("ac-verifier→reviewer-mechanical edge already asserted present");
        assert!(
            coder_pos < acv_pos,
            "coder→reviewer-mechanical must appear before ac-verifier→reviewer-mechanical (rework rewinds to coder, not ac-verifier)"
        );
    }

    #[test]
    fn coder_script_extracts_team_context_messages() {
        let s = CODER_CLAUDE_AGENTRY_SCRIPT;
        assert!(
            s.contains("team_context.messages"),
            "coder script must read findings from team_context.messages"
        );
        assert!(
            s.contains("prior_findings"),
            "coder script must materialize a prior_findings variable"
        );
        assert!(
            s.contains("finding_count"),
            "coder script must compute a finding_count to gate the banner"
        );
    }

    #[test]
    fn coder_script_injects_rework_banner_when_findings_present() {
        let s = CODER_CLAUDE_AGENTRY_SCRIPT;
        assert!(
            s.contains("This is a REWORK iteration"),
            "coder script must announce rework iteration in the banner"
        );
        assert!(
            s.contains("--- Prior reviewer findings ---"),
            "coder script must wrap findings with a header delimiter"
        );
        assert!(
            s.contains("--- End findings ---"),
            "coder script must wrap findings with a footer delimiter"
        );
        assert!(
            s.contains("${rework_banner}"),
            "coder script prompt heredoc must interpolate the rework_banner shell variable"
        );
    }

    #[test]
    fn coder_script_filters_to_blocker_severity() {
        // warns are informational and must NOT be injected — they would
        // confuse Claude into addressing non-issues, and only blockers carry
        // prohibitions+requirements that the rework prompt depends on.
        let s = CODER_CLAUDE_AGENTRY_SCRIPT;
        assert!(
            s.contains(".severity == \"blocker\""),
            "coder script must filter findings to blocker severity"
        );
    }

    #[test]
    fn claude_using_roles_mount_transcripts_dir() {
        // Every role that mounts /usr/local/bin/claude must also mount
        // /var/lib/agentry/transcripts → /transcripts so stream_claude has a
        // host-bind-mounted directory to tee into.
        let home = "/var/home/test";
        let settings = "/var/home/test/.config/agentry/claude-container-settings.json";
        let roles = [
            ("planner", build_planner_claude_agentry_role(home, settings)),
            (
                "archaeologist",
                build_archaeologist_claude_agentry_role(home, settings),
            ),
            ("verifier", build_verifier_claude_agentry_role()),
        ];
        for (name, role) in roles {
            let mounts_claude = role
                .mounts
                .iter()
                .any(|m| m.target == "/usr/local/bin/claude");
            let mounts_transcripts = role.mounts.iter().any(|m| m.target == "/transcripts");
            if mounts_claude {
                assert!(
                    mounts_transcripts,
                    "role {name} mounts claude but not /transcripts"
                );
            }
        }
    }

    #[test]
    fn archaeologist_script_invariants() {
        let s = ARCHAEOLOGIST_CLAUDE_AGENTRY_SCRIPT;
        assert!(s.contains("cfdb extract"), "script must run cfdb extract");
        assert!(
            s.contains("graph-specs check"),
            "script must run graph-specs check"
        );
        assert!(
            s.contains("/workspace/discovery.json"),
            "script must write discovery.json"
        );
        assert!(
            s.contains("claude -p"),
            "script must invoke claude -p for synthesis"
        );
        assert!(
            s.contains("emit_done \"shipped\""),
            "script must emit shipped on success"
        );
        // No git push, no curl-of-anything-but-claude.
        assert!(!s.contains("git push"), "archaeologist must not push");
    }

    #[test]
    fn archaeologist_role_permits_minimal() {
        let role = build_archaeologist_claude_agentry_role(
            "/var/home/test",
            "/var/home/test/.config/agentry/claude-container-settings.json",
        );

        // Exactly the five expected scopes — workspace r/w (2), claude API (1),
        // agency.lab for cargo install (1), sccache redis (1).
        let scopes: Vec<&str> = role.permit_scope.0.iter().map(String::as_str).collect();
        let expected = [
            "fs:read:/workspace/**",
            "fs:write:/workspace/**",
            "net:allow:api.anthropic.com",
            "net:allow:agency.lab",
            "net:allow:agentry-sccache-redis",
        ];
        assert_eq!(
            scopes.len(),
            expected.len(),
            "archaeologist permit_scope must have exactly {} entries, got {:?}",
            expected.len(),
            scopes
        );
        for want in expected {
            assert!(
                scopes.contains(&want),
                "archaeologist permit_scope missing {want}: {scopes:?}"
            );
        }

        // No broad net:allow:*, no fs:write outside /workspace.
        for s in &scopes {
            assert!(
                *s != "net:allow:*",
                "archaeologist must not have broad net:allow:*"
            );
            if let Some(rest) = s.strip_prefix("fs:write:") {
                assert!(
                    rest.starts_with("/workspace"),
                    "archaeologist fs:write must be inside /workspace, got {s}"
                );
            }
        }

        // tool_allowlist is empty — only built-in emit_event/emit_done used.
        assert!(
            role.tool_allowlist.0.is_empty(),
            "archaeologist tool_allowlist must be empty (only emit_event/emit_done used)"
        );

        // No declared binaries — cfdb + graph-specs come via cargo install.
        assert!(
            role.binaries.is_empty(),
            "archaeologist binaries must be empty (cfdb/graph-specs via extra_bootstrap)"
        );
        let bootstrap_joined = role.extra_bootstrap.join("\n");
        assert!(
            bootstrap_joined.contains("cargo install") && bootstrap_joined.contains("cfdb"),
            "extra_bootstrap must cargo install cfdb"
        );
        assert!(
            bootstrap_joined.contains("graph-specs"),
            "extra_bootstrap must cargo install graph-specs"
        );
    }

    #[test]
    fn archaeologist_bootstrap_uses_canonical_cargo_install_pattern() {
        let role = build_archaeologist_claude_agentry_role(
            "/var/home/test",
            "/var/home/test/.config/agentry/claude-container-settings.json",
        );
        let bootstrap_text = role.extra_bootstrap.join("\n");

        // cfdb install: must use positional cfdb-cli, must not use --bin cfdb
        assert!(
            bootstrap_text.contains("cfdb-cli"),
            "archaeologist must install cfdb-cli (positional package name); current: {bootstrap_text}"
        );
        assert!(
            !bootstrap_text.contains("--bin cfdb "),
            "archaeologist must NOT use --bin cfdb (cfdb workspace has multiple binaries; use positional cfdb-cli)"
        );

        // graph-specs install: must use positional application
        assert!(
            bootstrap_text.contains(" application "),
            "archaeologist must install graph-specs via positional 'application' package"
        );
        assert!(
            !bootstrap_text.contains("--bin graph-specs"),
            "archaeologist must NOT use --bin graph-specs (use positional 'application')"
        );

        // Fault tolerance — matches existing reviewer-mechanical-agentry pattern.
        assert!(
            bootstrap_text.contains("|| true"),
            "archaeologist cargo installs must include || true fault tolerance"
        );
    }

    #[test]
    fn planner_script_invariants() {
        let s = PLANNER_CLAUDE_AGENTRY_SCRIPT;
        assert!(
            s.contains("/workspace/discovery.json"),
            "must read discovery.json"
        );
        assert!(
            s.contains("/workspace/planner-children"),
            "must write children to manifest dir"
        );
        assert!(s.contains("claude -p"), "must invoke claude -p");
        assert!(
            s.contains("next_brief_refs"),
            "must emit next_brief_refs in outbox message"
        );
        assert!(
            s.contains("emit_done \"shipped\""),
            "must emit shipped on success"
        );
        assert!(!s.contains("git push"), "planner must not push");
        assert!(
            !s.contains("orchestrator submit"),
            "planner must NOT call CLI directly — children dispatched by daemon chain-trigger"
        );
    }

    #[test]
    fn planner_role_permits_minimal() {
        let role = build_planner_claude_agentry_role(
            "/var/home/test",
            "/var/home/test/.config/agentry/claude-container-settings.json",
        );

        // Strictly tighter than archaeologist: only workspace r/w + claude API.
        let scopes: Vec<&str> = role.permit_scope.0.iter().map(String::as_str).collect();
        let expected = [
            "fs:read:/workspace/**",
            "fs:write:/workspace/**",
            "net:allow:api.anthropic.com",
        ];
        assert_eq!(
            scopes.len(),
            expected.len(),
            "planner permit_scope must have exactly {} entries, got {:?}",
            expected.len(),
            scopes
        );
        for want in expected {
            assert!(
                scopes.contains(&want),
                "planner permit_scope missing {want}: {scopes:?}"
            );
        }

        // Forbidden scopes — planner does not cargo-install, compile, or
        // touch the forge.
        for forbidden in [
            "net:allow:agency.lab",
            "net:allow:agentry-sccache-redis",
            "net:allow:*",
        ] {
            assert!(
                !scopes.contains(&forbidden),
                "planner permit_scope must not contain {forbidden}: {scopes:?}"
            );
        }

        // No GITEA_TOKEN — planner does not touch the forge.
        assert!(
            !role.passthru_env.iter().any(|e| e == "GITEA_TOKEN"),
            "planner must not pass through GITEA_TOKEN: {:?}",
            role.passthru_env
        );

        // No declared binaries, no extra_bootstrap — planner installs nothing.
        assert!(role.binaries.is_empty(), "planner binaries must be empty");
        assert!(
            role.extra_bootstrap.is_empty(),
            "planner extra_bootstrap must be empty"
        );

        // tool_allowlist empty — only built-in emit_event/emit_message/emit_done.
        assert!(
            role.tool_allowlist.0.is_empty(),
            "planner tool_allowlist must be empty"
        );

        // Workspace mount is writable so the planner can write child JSONs.
        let ws = role.workspace_mount.expect("planner needs workspace_mount");
        assert_eq!(ws.container_path, "/workspace");
        assert!(!ws.readonly, "planner workspace must be writable");
    }

    #[test]
    fn verifier_script_invariants() {
        let s = VERIFIER_CLAUDE_AGENTRY_SCRIPT;
        assert!(s.contains("success_criteria"), "must read success_criteria");
        assert!(
            s.contains("emit_done \"shipped\""),
            "must emit shipped on pass"
        );
        assert!(
            s.contains("emit_done \"failed\""),
            "must emit failed on fail"
        );
        assert!(!s.contains("git"), "verifier must not touch git");
        assert!(!s.contains("curl"), "verifier must not call curl");
    }

    #[test]
    fn verifier_role_permits_strictest() {
        let role = build_verifier_claude_agentry_role();

        // Exactly two scopes — workspace read, /tmp write. No net at all.
        let scopes: Vec<&str> = role.permit_scope.0.iter().map(String::as_str).collect();
        let expected = ["fs:read:/workspace/**", "fs:write:/tmp/**"];
        assert_eq!(
            scopes.len(),
            expected.len(),
            "verifier permit_scope must have exactly {} entries, got {:?}",
            expected.len(),
            scopes
        );
        for want in expected {
            assert!(
                scopes.contains(&want),
                "verifier permit_scope missing {want}: {scopes:?}"
            );
        }

        // No net:allow:* of any kind — criterion is local-only in v0.
        for s in &scopes {
            assert!(
                !s.starts_with("net:"),
                "verifier must not have any net scope, got {s}"
            );
        }

        // No fs:write outside /tmp.
        for s in &scopes {
            if let Some(rest) = s.strip_prefix("fs:write:") {
                assert!(
                    rest.starts_with("/tmp"),
                    "verifier fs:write must be /tmp only, got {s}"
                );
            }
        }

        // No claude / forge / cargo install — verifier installs nothing.
        assert!(role.binaries.is_empty(), "verifier binaries must be empty");
        assert!(
            role.extra_bootstrap.is_empty(),
            "verifier extra_bootstrap must be empty"
        );
        assert!(role.mounts.is_empty(), "verifier mounts must be empty");
        assert!(
            !role.passthru_env.iter().any(|e| e == "GITEA_TOKEN"),
            "verifier must not pass through GITEA_TOKEN"
        );
        assert!(
            role.tool_allowlist.0.is_empty(),
            "verifier tool_allowlist must be empty"
        );
        assert!(role.model.is_none(), "verifier must not declare a model");

        // Workspace mount is read-only.
        let ws = role
            .workspace_mount
            .expect("verifier needs workspace_mount");
        assert_eq!(ws.container_path, "/workspace");
        assert!(ws.readonly, "verifier workspace must be read-only");
    }

    #[test]
    fn ci_watcher_merge_retries_transient_405_and_409() {
        let s = CI_WATCHER_AGENTRY_SCRIPT;

        // Retry budget identifier is present.
        assert!(
            s.contains("merge_max_retries"),
            "ci-watcher must declare a merge_max_retries budget"
        );

        // Both transient codes are explicitly handled as retry triggers.
        assert!(
            s.contains("\"405\""),
            "ci-watcher must treat 405 as a retry-able transient code"
        );
        assert!(
            s.contains("\"409\""),
            "ci-watcher must treat 409 as a retry-able transient code"
        );

        // Backoff uses sleep + $RANDOM jitter.
        assert!(
            s.contains("sleep \"$merge_sleep\""),
            "ci-watcher must sleep between merge retries"
        );
        assert!(
            s.contains("RANDOM"),
            "ci-watcher must use $RANDOM for jitter to avoid thundering herd"
        );
        assert!(
            s.contains("merge_jitter"),
            "ci-watcher must compute a named merge_jitter from $RANDOM"
        );

        // Non-transient failures are explicitly distinguished and fail fast.
        assert!(
            s.contains("non-transient"),
            "ci-watcher must distinguish non-transient merge failures explicitly"
        );

        // Successful merge event carries the attempt count.
        assert!(
            s.contains("merge_attempt:$a"),
            "ci-watcher merged event must include the merge_attempt count"
        );

        // Budget-exhaustion path emits the last http code + body and fails.
        assert!(
            s.contains("merge retry budget exhausted"),
            "ci-watcher must emit a budget-exhausted error event when retries run out"
        );
    }

    #[test]
    fn auditor_role_passes_through_gitea_token() {
        let auditor = build_auditor_claude_agentry_role();
        assert!(
            auditor.passthru_env.iter().any(|e| e == "GITEA_TOKEN"),
            "auditor must pass GITEA_TOKEN so the ra-query cargo install can authenticate against agency.lab: {:?}",
            auditor.passthru_env
        );
    }

    #[test]
    fn auditor_role_permit_includes_agency_lab() {
        let auditor = build_auditor_claude_agentry_role();
        assert!(
            auditor
                .permit_scope
                .0
                .iter()
                .any(|s| s == "net:allow:agency.lab"),
            "auditor permit_scope must allow agency.lab for the ra-query cargo install: {:?}",
            auditor.permit_scope.0
        );
    }

    #[test]
    fn auditor_extra_bootstrap_installs_ra_query() {
        let auditor = build_auditor_claude_agentry_role();
        let bootstrap = auditor.extra_bootstrap.join("\n");
        assert!(
            bootstrap.contains("ra-query.git"),
            "auditor extra_bootstrap must cargo install ra-query.git: {bootstrap}"
        );
        assert!(
            bootstrap.contains("--rev 2200414"),
            "auditor extra_bootstrap must pin ra-query to --rev 2200414: {bootstrap}"
        );
        assert!(
            bootstrap.contains("|| true"),
            "auditor ra-query install must end with || true for fault tolerance: {bootstrap}"
        );
    }

    #[test]
    fn auditor_script_runs_ra_query_unwraps_critical() {
        assert!(
            AUDITOR_CLAUDE_AGENTRY_SCRIPT.contains("ra-query unwraps"),
            "auditor script must invoke `ra-query unwraps`"
        );
        assert!(
            AUDITOR_CLAUDE_AGENTRY_SCRIPT.contains("--severity critical"),
            "auditor script must filter ra-query unwraps with --severity critical"
        );
        assert!(
            AUDITOR_CLAUDE_AGENTRY_SCRIPT.contains("unwraps_report"),
            "auditor script must emit an unwraps_report trace event"
        );
    }

    #[test]
    fn auditor_emits_unwrap_fix_children() {
        assert!(
            AUDITOR_CLAUDE_AGENTRY_SCRIPT.contains("audit-children/child-unwrap-"),
            "auditor must write unwrap fix-child briefs to audit-children/child-unwrap-*"
        );
        assert!(
            AUDITOR_CLAUDE_AGENTRY_SCRIPT.contains("brf_self_heal_${brief_id}_unwrap_"),
            "auditor must generate brf_self_heal_<brief_id>_unwrap_<j> identifiers"
        );
        assert!(
            AUDITOR_CLAUDE_AGENTRY_SCRIPT.contains("agentry-self-host-v0"),
            "unwrap fix-child briefs must dispatch into agentry-self-host-v0 (not bugfix-v0)"
        );
        assert!(
            AUDITOR_CLAUDE_AGENTRY_SCRIPT.contains("sort_by(-.critical_count)"),
            "auditor must select top-K unwrap files via sort_by(-.critical_count)"
        );
        assert!(
            AUDITOR_CLAUDE_AGENTRY_SCRIPT.contains("auditor-self-heal"),
            "unwrap fix-child briefs must reuse the auditor-self-heal submitted_by tag"
        );
        assert!(
            AUDITOR_CLAUDE_AGENTRY_SCRIPT.contains("Do NOT silently swallow errors"),
            "unwrap fix-child briefs must carry the swallow-errors constraint phrase"
        );
    }

    #[test]
    fn auditor_script_runs_ra_query_complexity() {
        assert!(
            AUDITOR_CLAUDE_AGENTRY_SCRIPT.contains("ra-query complexity"),
            "auditor script must invoke `ra-query complexity`"
        );
        assert!(
            AUDITOR_CLAUDE_AGENTRY_SCRIPT.contains("--threshold 15"),
            "auditor script must filter ra-query complexity with --threshold 15"
        );
        assert!(
            AUDITOR_CLAUDE_AGENTRY_SCRIPT.contains("complexity_report"),
            "auditor script must emit a complexity_report trace event"
        );
    }

    #[test]
    fn auditor_script_does_not_chain_trigger_on_complexity() {
        assert!(
            !AUDITOR_CLAUDE_AGENTRY_SCRIPT.contains("brf_self_heal_${brief_id}_complex"),
            "auditor v1.5 must NOT auto-dispatch complexity self-heal child briefs"
        );
        assert!(
            !AUDITOR_CLAUDE_AGENTRY_SCRIPT.contains("_complex_"),
            "auditor v1.5 must NOT generate per-complexity child-brief identifiers"
        );
    }
}
