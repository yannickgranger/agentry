#!/usr/bin/env bash
# captain-redeploy — operator-mediated daemon + runners swap.
#
# Runs the full post-merge redeploy ceremony documented in
# docs/captain-doctrine.md, end-to-end:
#   1. Pre-flight (no in-flight non-zombie briefs, no role containers).
#   2. Backup running orchestratord image to /tmp.
#   3. Fetch + ff-only pull on develop (auto-stashes uncommitted edits
#      on the touched files and pops them back after the pull).
#   4. cargo build --release for orchestrator + orchestratord + the
#      claude-using runners (reviewer-claude, coder-claude).
#   5. SIGTERM the running daemon (5s graceful), SIGKILL fallback.
#   6. Relaunch with the captured GITEA_TOKEN + redis pw + signing
#      key path. nohup detached.
#   7. Wait for "signing key loaded" log marker (timeout 10s).
#   8. cargo install --path crates/agentry-role-runtime --bin
#      reviewer-claude-runner --root ~/.local and the same for
#      coder-claude-runner — these are the binaries the role
#      containers bind-mount, NOT the daemon-side runners.
#
# Exits non-zero on any step failure. Prints the new daemon PID and
# the backup binary path at the end so the operator can roll back via
# `cp $BACKUP /var/mnt/workspaces/agentry/target/release/orchestratord`
# if the new daemon misbehaves.
#
# Usage: scripts/captain-redeploy.sh [--no-build] [--no-runners] [--force]
#   --no-build    skip cargo build (binaries already current)
#   --no-runners  skip cargo install for the runner binaries
#   --force       skip the in-flight brief pre-flight check (DANGEROUS
#                 — running briefs lose their daemon and orphan)

set -euo pipefail

NO_BUILD=false
NO_RUNNERS=false
FORCE=false
while [[ $# -gt 0 ]]; do
    case "$1" in
        --no-build) NO_BUILD=true; shift ;;
        --no-runners) NO_RUNNERS=true; shift ;;
        --force) FORCE=true; shift ;;
        *) echo "unknown arg: $1" >&2; exit 64 ;;
    esac
done

WORKSPACE="${AGENTRY_WORKSPACE:-/var/mnt/workspaces/agentry}"
LOCAL_BIN="${HOME}/.local/bin"
LOG_FILE="/tmp/agentry-orchestratord.log"
REDIS_PASS_FILE="${HOME}/.config/agentry/redis.password"
SIGNING_KEY="${HOME}/.config/agentry/signing.key"
REDIS_PORT=6380

for f in "$REDIS_PASS_FILE" "$SIGNING_KEY"; do
    test -f "$f" || { echo "missing required file: $f" >&2; exit 1; }
done
test -d "$WORKSPACE" || { echo "missing workspace: $WORKSPACE" >&2; exit 1; }

PW=$(cat "$REDIS_PASS_FILE")
OLDPID=$(pgrep -x orchestratord || true)
[ -n "$OLDPID" ] || { echo "no orchestratord process found; nothing to swap" >&2; exit 1; }
echo "==> running daemon: pid=$OLDPID"

echo "==> pre-flight: in-flight briefs"
INFLIGHT_COUNT=0
while IFS= read -r key; do
    [ -n "$key" ] || continue
    state=$(redis-cli -p "$REDIS_PORT" -a "$PW" --no-auth-warning GET "$key" 2>/dev/null || true)
    [ -n "$state" ] || continue
    kind=$(printf '%s' "$state" | python3 -c 'import sys,json; print(json.loads(sys.stdin.read())["state"].get("kind",""))' 2>/dev/null || true)
    case "$kind" in
        ""|shipped|failed) ;;
        *) echo "  in-flight: $key => $kind"; INFLIGHT_COUNT=$((INFLIGHT_COUNT+1)) ;;
    esac
done < <(redis-cli -p "$REDIS_PORT" -a "$PW" --no-auth-warning KEYS 'agentry:brief:*:state' 2>/dev/null)
if [ "$INFLIGHT_COUNT" -gt 0 ] && [ "$FORCE" != true ]; then
    echo "==> abort: $INFLIGHT_COUNT non-terminal briefs in-flight. Wait for them or pass --force." >&2
    echo "   (zombie states from pre-fix daemons are also flagged; use --force after a visual review.)" >&2
    exit 2
fi

