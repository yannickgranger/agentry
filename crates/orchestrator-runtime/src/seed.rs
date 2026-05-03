//! Seed the Redis registry with the agent roles and team topologies.
//!
//! Each role carries its entrypoint as an inline bash script (no per-agent
//! Containerfile). The spawner picks a stock public base image, installs the
//! role's declared `binaries` via `package_manager`, then execs the script.
//!
//! Idempotent: overwrites existing records with current definitions.

use crate::{redis_io, role_dir_loader, Config, Result};
use orchestrator_types::{
    AgentRole, MessageEdge, Mount, PackageManager, PermitScope, RoleName, RoleRef, SubstrateClass,
    TeamName, TeamTopology, ToolAllowlist, WorkspaceMount,
};
use std::path::PathBuf;

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

// EPIC #161 Wave 1.2a + 1.2b: BOTH halves of the coder-claude bash heredoc
// (entrypoint AND exitpoint) have been ported to one Rust runner —
// `crates/agentry-role-runtime/src/bin/coder_claude_runner.rs`. The role's
// entrypoint_script now `exec`s the runner; exitpoint_script is None. The
// merged binary owns the full role lifecycle:
//
//   - Read bundle, walk team_context for prior blocker findings,
//     compose verb-structured prompt with optional rework banner, stream
//     `claude -p` to /transcripts/<brief_id>.coder.jsonl.
//   - v1+ topology shortcut: best-effort `cargo fmt --all` then ship
//     (git-operator role handles commit/push for v1+).
//   - v0 topology exitpoint: cargo fmt → quality-hygiene → eval acceptance
//     → git add -A → optional self-review claude soft-fail → optional
//     dead-pub-check JSONL → git commit + emit committed event → done shipped.
//
// /tmp/brief_vars.sh cross-language IPC is gone (Wave 1.2a's transitional
// handle); state lives in typed Rust structs throughout. DoneGuard now
// wraps the whole role.

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

// EPIC #161 Wave 1.4: REVIEWER_CLAUDE_AGENTRY_SCRIPT bash heredoc that used
// to live here has been ported to a Rust runner —
// `crates/agentry-role-runtime/src/bin/reviewer_claude_runner.rs`. The role's
// entrypoint_script now just `exec /usr/local/bin/reviewer-claude-runner`.
// The runner has its own unit-test coverage for diff parsing, fence
// stripping, JSON salvage, severity mapping, and assistant-text reconstruction
// from claude stream-json transcripts.

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
/// emits Failed with CI context on CI red. Brief 137b adds a parallel
/// mergeable poll (`/pulls/<num>`): when `mergeable: false` (develop advanced
/// past the PR's base) the loop chain-triggers a `pr-rebaser-agentry` brief
/// and exits — the rebaser force-pushes the rebased branch on a separate
/// brief instance.
const CI_WATCHER_AGENTRY_SCRIPT: &str = r##"#!/usr/bin/env bash
set -euo pipefail
bundle="$(cat)"

brief_id=$(jq -r '.brief.id' <<<"$bundle")
target_repo=$(jq -r '.brief.payload.target_repo // "yg/agentry"' <<<"$bundle")
forge_host=$(jq -r '.brief.payload.forge_host // "agency.lab:3000"' <<<"$bundle")
owner="${target_repo%%/*}"
repo_name="${target_repo##*/}"

# Host workspace path mirrors workspace::DEFAULT_ROOT in the runtime; the
# daemon's chain-trigger reads brief JSON files off-container, so paths
# emitted in next_brief_refs MUST be absolute host paths.
host_workspace="/var/mnt/workspaces/agentry-work/briefs/${brief_id}"

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
pr_url_api="https://${forge_host}/api/v1/repos/${owner}/${repo_name}/pulls/${pr_number}"
status_url="https://${forge_host}/api/v1/repos/${owner}/${repo_name}/commits/${head_sha}/status"
for i in $(seq 1 "$max_polls"); do
    # Mergeable check (Brief 137b). The forge populates `mergeable`
    # asynchronously after PR creation — `null` means "still computing",
    # `false` means develop moved under us. Only treat literal `false` as
    # a chain-trigger signal; `null` falls through to the CI poll which
    # gives gitea time to settle. `state == "closed"` means the PR was
    # merged or closed externally — common after a rebase iteration where
    # a new PR replaced this one.
    pr_resp=$(curl -sS -k -H "Authorization: token ${GITEA_TOKEN}" "$pr_url_api" 2>/tmp/pr.err) || {
        err=$(tail -5 /tmp/pr.err)
        emit_event "$(jq -nc --arg err "$err" '{error:"pr GET failed",detail:$err}')"
        sleep 15; continue
    }
    pr_mergeable=$(jq -r '.mergeable // "unknown"' <<<"$pr_resp")
    pr_state=$(jq -r '.state // "open"' <<<"$pr_resp")
    pr_branch=$(jq -r '.head.ref // ""' <<<"$pr_resp")
    pr_base=$(jq -r '.base.ref // "develop"' <<<"$pr_resp")

    if [ "$pr_state" = "closed" ]; then
        # Already merged or closed externally — common after a rebase
        # iteration replaced this PR. Treat as success-equivalent for this
        # brief instance; the new PR (if any) has its own ci-watcher.
        emit_event "$(jq -nc --argjson n "$pr_number" '{msg:"pr closed externally",pr_number:$n}')"
        emit_done "shipped"; exit 0
    fi

    if [ "$pr_mergeable" = "false" ]; then
        # Develop moved under us — chain-trigger pr-rebaser-agentry on a
        # fresh brief and end this ci-watcher. The rebaser force-pushes
        # the rebased branch (or surfaces conflicts as findings); the
        # forge re-runs CI on the new head, which is observed by a
        # subsequent ci-watcher dispatch.
        if [ -z "$pr_branch" ]; then
            emit_event '{"error":"pr_resp missing .head.ref — cannot chain-trigger rebaser without branch"}'
            emit_done "failed"; exit 0
        fi
        rebaser_brief_id="brf_rebaser_${brief_id}_pr${pr_number}"
        rebaser_path="/workspace/pr_rebaser_brief.json"
        submitted_at=$(date -Iseconds)
        jq -nc \
            --arg id "$rebaser_brief_id" \
            --arg target_repo "$target_repo" \
            --argjson pr_number "$pr_number" \
            --arg branch "$pr_branch" \
            --arg base_branch "$pr_base" \
            --arg forge_host "$forge_host" \
            --arg parent "$brief_id" \
            --arg submitted_by "ci-watcher-agentry-${brief_id}" \
            --arg submitted_at "$submitted_at" \
            '{
                id: $id,
                project: null,
                topology: { name: "agentry-pr-rebaser-v0", version: 1 },
                payload: {
                    target_repo: $target_repo,
                    pr_number: $pr_number,
                    branch: $branch,
                    base_branch: $base_branch,
                    forge_host: $forge_host
                },
                budget: { max_wall_seconds: 600 },
                escalation: "autonomous",
                parent_brief: $parent,
                submitted_by: $submitted_by,
                submitted_at: $submitted_at
            }' > "$rebaser_path"
        host_path="${host_workspace}/pr_rebaser_brief.json"
        emit_event "$(jq -nc --argjson n "$pr_number" --arg b "$pr_branch" --arg base "$pr_base" --arg p "$host_path" \
            '{msg:"pr not mergeable — chain-triggering pr-rebaser-agentry",pr_number:$n,branch:$b,base_branch:$base,next_brief_ref:$p}')"
        # Sentinel target `_chain_trigger`: there is no role by that name on
        # the topology — the daemon's chain-trigger scans every accumulated
        # outbox payload for next_brief_refs regardless of `to`, so this
        # message carries the dispatch list without targeting any sibling.
        emit_message "_chain_trigger" "$(jq -nc --arg p "$host_path" '{next_brief_refs:[$p]}')"
        emit_done "shipped"; exit 0
    fi

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
                '{msg:"CI red — emitting rework_needed for coder loop-back",state:$s,failing_context:$ctx}')"
            emit_finding "blocker" "ci-watcher" "ci" "CI red on $ctx"
            emit_done "rework_needed"; exit 0
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

