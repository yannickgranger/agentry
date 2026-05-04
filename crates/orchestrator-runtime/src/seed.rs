//! Seed the Redis registry with the agent roles and team topologies.
//!
//! Each role carries its entrypoint as an inline bash script (no per-agent
//! Containerfile). The spawner picks a stock public base image, installs the
//! role's declared `binaries` via `package_manager`, then execs the script.
//!
//! Idempotent: overwrites existing records with current definitions.

use crate::{redis_io, role_dir_loader, Config, Error, Result};
use orchestrator_types::{
    AgentRole, MessageEdge, Mount, PackageManager, PermitScope, RoleName, RoleRef, SubstrateClass,
    TeamName, TeamTopology, ToolAllowlist, WorkspaceMount,
};
use std::path::PathBuf;

/// Derive the `net:allow:<host>` permit for the configured forge from
/// `cfg.forge.default_host`. The port suffix (if any) is stripped so a
/// `default_host = "agency.lab:3000"` still produces `"net:allow:agency.lab"`
/// — byte-for-byte equivalent to the literal that lived in seed.rs before
/// phase 4 of #330. Returns `Error::Config` when `default_host` is unset.
pub fn derive_forge_net_allow(cfg: &Config) -> Result<String> {
    let host_only = cfg
        .forge
        .default_host
        .as_deref()
        .map(|h| h.split(':').next().unwrap_or(h).to_string())
        .ok_or_else(|| Error::Config("[forge] default_host required".into()))?;
    Ok(format!("net:allow:{host_only}"))
}

/// Expand `cfg.forge.allowed_owners` to a `forge:write:<owner>/*` permit
/// per entry. Empty list returns `Error::Config` — the prior literal
/// `"forge:write:yg/*"` baked the only allowed owner into source; the
/// empty list is treated as "no forge writes permitted" and rejected at
/// seed time so misconfiguration surfaces immediately rather than as a
/// silent broker denial mid-brief.
pub fn derive_forge_write_permits(cfg: &Config) -> Result<Vec<String>> {
    if cfg.forge.allowed_owners.is_empty() {
        return Err(Error::Config(
            "[forge] allowed_owners required (empty list rejects all writes)".into(),
        ));
    }
    Ok(cfg
        .forge
        .allowed_owners
        .iter()
        .map(|owner| format!("forge:write:{owner}/*"))
        .collect())
}

/// Derive the optional `net:allow:<host>` permit for the shared sccache
/// backend. Same port-stripping idiom as `derive_forge_net_allow` — an
/// `endpoint = "agentry-sccache-redis:6379"` produces
/// `"net:allow:agentry-sccache-redis"`. `None` means roles seed without
/// the sccache permit and with `sccache: false`.
pub fn derive_sccache_net_allow(cfg: &Config) -> Option<String> {
    cfg.sccache.endpoint.as_deref().map(|ep| {
        let host_only = ep.split(':').next().unwrap_or(ep);
        format!("net:allow:{host_only}")
    })
}

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

// EPIC #161 wave-bash: SHIPPER_AGENTRY_SCRIPT bash heredoc that used to live
// here has been ported to a Rust runner —
// `crates/agentry-role-runtime/src/bin/shipper_runner.rs`. The role's
// entrypoint_script now just `exec /usr/local/bin/shipper-runner`. v2 of
// the port fixes the v1 reviewer Blocker on credential leakage by keeping
// the token out of every URL the runner builds — auth flows only via
// `-c http.extraheader` (git) and the `Authorization:` header (curl).

// EPIC #161 wave-bash port: PR_REBASER_AGENTRY_SCRIPT bash heredoc that
// used to live here has been ported to a Rust runner —
// `crates/agentry-role-runtime/src/bin/pr_rebaser_runner.rs`. The role's
// entrypoint_script now just `exec /usr/local/bin/pr-rebaser-runner`. The
// runner has its own unit-test coverage for payload parsing, remote-URL
// composition, porcelain-v2 unmerged-file extraction, push argv shape,
// and rebase-outcome classification.