echo "==> pre-flight: role containers"
ROLE_CONTAINERS=$(podman ps --format '{{.Names}}' 2>/dev/null | grep '^agentry-agt_' || true)
if [ -n "$ROLE_CONTAINERS" ] && [ "$FORCE" != true ]; then
    echo "==> abort: role containers running" >&2
    echo "$ROLE_CONTAINERS" | sed 's/^/   /' >&2
    exit 2
fi

echo "==> capture GITEA_TOKEN from running daemon env"
TOKEN=$(tr '\0' '\n' < "/proc/$OLDPID/environ" | grep '^GITEA_TOKEN=' | cut -d= -f2- || true)
[ -n "$TOKEN" ] || { echo "GITEA_TOKEN not in running daemon env; aborting (set it before relaunch)" >&2; exit 1; }

echo "==> backup running daemon image to /tmp"
BACKUP="/tmp/orchestratord.backup-$(date -u +%Y%m%dT%H%M%SZ)-pid$OLDPID"
cp "/proc/$OLDPID/exe" "$BACKUP"
echo "   $BACKUP"

echo "==> fetch + ff-only pull on develop"
cd "$WORKSPACE"
git fetch --quiet origin develop
# stash uncommitted edits on the dirty files so the pull can ff. Pop after.
DIRTY_FILES=$(git diff --name-only HEAD 2>/dev/null || true)
STASHED=false
if [ -n "$DIRTY_FILES" ]; then
    echo "==> stashing uncommitted edits:"
    printf '%s\n' "$DIRTY_FILES" | sed 's/^/   /'
    git stash push -m "captain-redeploy-tmp" -- $DIRTY_FILES >/dev/null
    STASHED=true
fi
git pull --ff-only --quiet origin develop
if [ "$STASHED" = true ]; then
    git stash pop --quiet
fi
echo "==> develop tip: $(git log --oneline -1 HEAD)"

if [ "$NO_BUILD" != true ]; then
    echo "==> cargo build --release (daemon + cli + claude-using runners)"
    cargo build --release \
        --bin orchestrator --bin orchestratord \
        --bin reviewer-claude-runner --bin coder-claude-runner
fi

echo "==> SIGTERM daemon $OLDPID (5s graceful)"
kill -TERM "$OLDPID"
for _ in 1 2 3 4 5; do
    kill -0 "$OLDPID" 2>/dev/null || break
    sleep 1
done
if kill -0 "$OLDPID" 2>/dev/null; then
    echo "==> daemon did not exit; SIGKILL"
    kill -9 "$OLDPID"
    sleep 1
fi

echo "==> relaunch daemon"
nohup env \
    AGENTRY_REDIS__URL="redis://:${PW}@127.0.0.1:${REDIS_PORT}" \
    AGENTRY_REDIS_PASSWORD="$PW" \
    AGENTRY_SIGNING__KEY_PATH="$SIGNING_KEY" \
    GITEA_TOKEN="$TOKEN" \
    PATH="$PATH" \
    HOME="$HOME" \
    "$WORKSPACE/target/release/orchestratord" \
    > "$LOG_FILE" 2>&1 &
disown
unset TOKEN

echo "==> wait for boot marker"
for _ in 1 2 3 4 5 6 7 8 9 10; do
    sleep 1
    if grep -q 'signing key loaded' "$LOG_FILE" 2>/dev/null; then
        break
    fi
done
if ! grep -q 'signing key loaded' "$LOG_FILE" 2>/dev/null; then
    echo "==> BOOT FAILED — daemon did not log 'signing key loaded' within 10s" >&2
    tail -20 "$LOG_FILE" >&2
    echo "==> rollback: cp $BACKUP $WORKSPACE/target/release/orchestratord && rerun" >&2
    exit 3
fi
NEWPID=$(pgrep -x orchestratord)
echo "==> daemon swapped: $OLDPID → $NEWPID"

if [ "$NO_RUNNERS" != true ]; then
    echo "==> deploy claude-using runner binaries into ~/.local/bin"
    cargo install --path crates/agentry-role-runtime \
        --bin reviewer-claude-runner --root "$HOME/.local" --locked --quiet
    cargo install --path crates/agentry-role-runtime \
        --bin coder-claude-runner --root "$HOME/.local" --locked --quiet
    echo "   $LOCAL_BIN/reviewer-claude-runner"
    echo "   $LOCAL_BIN/coder-claude-runner"
fi

echo "==> done. backup=$BACKUP newpid=$NEWPID"
