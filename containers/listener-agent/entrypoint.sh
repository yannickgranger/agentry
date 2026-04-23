#!/usr/bin/env bash
# listener-agent — M4. Reads team_context.messages from stdin JSON bundle,
# emits one `received` event per incoming message, then `done shipped`.
set -euo pipefail

bundle="$(cat)"
count="$(jq -r '.team_context.messages | length' <<<"$bundle")"

printf '{"at":"%s","type":"event","payload":{"msg":"listener-agent started","received_count":%s}}\n' "$(date -Iseconds)" "$count"

# Iterate over each message and emit a received event.
i=0
while [ "$i" -lt "$count" ]; do
    msg_json="$(jq -c ".team_context.messages[$i]" <<<"$bundle")"
    payload="$(jq -c ".payload" <<<"$msg_json")"
    from="$(jq -r ".from" <<<"$msg_json")"
    printf '{"at":"%s","type":"event","payload":{"received_from":"%s","payload":%s}}\n' "$(date -Iseconds)" "$from" "$payload"
    i=$((i+1))
done

printf '{"at":"%s","type":"done","verdict":"shipped"}\n' "$(date -Iseconds)"