// EPIC #161 Wave 3: VERIFIER_CLAUDE_AGENTRY_SCRIPT bash heredoc that used to
// live here (the DOL verifier — runs the meta-brief's success_criteria as a
// shell command and maps exit code to verdict) has been ported to a Rust
// runner — `crates/agentry-role-runtime/src/bin/verifier_dol_runner.rs`.
// The role's entrypoint_script now just `exec /usr/local/bin/verifier-dol-runner`.
// The runner has its own unit-test coverage for success_criteria parsing,
// exit-code → verdict mapping, and the output-tail constant.

// EPIC #161 Wave 1.3: the three AC_VERIFIER_*_AGENTRY_SCRIPT bash heredocs
// that used to live here have been ported to one Rust runner —
// `crates/agentry-role-runtime/src/bin/ac_verifier_runner.rs` — parameterized
// by `--provider claude|gemini|grok`. The roles' entrypoint_scripts now just
// `exec /usr/local/bin/ac-verifier-runner --provider X`. The runner has its
// own unit-test coverage for AC parsing / degradation envelopes.

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
    let home = std::env::var("HOME")
        .expect("HOME env var must be set to materialize container claude settings");
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

    // Phase 4 of #330: permit strings derived once from `Config` so every
    // role definition below references the operator-configured forge host
    // / sccache endpoint instead of hard-coded `agency.lab` and
    // `agentry-sccache-redis` literals. Port suffixes on either are
    // stripped — `agency.lab:3000` still produces `net:allow:agency.lab`,
    // byte-for-byte equivalent to the prior literal.
    let forge_net_allow = derive_forge_net_allow(cfg)?;
    let forge_write_permits = derive_forge_write_permits(cfg)?;
    let sccache_net_allow = derive_sccache_net_allow(cfg);

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
    let home = std::env::var("HOME")
        .expect("HOME env var must be set to bind claude credentials into the claude-echo role");
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
    let sccache_probe_permits: Vec<String> = sccache_net_allow
        .as_deref()
        .map(|s| vec![s.to_string()])
        .unwrap_or_default();
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
        permit_scope: PermitScope(sccache_probe_permits),
        passthru_env: vec![],
        extra_bootstrap: vec![],
        mounts: vec![],
        workspace_mount: None,
        sccache: sccache_net_allow.is_some(),
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
            forge_net_allow.clone(),
        ]),
        passthru_env: vec!["GITEA_TOKEN".into()],
        extra_bootstrap: vec![
            "rustup component add rustfmt clippy".into(),
            "git config --global http.sslVerify false".into(),
            "cargo install --git https://github.com/yannickgranger/cfdb.git --rev 02c5a45 --root /usr/local --locked --quiet cfdb-cli || true".into(),
            "cargo install --git https://github.com/yannickgranger/graph-specs.git --rev ecaedb9 --root /usr/local --locked --quiet application || true".into(),
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
    redis_io::save_role(&mut conn, &reviewer_mechanical_agentry).await?;

    let roles_dir = seed_roles_dir();
    if roles_dir.exists() {
        let template_ctx = role_dir_loader::TemplateContext {
            home: std::env::var("HOME").expect("HOME env var must be set to derive role-dir paths"),
            forge_net_allow: forge_net_allow.clone(),
            forge_write_permits: forge_write_permits.clone(),
            sccache_net_allow: sccache_net_allow.clone(),
        };
        let loaded =
            role_dir_loader::load_roles_from_dir(&mut conn, &roles_dir, &template_ctx).await?;
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

    let topologies_dir = seed_topologies_dir();
    if topologies_dir.exists() {
        let loaded = load_topologies_from_dir(&mut conn, &topologies_dir).await?;
        tracing::info!(
            count = loaded.len(),
            dir = %topologies_dir.display(),
            "loaded JSON topology catalog from seed directory",
        );
    } else {
        tracing::debug!(
            dir = %topologies_dir.display(),
            "seed topologies directory absent — skipping JSON topology load",
        );
    }

    tracing::info!(
"seeded: roles [echo, workspace-probe, sccache-probe, timeout-probe, naughty, speaker, listener, grok-echo, claude-echo, synthesizer, narrowed-coder, coder-claude-agentry, ac-verifier-claude-agentry, ac-verifier-gemini-agentry, ac-verifier-grok-agentry, reviewer-mechanical-agentry, reviewer-claude-agentry, null-agent-agentry] (inline entrypoint scripts); teams [echo, workspace-probe, sccache-probe, timeout-probe, naughty, speaker-listener, grok-echo, claude-echo, narrowed-team] (Rust literals); teams [agentry-null-v0, agentry-pr-rebaser-v0, agentry-discovery-v0, agentry-verify-v0, agentry-planner-v0, agentry-self-host-v0, agentry-self-audit-v0] (loaded from seed/topologies/*.json)"
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

/// Resolve the directory containing team-topology JSON files for seed-time
/// loading. Mirrors [`seed_roles_dir`]: defaults to `<workspace_root>/seed/topologies`,
/// overridable via `AGENTRY_SEED_TOPOLOGIES_DIR` for substrates that ship
/// the catalog elsewhere.
fn seed_topologies_dir() -> PathBuf {
    if let Ok(override_path) = std::env::var("AGENTRY_SEED_TOPOLOGIES_DIR") {
        return PathBuf::from(override_path);
    }
    let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let workspace_root = manifest
        .parent()
        .and_then(|p| p.parent())
        .map(PathBuf::from)
        .unwrap_or(manifest);
    workspace_root.join("seed").join("topologies")
}

/// Load every `*.json` topology file in `dir` into Redis via
/// [`redis_io::register_team_strict`]. Returns the list of `(name, version)`
/// pairs that were registered (or already present), in alphabetical order
/// by file name. Mirrors the role-dir-loader pattern.
///
/// First-writer-wins: if a topology key already exists, it is left untouched
/// and the loader logs the existing entry rather than overwriting. This
/// matches the `orchestrator team register` CLI semantics — operator-edited
/// topologies survive a re-seed.
async fn load_topologies_from_dir(
    conn: &mut redis::aio::ConnectionManager,
    dir: &std::path::Path,
) -> Result<Vec<(orchestrator_types::TeamName, u32)>> {
    if !dir.exists() {
        return Ok(Vec::new());
    }

    let mut json_files: Vec<PathBuf> = Vec::new();
    let mut rd = tokio::fs::read_dir(dir).await?;
    while let Some(entry) = rd.next_entry().await? {
        let path = entry.path();
        if path.extension().and_then(|s| s.to_str()) == Some("json") {
            json_files.push(path);
        }
    }
    json_files.sort();

    let mut out: Vec<(orchestrator_types::TeamName, u32)> = Vec::with_capacity(json_files.len());
    for path in json_files {
        let text =
            tokio::fs::read_to_string(&path)
                .await
                .map_err(|e| Error::TopologyLoadFailed {
                    path: path.clone(),
                    source: Box::new(e),
                })?;
        let team: TeamTopology =
            serde_json::from_str(&text).map_err(|e| Error::TopologyLoadFailed {
                path: path.clone(),
                source: Box::new(e),
            })?;
        match redis_io::register_team_strict(conn, &team)
            .await
            .map_err(|e| Error::TopologyLoadFailed {
                path: path.clone(),
                source: Box::new(e),
            })? {
            redis_io::RegisterOutcome::Registered => {
                tracing::info!(
                    team_name = %team.name.0,
                    version = team.version,
                    file_path = %path.display(),
                    "registered topology from JSON file",
                );
            }
            redis_io::RegisterOutcome::AlreadyExists => {
                tracing::info!(
                    team_name = %team.name.0,
                    version = team.version,
                    file_path = %path.display(),
                    "topology already registered — skipping (first-writer-wins)",
                );
            }
        }
        out.push((team.name, team.version));
    }
    Ok(out)
}