/// Bash entrypoint for `pr-rebaser-agentry`. Substrate auto-rebaser (#137):
/// fetches the PR's base + head branches, attempts `git rebase
/// origin/<base>`, and either force-pushes the rebased branch or surfaces
/// the conflicts as findings so the coder can re-roll the PR. Brief 137b
/// will wire this role into ci-watcher's chain-trigger so a stale PR
/// auto-rebases without operator intervention.
const PR_REBASER_AGENTRY_SCRIPT: &str = r##"#!/usr/bin/env bash
set -euo pipefail
bundle="$(cat)"

target_repo=$(jq -r '.brief.payload.target_repo // "yg/agentry"' <<<"$bundle")
pr_number=$(jq -r '.brief.payload.pr_number // ""' <<<"$bundle")
branch=$(jq -r '.brief.payload.branch // ""' <<<"$bundle")
base_branch=$(jq -r '.brief.payload.base_branch // "develop"' <<<"$bundle")
forge_host=$(jq -r '.brief.payload.forge_host // "agency.lab:3000"' <<<"$bundle")

if [ -z "$branch" ] || [ "$branch" = "null" ]; then
    emit_event '{"error":"branch missing in brief.payload"}'
    emit_done "failed"; exit 0
fi

if [ ! -d /workspace/.git ] && [ ! -f /workspace/.git ]; then
    emit_event '{"error":"workspace missing — no .git found at /workspace"}'
    emit_done "failed"; exit 0
fi

cd /workspace

# Idempotent — `git config` overwrites the existing value rather than appending.
git config user.email "pr-rebaser@agentry.lab"
git config user.name "pr-rebaser"

emit_event "$(jq -nc --arg b "$branch" --arg base "$base_branch" --arg pr "$pr_number" --arg repo "$target_repo" --arg fh "$forge_host" \
    '{msg:"pr-rebaser starting",branch:$b,base_branch:$base,pr_number:$pr,target_repo:$repo,forge_host:$fh}')"

if ! git fetch origin "$base_branch" 2>/tmp/fetch.err; then
    err=$(tail -10 /tmp/fetch.err)
    emit_event "$(jq -nc --arg base "$base_branch" --arg err "$err" '{error:"git fetch base failed",base:$base,detail:$err}')"
    emit_done "failed"; exit 0
fi

if ! git fetch origin "$branch" 2>/tmp/fetch.err; then
    err=$(tail -10 /tmp/fetch.err)
    emit_event "$(jq -nc --arg b "$branch" --arg err "$err" '{error:"git fetch branch failed",branch:$b,detail:$err}')"
    emit_done "failed"; exit 0
fi

if ! git checkout "$branch" 2>/tmp/co.err; then
    err=$(tail -10 /tmp/co.err)
    emit_event "$(jq -nc --arg b "$branch" --arg err "$err" '{error:"git checkout failed",branch:$b,detail:$err}')"
    emit_done "failed"; exit 0
fi

base_sha=$(git rev-parse "origin/${base_branch}")

rebase_rc=0
git rebase "origin/${base_branch}" >/tmp/rebase.out 2>&1 || rebase_rc=$?

if [ "$rebase_rc" = "0" ]; then
    new_sha=$(git rev-parse HEAD)
    if ! git push --force-with-lease origin "$branch" 2>/tmp/push.err; then
        err=$(tail -10 /tmp/push.err)
        emit_event "$(jq -nc --arg b "$branch" --arg err "$err" '{error:"git push --force-with-lease failed",branch:$b,detail:$err}')"
        emit_done "failed"; exit 0
    fi
    emit_event "$(jq -nc --arg b "$branch" --arg base "$base_sha" --arg new "$new_sha" \
        '{msg:"rebased and pushed",rebased:true,branch:$b,base_sha:$base,new_sha:$new}')"
    emit_done "shipped"; exit 0
fi

# Non-zero exit. Distinguish conflict (unmerged paths in `git status
# --porcelain=v2 -uno`) from any other failure.
status_out=$(git status --porcelain=v2 -uno 2>/tmp/status.err || true)
unmerged_files=$(printf '%s\n' "$status_out" | awk '/^u / {print $NF}')

if [ -n "$unmerged_files" ]; then
    while IFS= read -r f; do
        [ -z "$f" ] && continue
        emit_finding "blocker" "pr-rebaser" "rebase-conflict" "rebase conflict in $f"
    done <<<"$unmerged_files"
    git rebase --abort >/dev/null 2>&1 || true
    emit_event "$(jq -nc --arg b "$branch" '{msg:"rebase conflicts — aborted, requesting rework",branch:$b}')"
    emit_done "rework_needed"; exit 0
