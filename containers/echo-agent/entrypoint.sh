#!/usr/bin/env bash
# echo-agent entrypoint. Reads startup JSON on stdin (discard for M0), emits
# two NDJSON events on stdout, exits.

set -euo pipefail

# Discard the startup bundle.
cat > /dev/null

emit_event() {
    local payload="$1"
    printf '{"at":"%s","type":"event","payload":%s}\n' \
        "$(date -Iseconds)" "$payload"
}

emit_done() {
    local verdict="$1"
    printf '{"at":"%s","type":"done","verdict":"%s"}\n' \
        "$(date -Iseconds)" "$verdict"
}

emit_event '{"msg":"hello"}'
emit_done "shipped"
