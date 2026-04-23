#!/usr/bin/env bash
# synthesizer-agent — M6. Emits a Message to "narrowed-coder" whose payload
# carries `permit_overrides`. The orchestrator extracts it and narrows the
# coder's permit before spawn.
set -euo pipefail
cat > /dev/null

printf '{"at":"%s","type":"event","payload":{"msg":"synthesizer producing contract"}}\n' "$(date -Iseconds)"
printf '{"at":"%s","type":"message","to":"narrowed-coder","payload":{"files_to_touch":["/workspace/allowed.rs"],"permit_overrides":{"fs_write":["/workspace/allowed.rs"]}}}\n' "$(date -Iseconds)"
printf '{"at":"%s","type":"done","verdict":"shipped"}\n' "$(date -Iseconds)"