fi

# Non-conflict failure (e.g. detached worktree, missing ref). Abort any
# in-progress rebase and surface the captured stdout+stderr.
detail=$(tail -30 /tmp/rebase.out)
git rebase --abort >/dev/null 2>&1 || true
emit_event "$(jq -nc --arg b "$branch" --arg err "$detail" '{error:"git rebase failed (non-conflict)",branch:$b,detail:$err}')"
emit_done "failed"
"##;

// EPIC #161 Wave 3: ARCHAEOLOGIST_CLAUDE_AGENTRY_SCRIPT bash heredoc that
// used to live here has been ported to a Rust runner —
// `crates/agentry-role-runtime/src/bin/archaeologist_runner.rs`. The role's
// entrypoint_script now just `exec /usr/local/bin/archaeologist-runner`.
// The runner has its own unit-test coverage for cfdb-counts parsing,
// discovery-seed extraction, prompt assembly, and JSON-object slicing.

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
        allowed_tools: None,
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
        entrypoint_script: "#!/bin/sh\nexec /usr/local/bin/archaeologist-runner\n".into(),
        exitpoint_script: None,
        // cfdb + graph-specs come via extra_bootstrap cargo install (same
        // pattern as quality-hygiene in coder-claude-agentry).
        binaries: vec![],
        mcp_servers: vec![],
        tool_allowlist: ToolAllowlist(vec![]),
        allowed_tools: None,
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
            Mount {
                source: format!("{home}/.local/bin/archaeologist-runner"),
                target: "/usr/local/bin/archaeologist-runner".into(),
                readonly: true,
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

// EPIC #161 Wave 1.3: the three AC_VERIFIER_*_AGENTRY_SCRIPT bash heredocs
// that used to live here have been ported to one Rust runner —
// `crates/agentry-role-runtime/src/bin/ac_verifier_runner.rs` — parameterized
// by `--provider claude|gemini|grok`. The roles' entrypoint_scripts now just
// `exec /usr/local/bin/ac-verifier-runner --provider X`. The runner has its
// own unit-test coverage for AC parsing / degradation envelopes.

// EPIC #161 Wave 2: AUDITOR_CLAUDE_AGENTRY_SCRIPT bash heredoc that used to
// live here has been ported to a Rust runner —
// `crates/agentry-role-runtime/src/bin/auditor_claude_runner.rs`. The role's
// entrypoint_script now just `exec /usr/local/bin/auditor-claude-runner`.
// The runner has its own unit-test coverage for the udeps-pair walker, the
// unwraps-count aggregation, and the per-finding sites-block formatting.

/// Build the auditor-claude-agentry role. Extracted from `seed_m0` so the
/// permit-scope, passthru-env, and extra_bootstrap invariants can be asserted
/// in unit tests. Bind-mounts host-built ra-query at /usr/local/bin/ra-query
/// (operator runs `just ra-query-binary` to provide it) and the host-built
/// auditor-claude-runner at /usr/local/bin/auditor-claude-runner (operator
/// runs `just auditor-claude-runner-binary`); the runner's `which_on_path`
/// guard tolerates a missing ra-query by emitting `ra_query_unavailable`
/// and skipping the relevant stage.
fn build_auditor_claude_agentry_role(home: &str) -> AgentRole {
    AgentRole {
        name: RoleName("auditor-claude-agentry".into()),
        version: 1,
        model: None,
        system_prompt: None,
        image: "docker.io/library/rust:1.93".into(),
        substrate_class: SubstrateClass::Podman,
        package_manager: PackageManager::Apt,
        entrypoint_script: "#!/bin/sh\nexec /usr/local/bin/auditor-claude-runner\n".into(),
        exitpoint_script: None,
        binaries: vec![],
        mcp_servers: vec![],
        tool_allowlist: ToolAllowlist(vec![]),
        allowed_tools: None,
        permit_scope: PermitScope(vec![
            "fs:read:/workspace/**".into(),
            "fs:write:/workspace/**".into(),
            "net:allow:agentry-sccache-redis".into(),
            "net:allow:static.rust-lang.org".into(),
            "net:allow:crates.io".into(),
            "net:allow:index.crates.io".into(),
            "net:allow:static.crates.io".into(),
        ]),
        passthru_env: vec![],
        extra_bootstrap: vec![
            "rustup component add rustfmt clippy || true".into(),
            "rustup toolchain install nightly --profile minimal || true".into(),
            "cargo +nightly install cargo-udeps --locked --quiet || true".into(),
        ],
        mounts: vec![
            Mount {
                source: format!("{home}/.local/bin/ra-query"),
                target: "/usr/local/bin/ra-query".into(),
                readonly: true,
            },
            Mount {
                source: format!("{home}/.local/bin/auditor-claude-runner"),
                target: "/usr/local/bin/auditor-claude-runner".into(),
                readonly: true,
            },
        ],
        workspace_mount: Some(WorkspaceMount {
            container_path: "/workspace".into(),
            readonly: false,
        }),
        sccache: true,
    }
}

/// Verifier role for the `agentry-verify-v0` team. Despite the `claude` in
/// the name — kept for symmetry with the other agentry-* roles — the
/// verifier never invokes claude; it just runs `success_criteria` as a
/// shell command on a read-only snapshot of the workspace. Strictest
/// permits in the registry: fs:read on /workspace, fs:write on /tmp only,
/// no net, no git, no claude.
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
        allowed_tools: None,
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

/// PR rebaser role for the substrate auto-rebaser (#137). Triggered by a
/// future ci-watcher chain-trigger when a PR's `mergeable` flag flips
/// false because `develop` advanced past the PR's base. Reads the PR
/// coordinates from the brief payload, rebases the branch onto
/// `origin/<base>`, and either force-pushes the rebased head or surfaces
/// each conflict as a `Finding` so the coder can re-roll. Bash-only
/// (`git` + `curl` + `jq` from apk), no LLM. Brief 137a — registered but
/// unused until brief 137b wires it into ci-watcher's polling loop.
fn build_pr_rebaser_agentry_role(_home: &str) -> AgentRole {
    AgentRole {
        name: RoleName("pr-rebaser-agentry".into()),
        version: 1,
        model: None,
        system_prompt: None,
        // Mirrors ci-watcher's image — both are bash-only forge interactors.
        image: ALPINE.into(),
        substrate_class: SubstrateClass::Podman,
        package_manager: PackageManager::Apk,
        entrypoint_script: format!("{BASH_PRELUDE}{PR_REBASER_AGENTRY_SCRIPT}"),
        exitpoint_script: None,
        binaries: vec![
            "git".into(),
            "curl".into(),
            "jq".into(),
            "ca-certificates".into(),
        ],
        mcp_servers: vec![],
        tool_allowlist: ToolAllowlist(vec!["git".into(), "curl".into()]),
        allowed_tools: None,
        permit_scope: PermitScope(vec![
            "fs:read:/workspace/**".into(),
            "fs:write:/workspace/**".into(),
            "net:allow:agency.lab".into(),
            "forge:write:yg/*".into(),
        ]),
        passthru_env: vec!["GITEA_TOKEN".into()],
        extra_bootstrap: vec![],
        mounts: vec![],
        // Rebaser mutates /workspace/.git during fetch/checkout/rebase/push,
        // so the workspace mount must be writable (parallel to shipper-agentry).
        workspace_mount: Some(WorkspaceMount {
            container_path: "/workspace".into(),
            readonly: false,
        }),
        sccache: false,
    }
}

/// Pre-flight criterion analyser. Runs `success_criteria` against the current
/// tip and reports the baseline value, plus heuristic smell-tests for
/// obviously-broken criteria (issue #84). Read-only on the workspace; surfaces
/// signal via `Finding` events. Does not gate — gating is the planner's job in
/// brief 84b.
const PREFLIGHT_CRITERION_AGENTRY_SCRIPT: &str = r##"#!/usr/bin/env bash
set -uo pipefail
bundle="$(cat)"
criterion=$(jq -r '.brief.payload.success_criteria // ""' <<<"$bundle")
target_repo=$(jq -r '.brief.payload.target_repo // ""' <<<"$bundle")

if [ -z "$criterion" ]; then
    emit_event '{"error":"preflight-criterion missing success_criteria in payload"}'
    emit_done "failed"; exit 0
fi

# Split on the FIRST occurrence of " : " (space-colon-space).
case "$criterion" in
    *' : '*) ;;
    *)
        emit_event "$(jq -nc --arg c "$criterion" '{error:"success_criteria missing space-colon-space separator",criterion:$c}')"
        emit_done "failed"; exit 0
        ;;
