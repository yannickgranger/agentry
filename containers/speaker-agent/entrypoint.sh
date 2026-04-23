#!/usr/bin/env bash
# speaker-agent — M4. Ignores stdin, emits one Message to "listener-agent",
# then `done shipped`.
set -euo pipefail
cat > /dev/null

printf '{"at":"%s","type":"event","payload":{"msg":"speaker-agent started"}}\n' "$(date -Iseconds)"
printf '{"at":"%s","type":"message","to":"listener-agent","payload":{"greeting":"hello from speaker"}}\n' "$(date -Iseconds)"
printf '{"at":"%s","type":"done","verdict":"shipped"}\n' "$(date -Iseconds)"
