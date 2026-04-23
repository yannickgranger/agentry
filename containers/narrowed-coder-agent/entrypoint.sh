#!/usr/bin/env bash
# narrowed-coder-agent — M6. The role's base permit has fs:write:/workspace/**,
# but the synthesizer narrowed it to fs:write:/workspace/allowed.rs only.
# This agent attempts a write to /workspace/denied.rs — should be blocked.
set -euo pipefail
cat > /dev/null

printf '{"at":"%s","type":"event","payload":{"msg":"narrowed-coder starting"}}\n' "$(date -Iseconds)"
printf '{"at":"%s","type":"tool_call","call":{"tool":"write","args":{"path":"/workspace/denied.rs","content":"// should not land"}}}\n' "$(date -Iseconds)"
# If we reach here, broker didn't block — that's a regression.
printf '{"at":"%s","type":"event","payload":{"msg":"NOT BLOCKED — M6 regression if you see this"}}\n' "$(date -Iseconds)"
printf '{"at":"%s","type":"done","verdict":"shipped"}\n' "$(date -Iseconds)"
