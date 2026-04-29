//! Seed the Redis registry with the agent roles and team topologies.
//!
//! Each role carries its entrypoint as an inline bash script (no per-agent
//! Containerfile). The spawner picks a stock public base image, installs the
//! role's declared `binaries` via `package_manager`, then execs the script.
//!
//! Idempotent: overwrites existing records with current definitions.

use crate::{redis_io, role_dir_loader, Config, Result};
use orchestrator_types::{
    AgentRole, MessageEdge, Mount, PackageManager, PermitScope, RoleName, SubstrateClass, TeamName,
    TeamTopology, ToolAllowlist, WorkspaceMount,
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

/// Container base image for roles that bind-mount host-built runner
/// binaries (`reviewer-claude-runner`, `ac-verifier-runner`, etc.) and have
/// no Rust toolchain in the image itself.
///
/// Must have glibc ≥ the host build's compile-target glibc, otherwise the
/// dynamic linker fails with `version 'GLIBC_X.Y' not found` BEFORE `main()`
/// runs — the runner's `DoneGuard` never registers, no events emit, and the
/// role's verdict is synthesised from a bare exit code (issue #175).
///
/// `bookworm-slim` (glibc 2.36) was insufficient for binaries built on
/// Fedora 43 / glibc 2.42 toolchains, which emit GLIBC_2.39 symbols.
/// `trixie-slim` (glibc 2.41) covers the current host fleet; bump if/when
/// the host toolchain's glibc-target passes 2.41.
///
/// Roles whose image already ships a Rust toolchain (e.g. coder uses
/// `rust:1.93`, which is debian trixie based) need no change here — they
/// inherit a compatible glibc from the toolchain's own base.
const RUNNER_HOST_IMAGE: &str = "docker.io/library/debian:trixie-slim";

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
///
/// This role is the FIRST port of the EPIC #161 bash → Rust migration. The
/// behaviour now lives in `crates/agentry-role-runtime/src/bin/null_agent.rs`.
/// The role's `entrypoint_script` is a one-line shell wrapper that execs
/// `/usr/local/bin/null-agent` (bind-mounted from the host).
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
        image: RUNNER_HOST_IMAGE.into(),
        substrate_class: SubstrateClass::Podman,
        package_manager: PackageManager::Apt,
        // Bootstrap glue only: exec the bind-mounted Rust runner. Workspace
        // diff capture, optional ra-query pre-pass, claude streaming, and
        // finding emission live in the binary
        // (`crates/agentry-role-runtime/src/bin/reviewer_claude_runner.rs`).
        // EPIC #161 Wave 1.4 — replaces `BASH_PRELUDE +
        // REVIEWER_CLAUDE_AGENTRY_SCRIPT`.
        entrypoint_script: "#!/bin/sh\nexec /usr/local/bin/reviewer-claude-runner\n".into(),
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
            Mount {
                source: format!("{home}/.local/bin/reviewer-claude-runner"),
                target: "/usr/local/bin/reviewer-claude-runner".into(),
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
        image: RUNNER_HOST_IMAGE.into(),
        substrate_class: SubstrateClass::Podman,
        package_manager: PackageManager::Apt,
        // Bootstrap glue only: exec the bind-mounted Rust runner.
        // Workspace prep + provider invocation lives in the binary
        // (`crates/agentry-role-runtime/src/bin/ac_verifier_runner.rs`).
        // EPIC #161 Wave 1.3 — replaces `BASH_PRELUDE +
        // AC_VERIFIER_CLAUDE_AGENTRY_SCRIPT`.
        entrypoint_script: "#!/bin/sh\nexec /usr/local/bin/ac-verifier-runner --provider claude\n"
            .into(),
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
            Mount {
                source: format!("{home}/.local/bin/ac-verifier-runner"),
                target: "/usr/local/bin/ac-verifier-runner".into(),
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

/// Build the ac-verifier-gemini-agentry role. Sibling of
/// `ac-verifier-claude-agentry` (brief 2 of #134). Same shape: reads the
/// brief's `acceptance_criteria` + the coder's git diff, asks Gemini for a
/// per-AC verdict, emits one blocker `Finding` per failed AC. Brief 5 of #134
/// wired this role into `agentry-self-host-v0` as a parallel sibling of the
/// claude variant; coder fans out to all three providers and any verifier
/// emitting failed rewinds to the coder before reviewers spawn.
fn build_ac_verifier_gemini_agentry_role(home: &str) -> AgentRole {
    AgentRole {
        name: RoleName("ac-verifier-gemini-agentry".into()),
        version: 1,
        model: Some("gemini-3-flash-preview".into()),
        system_prompt: None,
        image: RUNNER_HOST_IMAGE.into(),
        substrate_class: SubstrateClass::Podman,
        package_manager: PackageManager::Apt,
        // Bootstrap glue only: exec the bind-mounted Rust runner. EPIC #161
        // Wave 1.3 — replaces `BASH_PRELUDE + AC_VERIFIER_GEMINI_AGENTRY_SCRIPT`.
        entrypoint_script: "#!/bin/sh\nexec /usr/local/bin/ac-verifier-runner --provider gemini\n"
            .into(),
        exitpoint_script: None,
        binaries: vec!["git".into(), "ca-certificates".into()],
        mcp_servers: vec![],
        tool_allowlist: ToolAllowlist(vec![]),
        permit_scope: PermitScope(vec![
            "fs:read:/workspace/**".into(),
            "net:allow:generativelanguage.googleapis.com".into(),
            "net:allow:agency.lab".into(),
        ]),
        passthru_env: vec!["GEMINI_API_KEY".into()],
        extra_bootstrap: vec![],
        mounts: vec![
            Mount {
                source: format!("{home}/.local/bin/ac-verifier-gemini"),
                target: "/usr/local/bin/ac-verifier-gemini".into(),
                readonly: true,
            },
            Mount {
                source: format!("{home}/.local/bin/ac-verifier-runner"),
                target: "/usr/local/bin/ac-verifier-runner".into(),
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

/// Build the ac-verifier-grok-agentry role. Sibling of the claude variant
/// (brief 4 of #134). Brief 5 of #134 enabled parallel mode by wiring grok
/// alongside claude and gemini in the `agentry-self-host-v0` DAG. Same
/// degradation envelope: missing/empty AC list, missing binary, or invalid
/// grok JSON degrades to `done shipped`.
fn build_ac_verifier_grok_agentry_role(home: &str) -> AgentRole {
    AgentRole {
        name: RoleName("ac-verifier-grok-agentry".into()),
        version: 1,
        model: Some("grok-4-fast".into()),
        system_prompt: None,
        // No rust toolchain — the binary is bind-mounted from the host.
        image: RUNNER_HOST_IMAGE.into(),
        substrate_class: SubstrateClass::Podman,
        package_manager: PackageManager::Apt,
        // Bootstrap glue only: exec the bind-mounted Rust runner. EPIC #161
        // Wave 1.3 — replaces `BASH_PRELUDE + AC_VERIFIER_GROK_AGENTRY_SCRIPT`.
        entrypoint_script: "#!/bin/sh\nexec /usr/local/bin/ac-verifier-runner --provider grok\n"
            .into(),
        exitpoint_script: None,
        binaries: vec!["git".into(), "curl".into(), "ca-certificates".into()],
        mcp_servers: vec![],
        tool_allowlist: ToolAllowlist(vec![]),
        permit_scope: PermitScope(vec![
            "fs:read:/workspace/**".into(),
            "net:allow:api.x.ai".into(),
            "net:allow:agency.lab".into(),
        ]),
        passthru_env: vec!["XAI_API_KEY".into()],
        extra_bootstrap: vec![],
        mounts: vec![
            Mount {
                source: format!("{home}/.local/bin/ac-verifier-grok"),
                target: "/usr/local/bin/ac-verifier-grok".into(),
                readonly: true,
            },
            Mount {
                source: format!("{home}/.local/bin/ac-verifier-runner"),
                target: "/usr/local/bin/ac-verifier-runner".into(),
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
        // EPIC #161 Wave 1.2a + 1.2b — entrypoint AND exitpoint ported to
        // a single Rust runner that owns the full role lifecycle: bundle
        // parse, rework banner, prompt build, claude streaming, then
        // (v0 only) cargo fmt → quality-hygiene → acceptance eval →
        // git add → optional self-review claude soft-fail → optional
        // dead-pub-check → git commit. The merged binary uses the standard
        // DoneGuard pattern; v1+ topologies short-circuit to a best-effort
        // fmt + done shipped (git-operator role handles commit/push).
        entrypoint_script: "#!/bin/sh\nexec /usr/local/bin/coder-claude-runner\n".into(),
        exitpoint_script: None,
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
            Mount {
                source: format!("{home}/.local/bin/ship"),
                target: "/usr/local/bin/ship".into(),
                readonly: true,
            },
            Mount {
                source: format!("{home}/.local/bin/coder-claude-runner"),
                target: "/usr/local/bin/coder-claude-runner".into(),
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

// EPIC #161 Wave 1.3: the three AC_VERIFIER_*_AGENTRY_SCRIPT bash heredocs
// that used to live here have been ported to one Rust runner —
// `crates/agentry-role-runtime/src/bin/ac_verifier_runner.rs` — parameterized
// by `--provider claude|gemini|grok`. The roles' entrypoint_scripts now just
// `exec /usr/local/bin/ac-verifier-runner --provider X`. The runner has its
// own unit-test coverage for AC parsing / degradation envelopes.

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
            cfindings=$(echo "$cfindings" | jq --argjson r "$cout" --arg p "$f" '. + [{file:$p, complex_count:($r.functions|length), result:$r}]')
            total_complex=$((total_complex + ccnt))
        fi
    done < <(find crates -name '*.rs' -not -path '*/tests/*' -not -name 'tests.rs' -not -path '*/target/*')
    emit_event "$(jq -nc --argjson cnt "$total_complex" --arg out "$(echo "$cfindings" | jq -c . | tail -c 8192)" '{msg:"complexity_report",complex_total:$cnt,findings_json_tail:$out}')"
else
    emit_event '{"msg":"ra_query_unavailable_complexity","detail":"skipping complexity stage"}'
fi
if command -v ra-query >/dev/null 2>&1; then
    pfindings='[]'; total_dead_pub=0
    while IFS= read -r ctoml; do
        [ -f "$ctoml" ] || continue
        cdir=$(dirname "$ctoml")
        pout=$(ra-query pub-surface "$cdir" --format json 2>/dev/null || echo '[]')
        crate_dead='{}'
        ilen=$(echo "$pout" | jq 'length')
        i=0
        while [ "$i" -lt "$ilen" ]; do
            ifile=$(echo "$pout" | jq -r ".[$i].file // \"\"")
            iline=$(echo "$pout" | jq -r ".[$i].line // 0")
            iname=$(echo "$pout" | jq -r ".[$i].name // \"\"")
            case "$ifile" in
                */lib.rs) i=$((i+1)); continue ;;
            esac
            cout=$(ra-query callers "${ifile}:${iline}" --format json 2>/dev/null || echo '{"callers":[]}')
            ccnt=$(echo "$cout" | jq '[.callers[]?] | length')
            if [ "$ccnt" -eq 0 ]; then
                crate_dead=$(echo "$crate_dead" | jq --arg f "$ifile" --arg n "$iname" --argjson l "$iline" '.[$f] = ((.[$f] // []) + [{name:$n,line:$l}])')
            fi
            i=$((i+1))
        done
        per_file=$(echo "$crate_dead" | jq -c '[to_entries[] | {file:.key, dead_count:(.value|length), items:.value}]')
        plen=$(echo "$per_file" | jq 'length')
        k=0
        while [ "$k" -lt "$plen" ]; do
            entry=$(echo "$per_file" | jq -c ".[$k]")
            ecnt=$(echo "$entry" | jq '.dead_count')
            pfindings=$(echo "$pfindings" | jq --argjson e "$entry" '. + [$e]')
            total_dead_pub=$((total_dead_pub + ecnt))
            k=$((k+1))
        done
    done < <(find crates -mindepth 2 -maxdepth 2 -name 'Cargo.toml' -not -path '*/target/*')
    emit_event "$(jq -nc --argjson cnt "$total_dead_pub" --arg out "$(echo "$pfindings" | jq -c . | tail -c 8192)" '{msg:"pub_surface_report",dead_pub_total:$cnt,findings_json_tail:$out}')"
else
    emit_event '{"msg":"ra_query_unavailable_pub_surface","detail":"skipping pub-surface stage"}'
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
top_dead_pub_files=$(echo "$pfindings" | jq -c 'sort_by(-.dead_count) | .[:3]')
dead_pub_k=$(echo "$top_dead_pub_files" | jq 'length')
j=0
while [ "$j" -lt "$dead_pub_k" ]; do
  pfile=$(echo "$top_dead_pub_files" | jq -r ".[$j].file")
  base=$(basename "$pfile")
  child="/workspace/audit-children/child-dead-pub-${j}.json"
  jq -nc \
    --arg id "brf_self_heal_${brief_id}_dead_pub_${j}" \
    --arg parent "$brief_id" \
    --arg pfile "$pfile" \
    --arg base "$base" \
    --argjson finding "$(echo "$top_dead_pub_files" | jq ".[$j]")" \
    --argjson rank "$j" \
    '($finding.items // []
        | map("  - " + ($finding.file) + ":" + ((.line // 0) | tostring) + " — " + (.name // "?"))
        | join("\n")) as $sites
     | {id:$id, project:null,
        topology:{name:"agentry-self-host-v0",version:1},
        payload:{
          issue_number:0,
          issue_title:("fix(dead-pub): remove or expose dead pub items in " + $base),
          issue_body:("Dead pub items in " + $pfile + " (zero workspace callers per ra-query callers).\n\nSites:\n" + $sites + "\n\nFor each site: DELETE the pub keyword OR add a `pub use` re-export in lib.rs to expose it as documented API surface. Do NOT silently leave items pub-but-unused — pick one path and apply it."),
          acceptance:"cargo fmt --check && cargo clippy --workspace --all-targets -- -D warnings && cargo test --workspace && scripts/arch-check.sh",
          target_repo:"yg/agentry",
          base_branch:"develop",
          pr_title:("fix(dead-pub): remove or expose dead pub items in " + $base),
          pr_body:("Auto-dispatched by auditor (ra-query pub-surface + ra-query callers, file ranked top-" + ($rank|tostring) + " by dead-pub count).")
        },
        budget:{max_wall_seconds:1500},
        escalation:"autonomous",
        parent_brief:$parent,
        submitted_by:"auditor-self-heal",
        submitted_at:(now|todate)}' > "$child"
  refs=$(echo "$refs" | jq -c --arg p "${host_workspace}/audit-children/child-dead-pub-${j}.json" '. + [$p]')
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
/// in unit tests. Bind-mounts host-built ra-query at /usr/local/bin/ra-query
/// (operator runs `just ra-query-binary` to provide it); the audit script's
/// `command -v ra-query` guard tolerates a missing binary by emitting
/// `ra_query_unavailable` and skipping the relevant stage.
fn build_auditor_claude_agentry_role(home: &str) -> AgentRole {
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
        ]),
        passthru_env: vec![],
        extra_bootstrap: vec![
            "rustup component add rustfmt clippy || true".into(),
            "rustup toolchain install nightly --profile minimal || true".into(),
            "cargo +nightly install cargo-udeps --locked --quiet || true".into(),
        ],
        mounts: vec![Mount {
            source: format!("{home}/.local/bin/ra-query"),
            target: "/usr/local/bin/ra-query".into(),
            readonly: true,
        }],
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
/// Build the null-agent role (EPIC #161 B0). First role ported from bash
/// to Rust. The behaviour lives in
/// `crates/agentry-role-runtime/src/bin/null_agent.rs`; the role spec just
/// bind-mounts the host-built binary and execs it.
fn build_null_agent_agentry_role(home: &str) -> AgentRole {
    AgentRole {
        name: RoleName("null-agent-agentry".into()),
        version: 1,
        model: None,
        system_prompt: None,
        image: ALPINE.into(),
        substrate_class: SubstrateClass::Podman,
        package_manager: PackageManager::Apk,
        entrypoint_script: "#!/bin/sh\nexec /usr/local/bin/null-agent\n".into(),
        exitpoint_script: None,
        binaries: vec![],
        mcp_servers: vec![],
        tool_allowlist: ToolAllowlist::default(),
        permit_scope: PermitScope::default(),
        passthru_env: vec![],
        extra_bootstrap: vec![],
        mounts: vec![Mount {
            source: format!("{home}/.local/bin/null-agent"),
            target: "/usr/local/bin/null-agent".into(),
            readonly: true,
        }],
        workspace_mount: None,
        sccache: false,
    }
}

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

/// Build the `git-operator` role. Ephemeral substrate-side worker that runs
/// `git add`/`commit`/`push` against /workspace and opens a PR via the gitea
/// REST API. The role's entrypoint is a one-line shell exec into the
/// bind-mounted Rust binary at `/usr/local/bin/git-operator` — acceptable
/// bootstrap glue per the refined EPIC #161 rule (no bash logic).
///
/// Bind-mounts `~/.local/bin/git-operator` from the host (built via
/// `just git-operator-binary`). Workspace mount is writable because git
/// updates `.git/{HEAD,refs,reflog}` during commit and push.
///
/// Registered in the seed but NOT wired into any topology — brief 6 of
/// EPIC #152 cuts agentry-self-host-v0 over from `shipper-agentry`.
fn build_git_operator_role(home: &str) -> AgentRole {
    AgentRole {
        name: RoleName("git-operator".into()),
        version: 1,
        model: None,
        system_prompt: None,
        image: RUNNER_HOST_IMAGE.into(),
        substrate_class: SubstrateClass::Podman,
        package_manager: PackageManager::Apt,
        // Bootstrap glue only: exec the bind-mounted Rust binary. All git +
        // forge logic lives in the binary, not in this script.
        entrypoint_script: "#!/bin/sh\nexec /usr/local/bin/git-operator\n".into(),
        exitpoint_script: None,
        // alpine + git + ca-certificates is enough — reqwest links rustls and
        // talks to the gitea API directly, so curl is not needed.
        binaries: vec!["git".into(), "ca-certificates".into()],
        mcp_servers: vec![],
        tool_allowlist: ToolAllowlist(vec!["git".into()]),
        permit_scope: PermitScope(vec![
            "fs:read:/workspace/**".into(),
            "fs:write:/workspace/**".into(),
            "net:allow:agency.lab".into(),
            "forge:write:yg/*".into(),
        ]),
        passthru_env: vec!["GITEA_TOKEN".into()],
        extra_bootstrap: vec![],
        mounts: vec![Mount {
            source: format!("{home}/.local/bin/git-operator"),
            target: "/usr/local/bin/git-operator".into(),
            readonly: true,
        }],
        // git push writes to /workspace/.git/{HEAD,refs,reflog,FETCH_HEAD};
        // workspace mount must be writable.
        workspace_mount: Some(WorkspaceMount {
            container_path: "/workspace".into(),
            readonly: false,
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
    let ac_verifier_gemini_agentry = build_ac_verifier_gemini_agentry_role(&home);
    let ac_verifier_grok_agentry = build_ac_verifier_grok_agentry_role(&home);
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
    // First role ported from bash to Rust under EPIC #161 B0. The legacy
    // bash heredoc (4 lines: emit_event + emit_done) becomes a Rust binary
    // built from `agentry-role-runtime`. The 1-line shell wrapper is
    // acceptable bootstrap glue per the refined #161 rule.
    let null_agent_agentry = build_null_agent_agentry_role(&home);
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
            ac_verifier_gemini_agentry.name.clone(),
            ac_verifier_grok_agentry.name.clone(),
            reviewer_mechanical_agentry.name.clone(),
            reviewer_claude_agentry.name.clone(),
            shipper_agentry.name.clone(),
            ci_watcher_agentry.name.clone(),
        ],
        // Rework loop enabled — max_retries=2 gives the coder two chances to
        // fix findings emitted by the reviewer before the team resolves Failed.
        message_graph: vec![
            // ORDERING INVARIANT: coder→reviewer edges are listed BEFORE
            // ac-verifier-{claude,gemini,grok}→reviewer edges so the daemon's
            // `team.incoming(reviewer).first()` rework lookup rewinds to the
            // coder, not to a (non-corrective) ac-verifier. Do not reorder.
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
            // Parallel ac-verifier siblings: gemini + grok fan out from the
            // coder and signal both reviewers. Any verifier emitting failed
            // rewinds to the coder before reviewers spawn (fail-closed).
            MessageEdge {
                from: coder_claude_agentry.name.clone(),
                to: ac_verifier_gemini_agentry.name.clone(),
                permit_overrides_from: None,
            },
            MessageEdge {
                from: coder_claude_agentry.name.clone(),
                to: ac_verifier_grok_agentry.name.clone(),
                permit_overrides_from: None,
            },
            MessageEdge {
                from: ac_verifier_gemini_agentry.name.clone(),
                to: reviewer_mechanical_agentry.name.clone(),
                permit_overrides_from: None,
            },
            MessageEdge {
                from: ac_verifier_gemini_agentry.name.clone(),
                to: reviewer_claude_agentry.name.clone(),
                permit_overrides_from: None,
            },
            MessageEdge {
                from: ac_verifier_grok_agentry.name.clone(),
                to: reviewer_mechanical_agentry.name.clone(),
                permit_overrides_from: None,
            },
            MessageEdge {
                from: ac_verifier_grok_agentry.name.clone(),
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

    // EPIC #152 brief 5: git-operator role registered. Brief 6 (this brief)
    // wires it into the new agentry-self-host-v1 topology below.
    let git_operator = build_git_operator_role(&home);

    // EPIC #152 brief 6: agentry-self-host-v1 — new topology shape.
    // Validators inside the coder's `/usr/local/bin/ship` tool absorb
    // reviewer-mechanical + ac-verifier roles. `git-operator` absorbs
    // shipper-agentry. `reviewer-claude` stays as design-review.
    //
    // v0 stays unchanged so in-flight v0 briefs continue to ship clean.
    let agentry_self_host_v1 = TeamTopology {
        name: TeamName("agentry-self-host-v1".into()),
        version: 1,
        roles: vec![
            coder_claude_agentry.name.clone(),
            reviewer_claude_agentry.name.clone(),
            git_operator.name.clone(),
            ci_watcher_agentry.name.clone(),
        ],
        message_graph: vec![
            MessageEdge {
                from: coder_claude_agentry.name.clone(),
                to: reviewer_claude_agentry.name.clone(),
                permit_overrides_from: None,
            },
            MessageEdge {
                from: reviewer_claude_agentry.name.clone(),
                to: git_operator.name.clone(),
                permit_overrides_from: None,
            },
            MessageEdge {
                from: git_operator.name.clone(),
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

    let auditor_claude_agentry = build_auditor_claude_agentry_role(&home);
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
    redis_io::save_role(&mut conn, &ac_verifier_gemini_agentry).await?;
    redis_io::save_role(&mut conn, &ac_verifier_grok_agentry).await?;
    redis_io::save_role(&mut conn, &shipper_agentry).await?;
    redis_io::save_role(&mut conn, &ci_watcher_agentry).await?;
    redis_io::save_team(&mut conn, &agentry_self_host_v0).await?;
    redis_io::save_team(&mut conn, &agentry_self_host_v1).await?;
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
    redis_io::save_role(&mut conn, &git_operator).await?;

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
        "seeded: roles [echo, workspace-probe, sccache-probe, timeout-probe, naughty, speaker, listener, grok-echo, claude-echo, synthesizer, narrowed-coder, shipper, coder-claude-agentry, ac-verifier-claude-agentry, ac-verifier-gemini-agentry, ac-verifier-grok-agentry, reviewer-mechanical-agentry, shipper-agentry, ci-watcher-agentry, reviewer-claude-agentry, auditor-claude-agentry, null-agent-agentry, archaeologist-claude-agentry, planner-claude-agentry, verifier-claude-agentry, git-operator] (inline entrypoint scripts); teams [echo, workspace-probe, sccache-probe, timeout-probe, naughty, speaker-listener, grok-echo, claude-echo, narrowed-team, shipper-solo-team, agentry-self-host-v0, agentry-self-host-v1, agentry-self-audit-v0, agentry-null-v0, agentry-discovery-v0, agentry-planner-v0, agentry-verify-v0]"
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn claude_p_timeout_is_env_overridable_in_bash_prelude() {
        assert!(BASH_PRELUDE.contains("CLAUDE_P_TIMEOUT=\"${CLAUDE_P_TIMEOUT:-1200}\""));
        assert!(!BASH_PRELUDE.contains("timeout 600"));
    }

    #[test]
    fn git_operator_role_has_workspace_mount_and_token_passthru() {
        let role = build_git_operator_role("/h");
        assert_eq!(role.name.0, "git-operator");
        assert!(
            role.workspace_mount.is_some(),
            "git-operator must have a workspace_mount (writable for git push)"
        );
        let ws = role.workspace_mount.as_ref().expect("workspace_mount");
        assert_eq!(ws.container_path, "/workspace");
        assert!(
            !ws.readonly,
            "workspace mount must be writable — git push updates .git/{{HEAD,refs,reflog}}"
        );
        assert!(
            role.passthru_env.iter().any(|e| e == "GITEA_TOKEN"),
            "git-operator must passthru GITEA_TOKEN for the gitea API auth header"
        );
        assert!(
            role.permit_scope
                .0
                .iter()
                .any(|s| s == "net:allow:agency.lab"),
            "git-operator permit_scope must allow net:allow:agency.lab for the gitea API call"
        );
        assert!(
            role.mounts
                .iter()
                .any(|m| m.target == "/usr/local/bin/git-operator" && m.readonly),
            "git-operator must bind-mount the host binary read-only at /usr/local/bin/git-operator"
        );
        assert!(
            role.entrypoint_script
                .contains("exec /usr/local/bin/git-operator"),
            "git-operator entrypoint must be the one-line shell exec into the Rust binary"
        );
        assert!(
            !role.entrypoint_script.contains("jq") && !role.entrypoint_script.contains("curl"),
            "git-operator entrypoint must not embed bash logic — all logic is in the Rust binary"
        );
    }

    // EPIC #161 Wave 1.4 — reviewer-claude prompt content (CRITICAL audit
    // clauses, salvage path, ra-query pre-pass) moved into the runner binary
    // at `crates/agentry-role-runtime/src/bin/reviewer_claude_runner.rs`.
    // Per-clause assertions live next to `build_review_prompt` and
    // `parse_findings` in that file's `mod tests`. The seed tests here now
    // only assert role-spec wiring (entrypoint, bind-mounts, permit_scope).

    #[test]
    fn reviewer_claude_role_entrypoint_invokes_runner() {
        let role = build_reviewer_claude_agentry_role(
            "/var/home/test",
            "/var/home/test/.config/agentry/claude-container-settings.json",
        );
        assert!(
            role.entrypoint_script
                .contains("exec /usr/local/bin/reviewer-claude-runner"),
            "reviewer-claude entrypoint must exec the runner: {}",
            role.entrypoint_script
        );
        // Defensive: no bash heredoc carryover from the pre-port script.
        for forbidden in ["set -euo pipefail", "stream_claude", "emit_event"] {
            assert!(
                !role.entrypoint_script.contains(forbidden),
                "entrypoint must not contain {} (legacy bash leftover)",
                forbidden
            );
        }
    }

    #[test]
    fn reviewer_claude_role_bind_mounts_runner_binary() {
        let role = build_reviewer_claude_agentry_role(
            "/var/home/test",
            "/var/home/test/.config/agentry/claude-container-settings.json",
        );
        assert!(
            role.mounts
                .iter()
                .any(|m| m.target == "/usr/local/bin/reviewer-claude-runner" && m.readonly),
            "reviewer-claude must bind-mount the runner read-only at /usr/local/bin/reviewer-claude-runner"
        );
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

    // EPIC #161 Wave 1.4 — ra-query pre-pass behaviour, prompt summary
    // injection, and missing-binary tolerance assertions live next to their
    // implementation in the runner binary's `mod tests`.

    #[test]
    fn null_agent_agentry_role_uses_rust_binary() {
        // EPIC #161 B0 — null-agent is the first role ported from a bash
        // heredoc to a Rust binary. The role's entrypoint_script is now a
        // one-line shell wrapper that execs the bind-mounted binary.
        let null = build_null_agent_agentry_role("/var/home/test");
        assert_eq!(null.name.0, "null-agent-agentry");
        assert!(
            null.entrypoint_script
                .contains("exec /usr/local/bin/null-agent"),
            "entrypoint must exec the Rust binary, got: {}",
            null.entrypoint_script
        );
        assert!(
            null.mounts
                .iter()
                .any(|m| m.target == "/usr/local/bin/null-agent"
                    && m.source == "/var/home/test/.local/bin/null-agent"),
            "null-agent role must bind-mount the host binary at the conventional path"
        );
        // Defensive: no bash heredoc carryover.
        for forbidden in ["set -euo pipefail", "emit_done", "emit_event"] {
            assert!(
                !null.entrypoint_script.contains(forbidden),
                "entrypoint must not contain {} (legacy bash leftover)",
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
        // EPIC #161 Wave 1.4 — REVIEWER_CLAUDE_AGENTRY_SCRIPT was deleted
        // and the role's claude invocation now lives in
        // `crates/agentry-role-runtime/src/bin/reviewer_claude_runner.rs`,
        // which uses `stream_claude` natively in Rust. The remaining bash
        // scripts must continue to use the bash `stream_claude` helper.
        // EPIC #161 Wave 1.2a — CODER_CLAUDE_AGENTRY_SCRIPT (entrypoint
        // half) was deleted and the role's claude invocation now lives in
        // `crates/agentry-role-runtime/src/bin/coder_claude_runner.rs`,
        // which uses Rust `stream_claude` natively. The remaining bash
        // scripts must continue to use the bash `stream_claude` helper.
        for (name, s) in [
            ("CLAUDE_SCRIPT", CLAUDE_SCRIPT),
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

    // EPIC #161 Wave 1.2b — self-review pipeline structure (stream-json
    // output, transcript suffix, multi-line JSON salvage, soft-fail
    // semantics) and dead-pub-check gate behaviour now live in
    // `crates/agentry-role-runtime/src/bin/coder_claude_runner.rs`. Tests
    // for those behaviours moved into the runner's own `mod tests`:
    // `parse_self_review_object_*`, `build_self_review_prompt_*`,
    // `slice_json_object_*`, plus the `run_self_review` / `run_dead_pub_check_phase`
    // wiring is tested at integration-runner level. The seed test below
    // pins down that the role wiring keeps the dead-pub-check bind-mount
    // (the runner needs the binary on PATH inside the container).

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
    fn coder_claude_agentry_role_has_ship_mount() {
        // EPIC #152 brief 1: stub `ship` binary must be bind-mounted at
        // /usr/local/bin/ship so future briefs (4 wires the validator
        // pipeline; 6 makes it the only path to publication) have the
        // delivery surface in place. Stub: prompt mentions but does NOT
        // call it yet.
        let role = build_coder_claude_agentry_role(
            "/var/home/test",
            "/var/home/test/.config/agentry/claude-container-settings.json",
        );
        assert!(
            role.mounts
                .iter()
                .any(|m| m.target == "/usr/local/bin/ship" && m.readonly),
            "coder-claude must bind-mount ship read-only at /usr/local/bin/ship"
        );
    }

    #[test]
    fn ac_verifier_claude_role_entrypoint_invokes_runner() {
        // EPIC #161 Wave 1.3 — script-content asserts (was: AC list parsing,
        // missing-binary degradation) moved to the runner's own unit tests
        // in `crates/agentry-role-runtime/src/bin/ac_verifier_runner.rs`.
        let role = build_ac_verifier_claude_agentry_role("/h", "/c");
        assert!(
            role.entrypoint_script
                .contains("ac-verifier-runner --provider claude"),
            "ac-verifier-claude entrypoint must exec the runner with --provider claude: {}",
            role.entrypoint_script
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
    fn ac_verifier_claude_role_bind_mounts_runner_binary() {
        let role = build_ac_verifier_claude_agentry_role("/h", "/c");
        assert!(
            role.mounts
                .iter()
                .any(|m| m.target == "/usr/local/bin/ac-verifier-runner" && m.readonly),
            "ac-verifier-claude must bind-mount the runner read-only at /usr/local/bin/ac-verifier-runner"
        );
    }

    #[test]
    fn ac_verifier_gemini_role_entrypoint_invokes_runner() {
        let role = build_ac_verifier_gemini_agentry_role("/h");
        assert!(
            role.entrypoint_script
                .contains("ac-verifier-runner --provider gemini"),
            "ac-verifier-gemini entrypoint must exec the runner with --provider gemini: {}",
            role.entrypoint_script
        );
    }

    #[test]
    fn ac_verifier_grok_role_entrypoint_invokes_runner() {
        let role = build_ac_verifier_grok_agentry_role("/h");
        assert!(
            role.entrypoint_script
                .contains("ac-verifier-runner --provider grok"),
            "ac-verifier-grok entrypoint must exec the runner with --provider grok: {}",
            role.entrypoint_script
        );
    }

    #[test]
    fn ac_verifier_gemini_role_bind_mounts_runner_binary() {
        let role = build_ac_verifier_gemini_agentry_role("/h");
        assert!(
            role.mounts
                .iter()
                .any(|m| m.target == "/usr/local/bin/ac-verifier-runner" && m.readonly),
            "ac-verifier-gemini must bind-mount the runner read-only at /usr/local/bin/ac-verifier-runner"
        );
    }

    #[test]
    fn ac_verifier_grok_role_bind_mounts_runner_binary() {
        let role = build_ac_verifier_grok_agentry_role("/h");
        assert!(
            role.mounts
                .iter()
                .any(|m| m.target == "/usr/local/bin/ac-verifier-runner" && m.readonly),
            "ac-verifier-grok must bind-mount the runner read-only at /usr/local/bin/ac-verifier-runner"
        );
    }

    #[test]
    fn ac_verifier_gemini_role_bind_mounts_ac_verifier_gemini_binary() {
        let role = build_ac_verifier_gemini_agentry_role("/h");
        assert!(
            role.mounts
                .iter()
                .any(|m| m.target == "/usr/local/bin/ac-verifier-gemini" && m.readonly),
            "ac-verifier-gemini role must bind-mount ac-verifier-gemini read-only at /usr/local/bin/ac-verifier-gemini"
        );
    }

    #[test]
    fn ac_verifier_grok_role_bind_mounts_ac_verifier_grok_binary() {
        let role = build_ac_verifier_grok_agentry_role("/h");
        assert!(
            role.mounts
                .iter()
                .any(|m| m.target == "/usr/local/bin/ac-verifier-grok" && m.readonly),
            "ac-verifier-grok role must bind-mount ac-verifier-grok read-only at /usr/local/bin/ac-verifier-grok"
        );
    }

    #[test]
    fn ac_verifier_gemini_role_passes_through_gemini_api_key() {
        let role = build_ac_verifier_gemini_agentry_role("/h");
        assert!(
            role.passthru_env.contains(&"GEMINI_API_KEY".to_string()),
            "ac-verifier-gemini role must pass through GEMINI_API_KEY: {:?}",
            role.passthru_env
        );
    }

    #[test]
    fn ac_verifier_grok_role_passes_through_xai_api_key() {
        let role = build_ac_verifier_grok_agentry_role("/h");
        assert!(
            role.passthru_env.contains(&"XAI_API_KEY".to_string()),
            "ac-verifier-grok role must passthru XAI_API_KEY"
        );
    }

    #[test]
    fn ac_verifier_gemini_role_permits_gemini_endpoint() {
        let role = build_ac_verifier_gemini_agentry_role("/h");
        assert!(
            role.permit_scope
                .0
                .iter()
                .any(|s| s.contains("generativelanguage.googleapis.com")),
            "ac-verifier-gemini role must allow generativelanguage.googleapis.com: {:?}",
            role.permit_scope.0
        );
    }

    #[test]
    fn ac_verifier_grok_role_permit_scope_allows_xai_api() {
        let role = build_ac_verifier_grok_agentry_role("/h");
        assert!(
            role.permit_scope.0.iter().any(|s| s.contains("api.x.ai")),
            "ac-verifier-grok role permit_scope must allow api.x.ai"
        );
    }

    #[test]
    fn agentry_self_host_v0_topology_has_ac_verifier_with_correct_edges() {
        // Mirror of the agentry-self-host-v0 topology block in seed_m0 — built
        // here so the dual-inbound ordering invariant is covered without
        // touching Redis. Keep in sync with seed_m0.
        let coder = build_coder_claude_agentry_role("/h", "/c");
        let ac_verifier = build_ac_verifier_claude_agentry_role("/h", "/c");
        let ac_verifier_gemini = build_ac_verifier_gemini_agentry_role("/h");
        let ac_verifier_grok = build_ac_verifier_grok_agentry_role("/h");
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
                ac_verifier_gemini.name.clone(),
                ac_verifier_grok.name.clone(),
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
                    from: coder.name.clone(),
                    to: ac_verifier_gemini.name.clone(),
                    permit_overrides_from: None,
                },
                MessageEdge {
                    from: coder.name.clone(),
                    to: ac_verifier_grok.name.clone(),
                    permit_overrides_from: None,
                },
                MessageEdge {
                    from: ac_verifier_gemini.name.clone(),
                    to: reviewer_mechanical.clone(),
                    permit_overrides_from: None,
                },
                MessageEdge {
                    from: ac_verifier_gemini.name.clone(),
                    to: reviewer_claude.name.clone(),
                    permit_overrides_from: None,
                },
                MessageEdge {
                    from: ac_verifier_grok.name.clone(),
                    to: reviewer_mechanical.clone(),
                    permit_overrides_from: None,
                },
                MessageEdge {
                    from: ac_verifier_grok.name.clone(),
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
        let coder_to_rev_claude = edge_idx(&coder.name, &reviewer_claude.name);

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

        // Brief 5 of #134: gemini + grok wired as parallel siblings.
        assert!(
            topology.roles.contains(&ac_verifier_gemini.name),
            "ac-verifier-gemini-agentry must be in roles"
        );
        assert!(
            topology.roles.contains(&ac_verifier_grok.name),
            "ac-verifier-grok-agentry must be in roles"
        );

        let coder_to_acv_gemini = edge_idx(&coder.name, &ac_verifier_gemini.name);
        let coder_to_acv_grok = edge_idx(&coder.name, &ac_verifier_grok.name);
        let acv_gemini_to_rev_mech = edge_idx(&ac_verifier_gemini.name, &reviewer_mechanical);
        let acv_gemini_to_rev_claude = edge_idx(&ac_verifier_gemini.name, &reviewer_claude.name);
        let acv_grok_to_rev_mech = edge_idx(&ac_verifier_grok.name, &reviewer_mechanical);
        let acv_grok_to_rev_claude = edge_idx(&ac_verifier_grok.name, &reviewer_claude.name);

        assert!(
            coder_to_acv_gemini.is_some(),
            "coder→ac-verifier-gemini edge must exist"
        );
        assert!(
            coder_to_acv_grok.is_some(),
            "coder→ac-verifier-grok edge must exist"
        );
        assert!(
            acv_gemini_to_rev_mech.is_some(),
            "ac-verifier-gemini→reviewer-mechanical edge must exist"
        );
        assert!(
            acv_gemini_to_rev_claude.is_some(),
            "ac-verifier-gemini→reviewer-claude edge must exist"
        );
        assert!(
            acv_grok_to_rev_mech.is_some(),
            "ac-verifier-grok→reviewer-mechanical edge must exist"
        );
        assert!(
            acv_grok_to_rev_claude.is_some(),
            "ac-verifier-grok→reviewer-claude edge must exist"
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

        // Extended ordering invariant across all three verifier variants:
        // every coder→reviewer edge index must be less than every
        // ac-verifier-*→reviewer edge index.
        let coder_to_reviewer_indices: Vec<usize> = [coder_to_rev_mech, coder_to_rev_claude]
            .iter()
            .map(|opt| opt.expect("coder→reviewer edge already asserted present"))
            .collect();
        let acv_to_reviewer_indices: Vec<usize> = [
            acv_to_rev_mech,
            acv_to_rev_claude,
            acv_gemini_to_rev_mech,
            acv_gemini_to_rev_claude,
            acv_grok_to_rev_mech,
            acv_grok_to_rev_claude,
        ]
        .iter()
        .map(|opt| opt.expect("ac-verifier→reviewer edge already asserted present"))
        .collect();
        for c_idx in &coder_to_reviewer_indices {
            for a_idx in &acv_to_reviewer_indices {
                assert!(
                    c_idx < a_idx,
                    "coder→reviewer edge at {c_idx} must precede ac-verifier→reviewer edge at {a_idx}"
                );
            }
        }
    }

    #[test]
    fn agentry_self_host_v0_topology_has_all_three_ac_verifiers_wired_in_parallel() {
        // Mirror of the agentry-self-host-v0 topology block in seed_m0 — built
        // here so the parallel-verifier wiring is covered without touching
        // Redis. Keep in sync with seed_m0.
        let coder = build_coder_claude_agentry_role("/h", "/c");
        let ac_verifier_claude = build_ac_verifier_claude_agentry_role("/h", "/c");
        let ac_verifier_gemini = build_ac_verifier_gemini_agentry_role("/h");
        let ac_verifier_grok = build_ac_verifier_grok_agentry_role("/h");
        let reviewer_claude = build_reviewer_claude_agentry_role("/h", "/c");
        let reviewer_mechanical = RoleName("reviewer-mechanical-agentry".into());
        let shipper = RoleName("shipper-agentry".into());
        let ci_watcher = RoleName("ci-watcher-agentry".into());

        let topology = TeamTopology {
            name: TeamName("agentry-self-host-v0".into()),
            version: 1,
            roles: vec![
                coder.name.clone(),
                ac_verifier_claude.name.clone(),
                ac_verifier_gemini.name.clone(),
                ac_verifier_grok.name.clone(),
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
                    to: ac_verifier_claude.name.clone(),
                    permit_overrides_from: None,
                },
                MessageEdge {
                    from: ac_verifier_claude.name.clone(),
                    to: reviewer_mechanical.clone(),
                    permit_overrides_from: None,
                },
                MessageEdge {
                    from: ac_verifier_claude.name.clone(),
                    to: reviewer_claude.name.clone(),
                    permit_overrides_from: None,
                },
                MessageEdge {
                    from: coder.name.clone(),
                    to: ac_verifier_gemini.name.clone(),
                    permit_overrides_from: None,
                },
                MessageEdge {
                    from: coder.name.clone(),
                    to: ac_verifier_grok.name.clone(),
                    permit_overrides_from: None,
                },
                MessageEdge {
                    from: ac_verifier_gemini.name.clone(),
                    to: reviewer_mechanical.clone(),
                    permit_overrides_from: None,
                },
                MessageEdge {
                    from: ac_verifier_gemini.name.clone(),
                    to: reviewer_claude.name.clone(),
                    permit_overrides_from: None,
                },
                MessageEdge {
                    from: ac_verifier_grok.name.clone(),
                    to: reviewer_mechanical.clone(),
                    permit_overrides_from: None,
                },
                MessageEdge {
                    from: ac_verifier_grok.name.clone(),
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

        // (a) all three verifier role names present.
        assert!(
            topology.roles.contains(&ac_verifier_claude.name),
            "ac-verifier-claude-agentry must be in roles"
        );
        assert!(
            topology.roles.contains(&ac_verifier_gemini.name),
            "ac-verifier-gemini-agentry must be in roles"
        );
        assert!(
            topology.roles.contains(&ac_verifier_grok.name),
            "ac-verifier-grok-agentry must be in roles"
        );

        // (b) coder fans out to all three verifiers.
        let coder_to_verifier_count = topology
            .message_graph
            .iter()
            .filter(|e| {
                e.from == coder.name
                    && (e.to == ac_verifier_claude.name
                        || e.to == ac_verifier_gemini.name
                        || e.to == ac_verifier_grok.name)
            })
            .count();
        assert_eq!(
            coder_to_verifier_count, 3,
            "coder must fan out to all three ac-verifier variants (claude, gemini, grok)"
        );

        // (c) each verifier signals both reviewers (six edges total).
        let verifier_to_reviewer_count = topology
            .message_graph
            .iter()
            .filter(|e| {
                (e.from == ac_verifier_claude.name
                    || e.from == ac_verifier_gemini.name
                    || e.from == ac_verifier_grok.name)
                    && (e.to == reviewer_mechanical || e.to == reviewer_claude.name)
            })
            .count();
        assert_eq!(
            verifier_to_reviewer_count, 6,
            "each ac-verifier variant must signal both reviewers (3 verifiers × 2 reviewers = 6)"
        );
    }

    #[test]
    fn agentry_self_host_v1_topology_has_expected_shape() {
        // EPIC #152 brief 6: mirror of the agentry-self-host-v1 topology block
        // in seed_m0 — built here so the topology shape is covered without
        // touching Redis. Keep in sync with seed_m0.
        let coder = build_coder_claude_agentry_role("/h", "/c");
        let reviewer_claude = build_reviewer_claude_agentry_role("/h", "/c");
        let git_operator = build_git_operator_role("/h");
        let ci_watcher = RoleName("ci-watcher-agentry".into());

        let topology = TeamTopology {
            name: TeamName("agentry-self-host-v1".into()),
            version: 1,
            roles: vec![
                coder.name.clone(),
                reviewer_claude.name.clone(),
                git_operator.name.clone(),
                ci_watcher.clone(),
            ],
            message_graph: vec![
                MessageEdge {
                    from: coder.name.clone(),
                    to: reviewer_claude.name.clone(),
                    permit_overrides_from: None,
                },
                MessageEdge {
                    from: reviewer_claude.name.clone(),
                    to: git_operator.name.clone(),
                    permit_overrides_from: None,
                },
                MessageEdge {
                    from: git_operator.name.clone(),
                    to: ci_watcher.clone(),
                    permit_overrides_from: None,
                },
            ],
            terminal_role: ci_watcher.clone(),
            max_retries: 2,
        };

        assert_eq!(
            topology.roles.len(),
            4,
            "v1 topology must have exactly 4 roles (coder, reviewer-claude, git-operator, ci-watcher) — not 8 like v0"
        );
        assert!(topology.roles.contains(&coder.name));
        assert!(topology.roles.contains(&reviewer_claude.name));
        assert!(topology.roles.contains(&git_operator.name));
        assert!(topology.roles.contains(&ci_watcher));

        assert_eq!(
            topology.terminal_role, ci_watcher,
            "v1 terminal role must be ci-watcher-agentry"
        );
        assert_eq!(topology.max_retries, 2, "v1 max_retries must be 2");

        assert_eq!(
            topology.message_graph.len(),
            3,
            "v1 must have exactly 3 edges (coder→reviewer-claude, reviewer-claude→git-operator, git-operator→ci-watcher)"
        );
        let has_edge = |from: &RoleName, to: &RoleName| -> bool {
            topology
                .message_graph
                .iter()
                .any(|e| e.from == *from && e.to == *to)
        };
        assert!(
            has_edge(&coder.name, &reviewer_claude.name),
            "v1 must have edge coder→reviewer-claude"
        );
        assert!(
            has_edge(&reviewer_claude.name, &git_operator.name),
            "v1 must have edge reviewer-claude→git-operator"
        );
        assert!(
            has_edge(&git_operator.name, &ci_watcher),
            "v1 must have edge git-operator→ci-watcher"
        );
    }

    // EPIC #161 Wave 1.2b — `coder_exitpoint_skips_git_under_v1` retired.
    // The v1+ topology short-circuit (was bash regex `-v[1-9][0-9]*$`,
    // now Rust `is_v1_plus_topology` in the runner) is asserted in the
    // runner binary's `mod tests`:
    // `is_v1_plus_topology_matches_v1_through_v99` (positive cases) and
    // `is_v1_plus_topology_rejects_v0_and_other_shapes` (v0, no leading
    // dash, leading zeros, suffixes after digits).

    // EPIC #161 Wave 1.2a — coder entrypoint behaviour assertions
    // (topology_name export, team_context.messages walk, rework banner
    // composition, blocker-severity filter) moved into the runner binary's
    // `mod tests` next to the implementations:
    //   - `collect_blocker_findings_*`
    //   - `build_rework_banner_*`
    //   - `write_brief_vars_emits_sourceable_script`
    //   - `build_coder_prompt_*`

    #[test]
    fn coder_role_entrypoint_invokes_runner() {
        let role = build_coder_claude_agentry_role(
            "/var/home/test",
            "/var/home/test/.config/agentry/claude-container-settings.json",
        );
        assert!(
            role.entrypoint_script
                .contains("exec /usr/local/bin/coder-claude-runner"),
            "coder entrypoint must exec the runner: {}",
            role.entrypoint_script
        );
        // Defensive: no bash heredoc carryover from the pre-port script.
        for forbidden in ["set -euo pipefail", "stream_claude", "team_context"] {
            assert!(
                !role.entrypoint_script.contains(forbidden),
                "coder entrypoint must not contain {} (legacy bash leftover)",
                forbidden
            );
        }
    }

    #[test]
    fn coder_role_bind_mounts_runner_binary() {
        let role = build_coder_claude_agentry_role(
            "/var/home/test",
            "/var/home/test/.config/agentry/claude-container-settings.json",
        );
        assert!(
            role.mounts
                .iter()
                .any(|m| m.target == "/usr/local/bin/coder-claude-runner" && m.readonly),
            "coder must bind-mount the runner read-only at /usr/local/bin/coder-claude-runner"
        );
    }

    #[test]
    fn coder_role_uses_single_rust_runner_no_bash_exitpoint() {
        // EPIC #161 Wave 1.2b: entrypoint AND exitpoint merged into one
        // Rust binary. The role's exitpoint_script is None — the merged
        // runner owns the full lifecycle (cargo fmt, quality-hygiene,
        // acceptance, self-review, dead-pub-check, git commit) in-process.
        let role = build_coder_claude_agentry_role(
            "/var/home/test",
            "/var/home/test/.config/agentry/claude-container-settings.json",
        );
        assert!(
            role.exitpoint_script.is_none(),
            "coder role must have no bash exitpoint after Wave 1.2b — got: {:?}",
            role.exitpoint_script
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
    fn auditor_role_bind_mounts_ra_query() {
        let role = build_auditor_claude_agentry_role("/var/home/test");
        assert!(
            role.mounts
                .iter()
                .any(|m| m.target == "/usr/local/bin/ra-query" && m.readonly),
            "auditor-claude must bind-mount ra-query read-only at /usr/local/bin/ra-query"
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

    #[test]
    fn auditor_complexity_jq_does_not_reference_undeclared_ccnt() {
        assert!(
            !AUDITOR_CLAUDE_AGENTRY_SCRIPT.contains("complex_count:$ccnt"),
            "issue #147: auditor complexity jq filter must not reference \
             undeclared jq variable $ccnt (the bash $ccnt is not passed via \
             --argjson, so jq aborts and findings_json_tail goes empty)"
        );
        assert!(
            AUDITOR_CLAUDE_AGENTRY_SCRIPT.contains("complex_count:($r.functions|length)"),
            "issue #147: auditor complexity jq filter must compute \
             complex_count from $r.functions|length (mirroring the unwraps \
             stage which uses ($r.functions|map(.unwraps|length)|add // 0))"
        );
    }

    #[test]
    fn auditor_script_runs_ra_query_pub_surface() {
        assert!(
            AUDITOR_CLAUDE_AGENTRY_SCRIPT.contains("ra-query pub-surface"),
            "auditor script must invoke `ra-query pub-surface`"
        );
        assert!(
            AUDITOR_CLAUDE_AGENTRY_SCRIPT.contains("ra-query callers"),
            "auditor script must invoke `ra-query callers` to count workspace usages of pub items"
        );
        assert!(
            AUDITOR_CLAUDE_AGENTRY_SCRIPT.contains("pub_surface_report"),
            "auditor script must emit a pub_surface_report trace event"
        );
        assert!(
            AUDITOR_CLAUDE_AGENTRY_SCRIPT.contains("ra_query_unavailable_pub_surface"),
            "auditor script must emit ra_query_unavailable_pub_surface when the binary is missing"
        );
    }

    #[test]
    fn auditor_emits_dead_pub_fix_children() {
        assert!(
            AUDITOR_CLAUDE_AGENTRY_SCRIPT.contains("audit-children/child-dead-pub-"),
            "auditor must write dead-pub fix-child briefs to audit-children/child-dead-pub-*"
        );
        assert!(
            AUDITOR_CLAUDE_AGENTRY_SCRIPT.contains("brf_self_heal_${brief_id}_dead_pub_"),
            "auditor must generate brf_self_heal_<brief_id>_dead_pub_<j> identifiers"
        );
        assert!(
            AUDITOR_CLAUDE_AGENTRY_SCRIPT.contains("sort_by(-.dead_count)"),
            "auditor must select top-K dead-pub files via sort_by(-.dead_count)"
        );
        assert!(
            AUDITOR_CLAUDE_AGENTRY_SCRIPT.contains("agentry-self-host-v0"),
            "dead-pub fix-child briefs must dispatch into agentry-self-host-v0 (mirrors unwrap children)"
        );
    }

    #[test]
    fn issue_175_runner_host_roles_use_glibc_compatible_image() {
        // Regression: roles whose entrypoint exec's a host-built runner
        // binary (`reviewer-claude-runner`, `ac-verifier-runner`, etc.) MUST
        // run in an image whose glibc is at least the host build's
        // compile-target. `debian:bookworm-slim` (glibc 2.36) was the prior
        // value; binaries built on a Fedora 43 / glibc 2.42 host emit
        // GLIBC_2.39 symbols, so the dynamic linker fails before `main()`
        // ever runs — `DoneGuard` never registers, no events emit, and the
        // role's verdict is synthesised from a bare exit code.
        //
        // This test pins the affected roles to `RUNNER_HOST_IMAGE`
        // (currently `debian:trixie-slim`, glibc 2.41). Future regressions
        // — e.g. someone copy-pasting `bookworm-slim` into a new role spec
        // — surface here at `cargo test` rather than in production at the
        // first reviewer dispatch. `git-operator` joined the set per #200:
        // its host-built binary has the same glibc dependency.
        let reviewer = build_reviewer_claude_agentry_role("/h", "/h/.claude/settings.json");
        let acv_claude = build_ac_verifier_claude_agentry_role("/h", "/h/.claude/settings.json");
        let acv_gemini = build_ac_verifier_gemini_agentry_role("/h");
        let acv_grok = build_ac_verifier_grok_agentry_role("/h");
        let git_operator = build_git_operator_role("/h");

        for role in &[reviewer, acv_claude, acv_gemini, acv_grok, git_operator] {
            assert_eq!(
                role.image, RUNNER_HOST_IMAGE,
                "role '{}' must use RUNNER_HOST_IMAGE — see #175 \
                 (host-built runner binary fails to load on bookworm-slim glibc 2.36)",
                role.name
            );
            // Sanity: the role exec's a host-built binary, which is what
            // makes glibc compatibility load-bearing. Accepts either a
            // `*-runner` binary (reviewer, ac-verifier family) or the
            // git-operator binary (#200). If a future refactor moves the
            // entrypoint back to inline bash this test is no longer
            // load-bearing — but until then the assertion holds.
            assert!(
                role.entrypoint_script.contains("exec /usr/local/bin/")
                    && (role.entrypoint_script.contains("-runner")
                        || role.entrypoint_script.contains("git-operator")),
                "role '{}' is expected to exec a host-built binary; \
                 if the entrypoint changed shape, revisit whether \
                 RUNNER_HOST_IMAGE still applies",
                role.name
            );
        }
    }
}
