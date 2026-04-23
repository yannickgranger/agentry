#!/usr/bin/env bash
# shipper-agent — M7. Reads {repo, branch, file, content, commit_msg,
# pr_title, pr_body} from brief.payload. Clones the forge repo with
# GITEA_TOKEN, creates the branch, writes the file, commits, pushes,
# opens a PR. Emits event per phase + done shipped.
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

emit_event() {
    jq -nc --arg at "$(date -Iseconds)" --argjson p "$1" '{at:$at, type:"event", payload:$p}'
}
emit_done() {
    jq -nc --arg at "$(date -Iseconds)" --arg v "$1" '{at:$at, type:"done", verdict:$v}'
}

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

# Write the file (create parent dirs if needed).
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