esac
cmd="${criterion%% : *}"
expected_raw="${criterion#* : }"
expected=$(printf '%s' "$expected_raw" | sed 's/^[[:space:]]*//;s/[[:space:]]*$//')

cd /workspace || { emit_event '{"error":"cd /workspace failed"}'; emit_done "failed"; exit 0; }
emit_event "$(jq -nc --arg c "$cmd" --arg e "$expected" --arg t "$target_repo" '{msg:"running preflight criterion",cmd:$c,expected:$e,target_repo:$t}')"

stdout_file=$(mktemp)
stderr_file=$(mktemp)
exit_code=0
bash -c "$cmd" >"$stdout_file" 2>"$stderr_file" || exit_code=$?
baseline_raw=$(cat "$stdout_file")
baseline=$(printf '%s' "$baseline_raw" | sed 's/^[[:space:]]*//;s/[[:space:]]*$//')
err_tail=$(tail -c 4096 "$stderr_file")

if [ "$baseline" = "$expected" ]; then
    is_match=true
else
    is_match=false
fi

emit_event "$(jq -nc --arg b "$baseline" --arg e "$expected" --argjson m "$is_match" --argjson rc "$exit_code" --arg err "$err_tail" '{msg:"baseline_match",baseline:$b,expected:$e,match:$m,exit_code:$rc,stderr_tail:$err}')"

# Smell heuristics — conservative; surface signal, don't reject. Operator (or
# planner in 84b) decides what to do. False positives on warnings are fine;
# false negatives let real broken criteria slip through, which is the bug.

# Smell 1: huge baseline vs zero expected on a wc -l style count.
if printf '%s' "$expected" | grep -qE '^[0-9]+$' \
    && printf '%s' "$baseline" | grep -qE '^[0-9]+$' \
    && [ "$expected" = "0" ] \
    && [ "$baseline" -gt 100 ] \
    && printf '%s' "$cmd" | grep -qF 'wc -l'; then
    emit_finding warn preflight-criterion criterion-quality \
        "criterion baseline ($baseline) is far from expected ($expected) — likely false positives if filter is naive"
fi

# Smell 2: canonical broken filter from #51.
if printf '%s' "$cmd" | grep -qF "grep -v 'mod tests'"; then
    emit_finding warn preflight-criterion criterion-quality \
        "grep -v 'mod tests' filters lines containing literal text but not #[cfg(test)] scopes; use a Rust-aware tool like ra-query or cfdb instead"
fi

# Smell 3: counting via wc -l without a #[cfg(test)] filter.
if printf '%s' "$cmd" | grep -qF 'wc -l' \
    && ! printf '%s' "$cmd" | grep -qF '#[cfg(test)]'; then
    emit_finding warn preflight-criterion criterion-quality \
        "counting via wc -l without test-scope exclusion may include test code"
fi

emit_done "shipped"
"##;

