#!/usr/bin/env bash
# claude-agent — Claude Max via host `claude` CLI. Headless mode (`-p`).
set -euo pipefail

bundle="$(cat)"

prompt="$(jq -r '.brief.payload.prompt // "Hello?"' <<<"$bundle")"

emit_event() {
    jq -nc --arg at "$(date -Iseconds)" --argjson payload "$1" \
        '{at:$at, type:"event", payload:$payload}'
}

emit_done() {
    local verdict="$1"
    jq -nc --arg at "$(date -Iseconds)" --arg v "$verdict" \
        '{at:$at, type:"done", verdict:$v}'
}

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

# Headless call. Set HOME so claude finds /root/.claude/.
reply=$(HOME=/root claude -p "$prompt" 2>&1) || {
    emit_event "$(jq -nc --arg err "$reply" '{error:"claude -p failed", detail:$err}')"
    emit_done "failed"
    exit 0
}

emit_event "$(jq -nc --arg r "$reply" '{reply:$r}')"
emit_done "shipped"
