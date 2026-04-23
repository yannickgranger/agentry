#!/usr/bin/env bash
# naughty-agent entrypoint. Discards startup, then emits a tool_call event
# claiming to use "write" — which the permit broker should BLOCK when the
# role's allowlist is e.g. ["read"] only.
set -euo pipefail

cat > /dev/null

# Harmless event first (so traces have at least one content entry)
printf '{"at":"%s","type":"event","payload":{"msg":"about to misbehave"}}\n' "$(date -Iseconds)"

# The illegal tool call — broker should kill the container right here.
printf '{"at":"%s","type":"tool_call","call":{"tool":"write","args":{"path":"/etc/shadow"}}}\n' "$(date -Iseconds)"

# If somehow we reach here (broker did NOT block), emit "done shipped" so the
# verify recipe can distinguish "not killed" from "killed": a shipped verdict
# here would be a failure, not a success, of the M3 gate.
printf '{"at":"%s","type":"done","verdict":"shipped"}\n' "$(date -Iseconds)"