/// Build the preflight-criterion-agentry role (issue #84). Runs the brief's
/// `success_criteria` against the current workspace tip and reports the
/// baseline + heuristic smells; does not gate. Brief 84b wires the planner to
/// consume the baseline; until then the role is invoked manually for
/// diagnosis.
fn build_preflight_criterion_agentry_role(home: &str) -> AgentRole {
    let _ = home;
    AgentRole {
        name: RoleName("preflight-criterion-agentry".into()),
        version: 1,
        model: None,
        system_prompt: None,
        image: ALPINE.into(),
        substrate_class: SubstrateClass::Podman,
        package_manager: PackageManager::Apk,
        entrypoint_script: format!("{BASH_PRELUDE}{PREFLIGHT_CRITERION_AGENTRY_SCRIPT}"),
        exitpoint_script: None,
        binaries: vec![
            "bash".into(),
            "ripgrep".into(),
            "jq".into(),
            "coreutils".into(),
        ],
        mcp_servers: vec![],
        tool_allowlist: ToolAllowlist(vec!["bash".into(), "rg".into(), "grep".into(), "wc".into()]),
        allowed_tools: None,
        permit_scope: PermitScope(vec!["fs:read:/workspace/**".into()]),
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
        allowed_tools: None,
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
        roles: vec![RoleRef {
            name: echo.name.clone(),
            version: echo.version,
        }],
        message_graph: Vec::<MessageEdge>::new(),
        terminal_role: RoleRef {
            name: echo.name.clone(),
            version: echo.version,
        },
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
        allowed_tools: None,
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
        roles: vec![RoleRef {
            name: naughty.name.clone(),
            version: naughty.version,
        }],
        message_graph: Vec::<MessageEdge>::new(),
        terminal_role: RoleRef {
            name: naughty.name.clone(),
            version: naughty.version,
        },
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
        allowed_tools: None,
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
        allowed_tools: None,
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
        roles: vec![
            RoleRef {
                name: speaker.name.clone(),
                version: speaker.version,
            },
            RoleRef {
                name: listener.name.clone(),
                version: listener.version,
            },
        ],
        message_graph: vec![MessageEdge {
            from: RoleRef {
                name: speaker.name.clone(),
                version: speaker.version,
            },
            to: RoleRef {
                name: listener.name.clone(),
                version: listener.version,
            },
            permit_overrides_from: None,
            rework_target: None,
        }],
        terminal_role: RoleRef {
            name: listener.name.clone(),
            version: listener.version,
        },
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
        allowed_tools: None,
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
        roles: vec![RoleRef {
            name: grok_echo.name.clone(),
            version: grok_echo.version,
        }],
        message_graph: vec![],
        terminal_role: RoleRef {
            name: grok_echo.name.clone(),
            version: grok_echo.version,
        },
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
        allowed_tools: None,
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
        roles: vec![RoleRef {
            name: claude_echo.name.clone(),
            version: claude_echo.version,
        }],
        message_graph: vec![],
        terminal_role: RoleRef {
            name: claude_echo.name.clone(),
            version: claude_echo.version,
        },
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
        allowed_tools: None,
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
        allowed_tools: None,
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
        roles: vec![
            RoleRef {
                name: synthesizer.name.clone(),
                version: synthesizer.version,
            },
            RoleRef {
                name: narrowed_coder.name.clone(),
                version: narrowed_coder.version,
            },
        ],
        message_graph: vec![MessageEdge {
            from: RoleRef {
                name: synthesizer.name.clone(),
                version: synthesizer.version,
            },
            to: RoleRef {
                name: narrowed_coder.name.clone(),
                version: narrowed_coder.version,
            },
            permit_overrides_from: Some("permit_overrides".into()),
            rework_target: None,
        }],
        terminal_role: RoleRef {
            name: narrowed_coder.name.clone(),
            version: narrowed_coder.version,
        },
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
        allowed_tools: None,
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
        roles: vec![RoleRef {
            name: shipper.name.clone(),
            version: shipper.version,
        }],
        message_graph: vec![],
        terminal_role: RoleRef {
            name: shipper.name.clone(),
            version: shipper.version,
        },
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
        allowed_tools: None,
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
        roles: vec![RoleRef {
            name: workspace_probe.name.clone(),
            version: workspace_probe.version,
        }],
        message_graph: Vec::<MessageEdge>::new(),
        terminal_role: RoleRef {
            name: workspace_probe.name.clone(),
            version: workspace_probe.version,
        },
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
        allowed_tools: None,
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
        roles: vec![RoleRef {
            name: sccache_probe.name.clone(),
            version: sccache_probe.version,
        }],
        message_graph: Vec::<MessageEdge>::new(),
        terminal_role: RoleRef {
            name: sccache_probe.name.clone(),
            version: sccache_probe.version,
        },
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
        allowed_tools: None,
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
        roles: vec![RoleRef {
            name: timeout_probe.name.clone(),
            version: timeout_probe.version,
        }],
        message_graph: Vec::<MessageEdge>::new(),
        terminal_role: RoleRef {
            name: timeout_probe.name.clone(),
            version: timeout_probe.version,
        },
        max_retries: 0,
    };

    // ---- agentry-self-host-v0 team (cutoff trigger) ----
    // Coder clones, calls claude, runs acceptance, commits locally.
    // Reviewer re-runs acceptance in isolation on the coder's workspace.
    // Shipper pushes the branch and opens a PR on the forge.
    // Ci-watcher polls forge CI on the PR's head sha and merges on green.
    let home = std::env::var("HOME").unwrap_or_else(|_| "/var/home/yg".into());
    let coder_claude_agentry = RoleRef {
        name: RoleName("coder-claude-agentry".into()),
        version: 1,
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
        allowed_tools: None,
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
    let reviewer_claude_agentry = RoleRef {
        name: RoleName("reviewer-claude-agentry".into()),
        version: 1,
    };
    let ac_verifier_claude_agentry = RoleRef {
        name: RoleName("ac-verifier-claude-agentry".into()),
        version: 1,
    };
    let ac_verifier_gemini_agentry = RoleRef {
        name: RoleName("ac-verifier-gemini-agentry".into()),
        version: 1,
    };
    let ac_verifier_grok_agentry = RoleRef {
        name: RoleName("ac-verifier-grok-agentry".into()),
        version: 1,
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
        allowed_tools: None,
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
        binaries: vec!["curl".into(), "jq".into(), "ca-certificates".into()],
        mcp_servers: vec![],
        tool_allowlist: ToolAllowlist(vec![]),
        allowed_tools: None,
        permit_scope: PermitScope(vec![
            "fs:write:/workspace/**".into(),
            "net:allow:agency.lab".into(),
            "forge:write:yg/agentry".into(),
        ]),
        passthru_env: vec!["GITEA_TOKEN".into()],
        extra_bootstrap: vec![],
        mounts: vec![],
        // Brief 137b: ci-watcher writes /workspace/pr_rebaser_brief.json
        // when chain-triggering pr-rebaser-agentry on a `mergeable: false`
        // PR; the daemon's chain-trigger reads that file off-host once
        // ci-watcher ships, so the workspace mount must be writable.
        workspace_mount: Some(WorkspaceMount {
            container_path: "/workspace".into(),
            readonly: false,
        }),
        sccache: false,
    };
    // pr-rebaser-agentry — substrate auto-rebaser (#137). Brief 137b wires
    // ci-watcher to chain-trigger a brief on this single-role topology when
    // a PR's `mergeable` flag flips false; the rebaser force-pushes the
    // rebased branch (or surfaces conflicts as findings).
    let pr_rebaser_agentry = build_pr_rebaser_agentry_role(&home);
    let agentry_pr_rebaser_v0 = TeamTopology {
        name: TeamName("agentry-pr-rebaser-v0".into()),
        version: 1,
        roles: vec![RoleRef {
            name: pr_rebaser_agentry.name.clone(),
            version: pr_rebaser_agentry.version,
        }],
        message_graph: Vec::<MessageEdge>::new(),
        terminal_role: RoleRef {
            name: pr_rebaser_agentry.name.clone(),
            version: pr_rebaser_agentry.version,
        },
        max_retries: 0,
    };
    // ---- agentry-null-v0 team (shake-down for role-introduction pipeline) ----
    // null-agent emits one event then `done shipped`. Zero work — exercises
    // the reviewer-claude Role-spec audit clause, permit broker, and spawner
    // teardown on a deliberately minimal-permitted, well-formed role before
    // real planner roles (archaeologist, planner, verifier) land.
    // First role ported from bash to Rust under EPIC #161 B0. The legacy
    // bash heredoc (4 lines: emit_event + emit_done) becomes a Rust binary
    // built from `agentry-role-runtime`. The 1-line shell wrapper is
    // acceptable bootstrap glue per the refined #161 rule.
    let null_agent_agentry = RoleRef {
        name: RoleName("null-agent-agentry".into()),
        version: 1,
    };
    let agentry_null_v0 = TeamTopology {
        name: TeamName("agentry-null-v0".into()),
        version: 1,
        roles: vec![RoleRef {
            name: null_agent_agentry.name.clone(),
            version: null_agent_agentry.version,
        }],
        message_graph: Vec::<MessageEdge>::new(),
        terminal_role: RoleRef {
            name: null_agent_agentry.name.clone(),
            version: null_agent_agentry.version,
        },
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
        roles: vec![RoleRef {
            name: archaeologist_claude_agentry.name.clone(),
            version: archaeologist_claude_agentry.version,
        }],
        message_graph: Vec::<MessageEdge>::new(),
        terminal_role: RoleRef {
            name: archaeologist_claude_agentry.name.clone(),
            version: archaeologist_claude_agentry.version,
        },
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
            RoleRef {
                name: archaeologist_claude_agentry.name.clone(),
                version: archaeologist_claude_agentry.version,
            },
            RoleRef {
                name: planner_claude_agentry.name.clone(),
                version: planner_claude_agentry.version,
            },
        ],
        // discovery.json is on the shared workspace, not message-borne — the
        // edge exists only to gate the planner on the archaeologist shipping.
        message_graph: vec![MessageEdge {
            from: RoleRef {
                name: archaeologist_claude_agentry.name.clone(),
                version: archaeologist_claude_agentry.version,
            },
            to: RoleRef {
                name: planner_claude_agentry.name.clone(),
                version: planner_claude_agentry.version,
            },
            permit_overrides_from: None,
            rework_target: None,
        }],
        terminal_role: RoleRef {
            name: planner_claude_agentry.name.clone(),
            version: planner_claude_agentry.version,
        },
        max_retries: 0,
    };

    // ---- agentry-verify-v0 team (DOL verifier — runs success_criteria) ----
    // Daemon-Orchestrated Lifecycle: when all children of a meta-brief reach
    // terminal verdict, the daemon auto-dispatches a verifier brief that runs
    // the meta-brief's success_criteria. The verifier's verdict composes with
    // the children's verdicts to produce the meta-brief's terminal verdict.
    let verifier_claude_agentry = build_verifier_claude_agentry_role();
    let preflight_criterion_agentry = build_preflight_criterion_agentry_role(&home);
    let agentry_verify_v0 = TeamTopology {
        name: TeamName("agentry-verify-v0".into()),
        version: 1,
        roles: vec![RoleRef {
            name: verifier_claude_agentry.name.clone(),
            version: verifier_claude_agentry.version,
        }],
        message_graph: Vec::<MessageEdge>::new(),
        terminal_role: RoleRef {
            name: verifier_claude_agentry.name.clone(),
            version: verifier_claude_agentry.version,
        },
        max_retries: 0,
    };

    let agentry_self_host_v0 = TeamTopology {
        name: TeamName("agentry-self-host-v0".into()),
        version: 1,
        roles: vec![
            RoleRef {
                name: coder_claude_agentry.name.clone(),
                version: coder_claude_agentry.version,
            },
            RoleRef {
                name: ac_verifier_claude_agentry.name.clone(),
                version: ac_verifier_claude_agentry.version,
            },
            RoleRef {
                name: ac_verifier_gemini_agentry.name.clone(),
                version: ac_verifier_gemini_agentry.version,
            },
            RoleRef {
                name: ac_verifier_grok_agentry.name.clone(),
                version: ac_verifier_grok_agentry.version,
            },
            RoleRef {
                name: reviewer_mechanical_agentry.name.clone(),
                version: reviewer_mechanical_agentry.version,
            },
            RoleRef {
                name: reviewer_claude_agentry.name.clone(),
                version: reviewer_claude_agentry.version,
            },
            RoleRef {
                name: shipper_agentry.name.clone(),
                version: shipper_agentry.version,
            },
            RoleRef {
                name: ci_watcher_agentry.name.clone(),
                version: ci_watcher_agentry.version,
            },
        ],
        // Rework loop enabled — max_retries=2 gives the coder two chances to
        // fix findings emitted by the reviewer before the team resolves Failed.
        message_graph: vec![
            // ORDERING INVARIANT: coder→reviewer edges are listed BEFORE
            // ac-verifier-{claude,gemini,grok}→reviewer edges so the daemon's
            // `team.incoming(reviewer).first()` rework lookup rewinds to the
            // coder, not to a (non-corrective) ac-verifier. Do not reorder.
            MessageEdge {
                from: RoleRef {
                    name: coder_claude_agentry.name.clone(),
                    version: coder_claude_agentry.version,
                },
                to: RoleRef {
                    name: reviewer_mechanical_agentry.name.clone(),
                    version: reviewer_mechanical_agentry.version,
                },
                permit_overrides_from: None,
                rework_target: None,
            },
            MessageEdge {
                from: RoleRef {
                    name: coder_claude_agentry.name.clone(),
                    version: coder_claude_agentry.version,
                },
                to: RoleRef {
                    name: reviewer_claude_agentry.name.clone(),
                    version: reviewer_claude_agentry.version,
                },
                permit_overrides_from: None,
                rework_target: None,
            },
            // Coder fans out to ac-verifier as well; ac-verifier short-circuits
            // failed-AC reworks BEFORE reviewer-claude is spent.
            MessageEdge {
                from: RoleRef {
                    name: coder_claude_agentry.name.clone(),
                    version: coder_claude_agentry.version,
                },
                to: RoleRef {
                    name: ac_verifier_claude_agentry.name.clone(),
                    version: ac_verifier_claude_agentry.version,
                },
                permit_overrides_from: None,
                rework_target: None,
            },
            // Dual-inbound trick: ac-verifier also signals each reviewer so the
            // sequential flow holds, but rework still rewinds to coder above.
            MessageEdge {
                from: RoleRef {
                    name: ac_verifier_claude_agentry.name.clone(),
                    version: ac_verifier_claude_agentry.version,
                },
                to: RoleRef {
                    name: reviewer_mechanical_agentry.name.clone(),
                    version: reviewer_mechanical_agentry.version,
                },
                permit_overrides_from: None,
                rework_target: None,
            },
            MessageEdge {
                from: RoleRef {
                    name: ac_verifier_claude_agentry.name.clone(),
                    version: ac_verifier_claude_agentry.version,
                },
                to: RoleRef {
                    name: reviewer_claude_agentry.name.clone(),
                    version: reviewer_claude_agentry.version,
                },
                permit_overrides_from: None,
                rework_target: None,
            },
            // Parallel ac-verifier siblings: gemini + grok fan out from the
            // coder and signal both reviewers. Any verifier emitting failed
            // rewinds to the coder before reviewers spawn (fail-closed).
            MessageEdge {
                from: RoleRef {
                    name: coder_claude_agentry.name.clone(),
                    version: coder_claude_agentry.version,
                },
                to: RoleRef {
                    name: ac_verifier_gemini_agentry.name.clone(),
                    version: ac_verifier_gemini_agentry.version,
                },
                permit_overrides_from: None,
                rework_target: None,
            },
            MessageEdge {
                from: RoleRef {
                    name: coder_claude_agentry.name.clone(),
                    version: coder_claude_agentry.version,
                },
                to: RoleRef {
                    name: ac_verifier_grok_agentry.name.clone(),
                    version: ac_verifier_grok_agentry.version,
                },
                permit_overrides_from: None,
                rework_target: None,
            },
            MessageEdge {
                from: RoleRef {
                    name: ac_verifier_gemini_agentry.name.clone(),
                    version: ac_verifier_gemini_agentry.version,
                },
                to: RoleRef {
                    name: reviewer_mechanical_agentry.name.clone(),
                    version: reviewer_mechanical_agentry.version,
                },
                permit_overrides_from: None,
                rework_target: None,
            },
            MessageEdge {
                from: RoleRef {
                    name: ac_verifier_gemini_agentry.name.clone(),
                    version: ac_verifier_gemini_agentry.version,
                },
                to: RoleRef {
                    name: reviewer_claude_agentry.name.clone(),
                    version: reviewer_claude_agentry.version,
                },
                permit_overrides_from: None,
                rework_target: None,
            },
            MessageEdge {
                from: RoleRef {
                    name: ac_verifier_grok_agentry.name.clone(),
                    version: ac_verifier_grok_agentry.version,
                },
                to: RoleRef {
                    name: reviewer_mechanical_agentry.name.clone(),
                    version: reviewer_mechanical_agentry.version,
                },
                permit_overrides_from: None,
                rework_target: None,
            },
            MessageEdge {
                from: RoleRef {
                    name: ac_verifier_grok_agentry.name.clone(),
                    version: ac_verifier_grok_agentry.version,
                },
                to: RoleRef {
                    name: reviewer_claude_agentry.name.clone(),
                    version: reviewer_claude_agentry.version,
                },
                permit_overrides_from: None,
                rework_target: None,
            },
            // Mechanical reviewer signals shipper only for sequential flow; no
            // data payload carried on this edge.
            MessageEdge {
                from: RoleRef {
                    name: reviewer_mechanical_agentry.name.clone(),
                    version: reviewer_mechanical_agentry.version,
                },
                to: RoleRef {
                    name: shipper_agentry.name.clone(),
                    version: shipper_agentry.version,
                },
                permit_overrides_from: None,
                rework_target: None,
            },
            // Claude reviewer also signals shipper.
            MessageEdge {
                from: RoleRef {
                    name: reviewer_claude_agentry.name.clone(),
                    version: reviewer_claude_agentry.version,
                },
                to: RoleRef {
                    name: shipper_agentry.name.clone(),
                    version: shipper_agentry.version,
                },
                permit_overrides_from: None,
                rework_target: None,
            },
            // Shipper routes head_sha + pr_number to ci-watcher via Message event.
            MessageEdge {
                from: RoleRef {
                    name: shipper_agentry.name.clone(),
                    version: shipper_agentry.version,
                },
                to: RoleRef {
                    name: ci_watcher_agentry.name.clone(),
                    version: ci_watcher_agentry.version,
                },
                permit_overrides_from: None,
                rework_target: None,
            },
        ],
        terminal_role: RoleRef {
            name: ci_watcher_agentry.name.clone(),
            version: ci_watcher_agentry.version,
        },
        max_retries: 2,
    };

    let agentry_bugfix_v0 = TeamTopology {
        name: TeamName("agentry-bugfix-v0".into()),
        version: 1,
        roles: vec![
            RoleRef {
                name: coder_claude_agentry.name.clone(),
                version: coder_claude_agentry.version,
            },
            RoleRef {
                name: reviewer_mechanical_agentry.name.clone(),
                version: reviewer_mechanical_agentry.version,
            },
            RoleRef {
                name: shipper_agentry.name.clone(),
                version: shipper_agentry.version,
            },
            RoleRef {
                name: ci_watcher_agentry.name.clone(),
                version: ci_watcher_agentry.version,
            },
        ],
        message_graph: vec![
            MessageEdge {
                from: RoleRef {
                    name: coder_claude_agentry.name.clone(),
                    version: coder_claude_agentry.version,
                },
                to: RoleRef {
                    name: reviewer_mechanical_agentry.name.clone(),
                    version: reviewer_mechanical_agentry.version,
                },
                permit_overrides_from: None,
                rework_target: None,
            },
            MessageEdge {
                from: RoleRef {
                    name: reviewer_mechanical_agentry.name.clone(),
                    version: reviewer_mechanical_agentry.version,
                },
                to: RoleRef {
                    name: shipper_agentry.name.clone(),
                    version: shipper_agentry.version,
                },
                permit_overrides_from: None,
                rework_target: None,
            },
            MessageEdge {
                from: RoleRef {
                    name: shipper_agentry.name.clone(),
                    version: shipper_agentry.version,
                },
                to: RoleRef {
                    name: ci_watcher_agentry.name.clone(),
                    version: ci_watcher_agentry.version,
                },
                permit_overrides_from: None,
                rework_target: None,
            },
        ],
        terminal_role: RoleRef {
            name: ci_watcher_agentry.name.clone(),
            version: ci_watcher_agentry.version,
        },
        max_retries: 2,
    };

    let agentry_spec_edit_v0 = TeamTopology {
        name: TeamName("agentry-spec-edit-v0".into()),
        version: 1,
        roles: vec![
            RoleRef {
                name: coder_claude_agentry.name.clone(),
                version: coder_claude_agentry.version,
            },
            RoleRef {
                name: shipper_agentry.name.clone(),
                version: shipper_agentry.version,
            },
            RoleRef {
                name: ci_watcher_agentry.name.clone(),
                version: ci_watcher_agentry.version,
            },
        ],
        message_graph: vec![
            MessageEdge {
                from: RoleRef {
                    name: coder_claude_agentry.name.clone(),
                    version: coder_claude_agentry.version,
                },
                to: RoleRef {
                    name: shipper_agentry.name.clone(),
                    version: shipper_agentry.version,
                },
                permit_overrides_from: None,
                rework_target: None,
            },
            MessageEdge {
                from: RoleRef {
                    name: shipper_agentry.name.clone(),
                    version: shipper_agentry.version,
                },
                to: RoleRef {
                    name: ci_watcher_agentry.name.clone(),
                    version: ci_watcher_agentry.version,
                },
                permit_overrides_from: None,
                rework_target: None,
            },
        ],
        terminal_role: RoleRef {
            name: ci_watcher_agentry.name.clone(),
            version: ci_watcher_agentry.version,
        },
        max_retries: 1,
    };

    let auditor_claude_agentry = build_auditor_claude_agentry_role(&home);
    let agentry_self_audit_v0 = TeamTopology {
        name: TeamName("agentry-self-audit-v0".into()),
        version: 1,
        roles: vec![RoleRef {
            name: auditor_claude_agentry.name.clone(),
            version: auditor_claude_agentry.version,
        }],
        message_graph: Vec::<MessageEdge>::new(),
        terminal_role: RoleRef {
            name: auditor_claude_agentry.name.clone(),
            version: auditor_claude_agentry.version,
        },
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
    redis_io::save_role(&mut conn, &reviewer_mechanical_agentry).await?;
    redis_io::save_role(&mut conn, &shipper_agentry).await?;
    redis_io::save_role(&mut conn, &ci_watcher_agentry).await?;
    redis_io::save_role(&mut conn, &pr_rebaser_agentry).await?;
    redis_io::save_team(&mut conn, &agentry_pr_rebaser_v0).await?;
    redis_io::save_team(&mut conn, &agentry_self_host_v0).await?;
    redis_io::save_team(&mut conn, &agentry_bugfix_v0).await?;
    redis_io::save_team(&mut conn, &agentry_spec_edit_v0).await?;
    redis_io::save_role(&mut conn, &auditor_claude_agentry).await?;
    redis_io::save_team(&mut conn, &agentry_self_audit_v0).await?;
    redis_io::save_team(&mut conn, &agentry_null_v0).await?;
    redis_io::save_role(&mut conn, &archaeologist_claude_agentry).await?;
    redis_io::save_team(&mut conn, &agentry_discovery_v0).await?;
    redis_io::save_role(&mut conn, &planner_claude_agentry).await?;
    redis_io::save_team(&mut conn, &agentry_planner_v0).await?;
    redis_io::save_role(&mut conn, &verifier_claude_agentry).await?;
    redis_io::save_team(&mut conn, &agentry_verify_v0).await?;
    redis_io::save_role(&mut conn, &preflight_criterion_agentry).await?;

    let roles_dir = seed_roles_dir();
    if roles_dir.exists() {
        let loaded = role_dir_loader::load_roles_from_dir(&mut conn, &roles_dir).await?;
        tracing::info!(
            count = loaded.len(),
            dir = %roles_dir.display(),
            "loaded JSON role catalog from seed directory",
        );
    } else {
        tracing::debug!(
            dir = %roles_dir.display(),
            "seed roles directory absent — skipping JSON role load",
        );
    }

    tracing::info!(
        "seeded: roles [echo, workspace-probe, sccache-probe, timeout-probe, naughty, speaker, listener, grok-echo, claude-echo, synthesizer, narrowed-coder, shipper, coder-claude-agentry, ac-verifier-claude-agentry, ac-verifier-gemini-agentry, ac-verifier-grok-agentry, reviewer-mechanical-agentry, shipper-agentry, ci-watcher-agentry, pr-rebaser-agentry, reviewer-claude-agentry, auditor-claude-agentry, null-agent-agentry, archaeologist-claude-agentry, planner-claude-agentry, verifier-claude-agentry] (inline entrypoint scripts); teams [echo, workspace-probe, sccache-probe, timeout-probe, naughty, speaker-listener, grok-echo, claude-echo, narrowed-team, shipper-solo-team, agentry-self-host-v0, agentry-pr-rebaser-v0, agentry-self-audit-v0, agentry-null-v0, agentry-discovery-v0, agentry-planner-v0, agentry-verify-v0]"
    );
    Ok(())
}

/// Resolve the directory containing role JSON files for seed-time loading.
///
/// The default is `<workspace_root>/seed/roles`, computed by walking up two
/// levels from `CARGO_MANIFEST_DIR` (which points at the
/// `crates/orchestrator-runtime` directory). The env var
/// `AGENTRY_SEED_ROLES_DIR` overrides it for substrates that ship the
/// catalog elsewhere.
fn seed_roles_dir() -> PathBuf {
    if let Ok(override_path) = std::env::var("AGENTRY_SEED_ROLES_DIR") {
        return PathBuf::from(override_path);
    }
    let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    // crates/orchestrator-runtime → workspace root.
    let workspace_root = manifest
        .parent()
        .and_then(|p| p.parent())
        .map(PathBuf::from)
        .unwrap_or(manifest);
    workspace_root.join("seed").join("roles")
}
