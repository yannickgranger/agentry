# agentry — dev workflow

# Environment — LOCAL podman Redis. NEVER prod.
# Password lives at ~/.config/agentry/redis.password (600, gitignored).
# Port 6380 to avoid collisions with any other local Redis.
#
# Config is loaded via figment (see config.rs). Load order:
#   1. defaults in code
#   2. ~/.config/agentry/agentry.toml (optional; see agentry.example.toml)
#   3. env vars prefixed AGENTRY_ with nested keys split by __
#
# Env-var names use the figment __ nesting:
#   AGENTRY_REDIS__URL           (maps to redis.url)
#   AGENTRY_DASHBOARD__PORT      (maps to dashboard.port)
#   AGENTRY_SIGNING__KEY_PATH    (maps to signing.key_path)
export AGENTRY_REDIS_PASSWORD := `cat ~/.config/agentry/redis.password 2>/dev/null || echo ""`
export AGENTRY_REDIS__URL := "redis://:" + AGENTRY_REDIS_PASSWORD + "@127.0.0.1:6380"
export AGENTRY_DASHBOARD__PORT := "7800"
export RUST_LOG := env_var_or_default("RUST_LOG", "orchestrator=info,info")

# Default
default:
    @just --list

# Build ra-query into ~/.local/bin/ra-query for the reviewer-claude bind-mount.
# Operator-invoked; idempotent. Reviewer-claude container expects the binary
# at this path (matches the existing ~/.local/bin/claude pattern).
ra-query-binary:
    #!/usr/bin/env bash
    set -euo pipefail
    : ${GITEA_TOKEN:?GITEA_TOKEN must be set}
    CARGO_NET_GIT_FETCH_WITH_CLI=true cargo install \
        --git https://oauth2:${GITEA_TOKEN}@agency.lab:3000/yg/ra-query.git \
        --rev 2200414 --root ~/.local --locked ra-query
    test -x ~/.local/bin/ra-query
    echo "ra-query installed at ~/.local/bin/ra-query"

# Build dead-pub-check into ~/.local/bin/dead-pub-check for the coder-claude
# bind-mount. Operator-invoked; idempotent. Coder-claude container expects the
# binary at this path (matches the existing ~/.local/bin/ra-query pattern).
dead-pub-check-binary:
    #!/usr/bin/env bash
    set -euo pipefail
    cargo install --path crates/coder-precommit --bin dead-pub-check --root ~/.local --locked --quiet
    test -x ~/.local/bin/dead-pub-check
    echo "dead-pub-check installed at ~/.local/bin/dead-pub-check"

# Build ac-verifier into ~/.local/bin/ac-verifier for the
# ac-verifier-claude-agentry bind-mount. Operator-invoked; idempotent.
ac-verifier-binary:
    #!/usr/bin/env bash
    set -euo pipefail
    cargo install --path crates/coder-precommit --bin ac-verifier --root ~/.local --locked --quiet
    test -x ~/.local/bin/ac-verifier
    echo "ac-verifier installed at ~/.local/bin/ac-verifier"

# Build everything
build:
    cargo build --workspace --release

# Run tests
test:
    cargo test --workspace

# Format + lint
check:
    cargo fmt --check
    cargo clippy --workspace --all-targets -- -D warnings

# Fix lint
fix:
    cargo fmt
    cargo clippy --workspace --fix --allow-dirty --allow-staged

# Start local dev Redis container on :6380. Idempotent.
dev-redis-up:
    #!/usr/bin/env bash
    set -euo pipefail
    mkdir -p ~/.config/agentry
    if [ ! -f ~/.config/agentry/redis.password ]; then
        openssl rand -base64 24 | tr -d '\n/+=' > ~/.config/agentry/redis.password
        chmod 600 ~/.config/agentry/redis.password
        echo "generated a new dev redis password"
    fi
    PW=$(cat ~/.config/agentry/redis.password)
    if podman container exists agentry-dev-redis; then
        echo "agentry-dev-redis already exists; (re)starting..."
        podman start agentry-dev-redis > /dev/null
    else
        podman volume inspect agentry-dev-redis-data >/dev/null 2>&1 || podman volume create agentry-dev-redis-data > /dev/null
        podman run -d \
            --name agentry-dev-redis \
            -p 127.0.0.1:6380:6379 \
            -v agentry-dev-redis-data:/data \
            docker.io/library/redis:7-alpine \
            redis-server --requirepass "$PW" --appendonly yes > /dev/null
        echo "created agentry-dev-redis"
    fi
    sleep 1
    redis-cli -h 127.0.0.1 -p 6380 -a "$PW" --no-auth-warning ping

dev-redis-down:
    -podman stop agentry-dev-redis
    -podman rm agentry-dev-redis

# Create the podman network every agentry-spawned container joins, and bring
# up a dedicated `agentry-sccache-redis` container attached to it so roles
# with `sccache=true` can reach the compile cache by DNS name. Orchestratord
# itself runs on the host and reaches agentry-dev-redis via the existing
# 127.0.0.1:6380 port mapping — it doesn't need agentry-net. Idempotent.
agentry-net-up:
    #!/usr/bin/env bash
    set -euo pipefail
    if ! podman network exists agentry-net; then
        podman network create agentry-net > /dev/null
        echo "created podman network agentry-net"
    else
        echo "agentry-net already exists"
    fi
    # agentry-scoped sccache backend. Kept separate from any other `sccache-redis`
    # container so we don't touch cross-team state. No host port mapping — only
    # reachable from containers on agentry-net.
    if podman container exists agentry-sccache-redis; then
        podman start agentry-sccache-redis > /dev/null 2>&1 || true
        echo "agentry-sccache-redis already exists; (re)started"
    else
        podman volume inspect agentry-sccache-data >/dev/null 2>&1 || podman volume create agentry-sccache-data > /dev/null
        podman run -d \
            --name agentry-sccache-redis \
            --network agentry-net \
            -v agentry-sccache-data:/data \
            docker.io/library/redis:7-alpine \
            redis-server --maxmemory 2gb --maxmemory-policy allkeys-lru --appendonly no > /dev/null
        echo "created agentry-sccache-redis on agentry-net"
    fi
    sleep 1
    # Smoke test: reach the cache by DNS name from an ephemeral container on agentry-net.
    podman run --rm --network agentry-net docker.io/library/redis:7-alpine \
        redis-cli -h agentry-sccache-redis -p 6379 ping

agentry-net-down:
    -podman stop agentry-sccache-redis
    -podman rm agentry-sccache-redis
    -podman network rm -f agentry-net

# Start dev infra (orchestratord + dashboard as user processes, podman for agents)
dev-up: dev-redis-up build
    #!/usr/bin/env bash
    set -euo pipefail
    # Ensure Redis is reachable
    redis-cli -u "$AGENTRY_REDIS__URL" ping > /dev/null
    # Seed roles + teams in Redis
    ./target/release/orchestrator seed
    # Start orchestratord + dashboard (background, pids in /tmp/agentry-*.pid)
    ./target/release/orchestratord > /tmp/agentry-orchestratord.log 2>&1 &
    echo $! > /tmp/agentry-orchestratord.pid
    ./target/release/orchestrator-dashboard > /tmp/agentry-dashboard.log 2>&1 &
    echo $! > /tmp/agentry-dashboard.pid
    sleep 1
    echo "orchestratord pid: $(cat /tmp/agentry-orchestratord.pid)"
    echo "dashboard pid:    $(cat /tmp/agentry-dashboard.pid)"
    echo "dashboard:        http://localhost:$AGENTRY_DASHBOARD__PORT"

# Stop dev infra
dev-down:
    #!/usr/bin/env bash
    for svc in orchestratord dashboard; do
      pidfile=/tmp/agentry-$svc.pid
      if [ -f $pidfile ]; then
        pid=$(cat $pidfile)
        kill $pid 2>/dev/null || true
        rm -f $pidfile
        echo "stopped $svc ($pid)"
      fi
    done
    # Kill any stray agentry agent containers
    podman ps --filter "label=agentry.brief" -q | xargs -r podman stop -t 1
    podman ps -a --filter "label=agentry.brief" -q | xargs -r podman rm

# Follow orchestratord logs
logs:
    tail -f /tmp/agentry-orchestratord.log

# Logs for dashboard
logs-dash:
    tail -f /tmp/agentry-dashboard.log

# Verify M0: echo agent end-to-end on real infra
verify-M0:
    #!/usr/bin/env bash
    set -euo pipefail
    ./target/release/orchestrator submit examples/verify-M0.json
    echo "Brief submitted. Waiting for verdict..."
    sleep 5
    redis-cli -h 127.0.0.1 -p 6380 -a "$AGENTRY_REDIS_PASSWORD" --no-auth-warning XREVRANGE agentry:verdicts + - COUNT 1

# Verify M8-chain: brief A's payload.next_brief_ref triggers brief B.
verify-M8-chain:
    #!/usr/bin/env bash
    set -euo pipefail
    # Stage brief B where orchestratord will find it (absolute path, matches chain-A's next_brief_ref).
    cp examples/verify-M8-chain-B.json /tmp/agentry-verify-m8-chain-b.json
    ./target/release/orchestrator submit examples/verify-M8-chain-A.json
    echo "Submitted A. Waiting for chain..."
    sleep 8
    echo "--- last 2 verdicts (expect brf_verify_m8_chain_a + brf_verify_m8_chain_b, both shipped) ---"
    redis-cli -h 127.0.0.1 -p 6380 -a "$AGENTRY_REDIS_PASSWORD" --no-auth-warning XREVRANGE agentry:verdicts + - COUNT 2
    echo ""
    got_b=$(redis-cli -h 127.0.0.1 -p 6380 -a "$AGENTRY_REDIS_PASSWORD" --no-auth-warning XREVRANGE agentry:verdicts + - COUNT 5 | grep -c 'brf_verify_m8_chain_b' || true)
    got_a=$(redis-cli -h 127.0.0.1 -p 6380 -a "$AGENTRY_REDIS_PASSWORD" --no-auth-warning XREVRANGE agentry:verdicts + - COUNT 5 | grep -c 'brf_verify_m8_chain_a' || true)
    [ "$got_a" -gt 0 ] && [ "$got_b" -gt 0 ] && echo "M8-chain verify PASS" || (echo "M8-chain verify FAIL (a=$got_a b=$got_b)"; exit 1)

# Verify M8-webhook: POST /submit with shared-secret token -> brief flows.
# Requires AGENTRY_WEBHOOK__SECRET set in dashboard env AND passed to curl.
verify-M8-webhook:
    #!/usr/bin/env bash
    set -euo pipefail
    if [ -z "${AGENTRY_WEBHOOK__SECRET:-}" ]; then
        echo "AGENTRY_WEBHOOK__SECRET not set; generating one for this run"
        export AGENTRY_WEBHOOK__SECRET=$(openssl rand -hex 16)
        echo "regen'd: $AGENTRY_WEBHOOK__SECRET"
        echo "NOTE: orchestratord/dashboard must also have this in their env; restart with it set"
        exit 2
    fi
    echo "--- POST /submit with bad token (expect 401) ---"
    code=$(curl -sS -o /dev/null -w "%{http_code}" \
        -H "X-Agentry-Token: WRONG" \
        -H "Content-Type: application/json" \
        --data-binary @examples/verify-M8-webhook.json \
        http://localhost:${AGENTRY_DASHBOARD__PORT}/submit)
    [ "$code" = "401" ] && echo "bad token -> 401 OK" || (echo "expected 401, got $code"; exit 1)
    echo "--- POST /submit with good token (expect 200) ---"
    resp=$(curl -sS -X POST \
        -H "X-Agentry-Token: $AGENTRY_WEBHOOK__SECRET" \
        -H "Content-Type: application/json" \
        --data-binary @examples/verify-M8-webhook.json \
        http://localhost:${AGENTRY_DASHBOARD__PORT}/submit)
    echo "$resp"
    sleep 5
    echo "--- verdict (expect kind=shipped for brf_verify_m8_webhook) ---"
    redis-cli -h 127.0.0.1 -p 6380 -a "$AGENTRY_REDIS_PASSWORD" --no-auth-warning XREVRANGE agentry:verdicts + - COUNT 3 | grep -q 'brf_verify_m8_webhook' && echo "M8-webhook verify PASS" || (echo "M8-webhook verify FAIL"; exit 1)

# Verify M7: shipper opens a real PR on yg/agentry-toy.
# Requires GITEA_TOKEN in orchestratord's env (via keepassxc gitea/agency.lab).
verify-M7:
    #!/usr/bin/env bash
    set -euo pipefail
    ./target/release/orchestrator submit examples/verify-M7.json
    echo "Brief submitted. Waiting for shipper to clone/push/PR..."
    sleep 15
    echo "--- verdict (expect kind=shipped) ---"
    redis-cli -h 127.0.0.1 -p 6380 -a "$AGENTRY_REDIS_PASSWORD" --no-auth-warning XREVRANGE agentry:verdicts + - COUNT 1
    echo "--- trace ---"
    redis-cli -h 127.0.0.1 -p 6380 -a "$AGENTRY_REDIS_PASSWORD" --no-auth-warning XRANGE agentry:brief:brf_verify_m7:trace - +
    echo "--- trace must have 'PR opened' with html_url ---"
    redis-cli -h 127.0.0.1 -p 6380 -a "$AGENTRY_REDIS_PASSWORD" --no-auth-warning XRANGE agentry:brief:brf_verify_m7:trace - + \
        | grep -q 'PR opened' && echo "M7 verify PASS" || (echo "M7 verify FAIL"; exit 1)

# Verify M6: synthesizer narrows coder's fs:write scope via permit_overrides.
verify-M6:
    #!/usr/bin/env bash
    set -euo pipefail
    ./target/release/orchestrator submit examples/verify-M6.json
    echo "Brief submitted. Waiting for broker to block out-of-scope write..."
    sleep 6
    echo "--- verdicts for this brief (expect last = permit_violation with fs:write reason) ---"
    redis-cli -h 127.0.0.1 -p 6380 -a "$AGENTRY_REDIS_PASSWORD" --no-auth-warning XREVRANGE agentry:verdicts + - COUNT 3
    echo "--- trace ---"
    redis-cli -h 127.0.0.1 -p 6380 -a "$AGENTRY_REDIS_PASSWORD" --no-auth-warning XRANGE agentry:brief:brf_verify_m6:trace - +
    echo ""
    redis-cli -h 127.0.0.1 -p 6380 -a "$AGENTRY_REDIS_PASSWORD" --no-auth-warning XREVRANGE agentry:verdicts + - COUNT 2 \
        | grep -q 'fs:write scope denied' && echo "M6 verify PASS" || (echo "M6 verify FAIL — expected narrow-scope denial"; exit 1)

# Verify M5b: Claude Max via the host `claude` CLI — no Anthropic API spend.
# Requires: host claude binary + ~/.claude/.credentials.json present (OAuth login done).
verify-M5b:
    #!/usr/bin/env bash
    set -euo pipefail
    ./target/release/orchestrator submit examples/verify-M5b.json
    echo "Brief submitted. Waiting for Claude Max reply (can take ~10s)..."
    sleep 15
    echo "--- verdict (expect kind=shipped) ---"
    redis-cli -h 127.0.0.1 -p 6380 -a "$AGENTRY_REDIS_PASSWORD" --no-auth-warning XREVRANGE agentry:verdicts + - COUNT 1
    echo "--- trace ---"
    redis-cli -h 127.0.0.1 -p 6380 -a "$AGENTRY_REDIS_PASSWORD" --no-auth-warning XRANGE agentry:brief:brf_verify_m5b:trace - +
    echo "--- reply must contain 'pong' ---"
    redis-cli -h 127.0.0.1 -p 6380 -a "$AGENTRY_REDIS_PASSWORD" --no-auth-warning XRANGE agentry:brief:brf_verify_m5b:trace - + \
        | grep -qi 'reply":"pong' && echo "M5b verify PASS" || (echo "M5b verify FAIL"; exit 1)

# Verify M5a: call xAI Grok from inside a container; emit a reply event.
# Requires XAI_API_KEY in orchestratord's env at startup.
verify-M5a:
    #!/usr/bin/env bash
    set -euo pipefail
    ./target/release/orchestrator submit examples/verify-M5a.json
    echo "Brief submitted. Waiting for Grok reply..."
    sleep 10
    echo "--- verdict (expect kind=shipped) ---"
    redis-cli -h 127.0.0.1 -p 6380 -a "$AGENTRY_REDIS_PASSWORD" --no-auth-warning XREVRANGE agentry:verdicts + - COUNT 1
    echo "--- trace ---"
    redis-cli -h 127.0.0.1 -p 6380 -a "$AGENTRY_REDIS_PASSWORD" --no-auth-warning XRANGE agentry:brief:brf_verify_m5a:trace - +
    echo "--- reply must contain 'pong' ---"
    redis-cli -h 127.0.0.1 -p 6380 -a "$AGENTRY_REDIS_PASSWORD" --no-auth-warning XRANGE agentry:brief:brf_verify_m5a:trace - + \
        | grep -qi '"reply":"pong' && echo "M5a verify PASS" || (echo "M5a verify FAIL"; exit 1)

# Verify M4: speaker emits a Message; listener receives it via team_context.
verify-M4:
    #!/usr/bin/env bash
    set -euo pipefail
    ./target/release/orchestrator submit examples/verify-M4.json
    echo "Brief submitted. Waiting for team to complete..."
    sleep 8
    echo "--- verdict (expect kind=shipped) ---"
    redis-cli -h 127.0.0.1 -p 6380 -a "$AGENTRY_REDIS_PASSWORD" --no-auth-warning XREVRANGE agentry:verdicts + - COUNT 1
    echo "--- trace events ---"
    redis-cli -h 127.0.0.1 -p 6380 -a "$AGENTRY_REDIS_PASSWORD" --no-auth-warning XRANGE agentry:brief:brf_verify_m4:trace - +
    echo "--- listener must have a 'received_from=speaker-agent' event ---"
    redis-cli -h 127.0.0.1 -p 6380 -a "$AGENTRY_REDIS_PASSWORD" --no-auth-warning XRANGE agentry:brief:brf_verify_m4:trace - + \
        | grep -q 'received_from":"speaker-agent' && echo "M4 verify PASS" || (echo "M4 verify FAIL"; exit 1)

# Verify M3: naughty agent attempts an unauthorized tool call; broker kills it.
verify-M3:
    #!/usr/bin/env bash
    set -euo pipefail
    ./target/release/orchestrator submit examples/verify-M3.json
    echo "Brief submitted. Waiting for broker to kill the naughty agent..."
    sleep 6
    echo "--- verdict (expect kind=permit_violation) ---"
    redis-cli -h 127.0.0.1 -p 6380 -a "$AGENTRY_REDIS_PASSWORD" --no-auth-warning XREVRANGE agentry:verdicts + - COUNT 1
    echo "--- audit stream (should record the unauthorized attempt) ---"
    redis-cli -h 127.0.0.1 -p 6380 -a "$AGENTRY_REDIS_PASSWORD" --no-auth-warning XRANGE agentry:brief:brf_verify_m3:audit - +
    echo ""
    redis-cli -h 127.0.0.1 -p 6380 -a "$AGENTRY_REDIS_PASSWORD" --no-auth-warning XREVRANGE agentry:verdicts + - COUNT 1 \
        | grep -q '"kind":"permit_violation"' && echo "M3 verify PASS" || (echo "M3 verify FAIL — expected permit_violation"; exit 1)

# Verify M2: create a role + team via dashboard forms, run a brief on them.
verify-M2:
    #!/usr/bin/env bash
    set -euo pipefail
    # 1. Create "printer" role via POST /roles (dashboard must be running).
    curl -sS -X POST "http://localhost:${AGENTRY_DASHBOARD__PORT}/roles" \
        --data-urlencode "name=printer" \
        --data-urlencode "model=" \
        --data-urlencode "image=localhost/agentry/echo-agent:v1" \
        --data-urlencode "substrate_class=podman" \
        --data-urlencode "system_prompt=" \
        --data-urlencode "binaries_csv=" \
        --data-urlencode "tool_allowlist_csv=" \
        --data-urlencode "permit_scope_lines=net:deny:*" \
        --data-urlencode "mcp_servers_json=" \
        -o /dev/null -w "POST /roles -> HTTP %{http_code} (expect 303)\n"
    # 2. Create "printer-team" referencing the printer role.
    curl -sS -X POST "http://localhost:${AGENTRY_DASHBOARD__PORT}/teams" \
        --data-urlencode "name=printer-team" \
        --data-urlencode "roles_csv=printer" \
        --data-urlencode "graph_lines=" \
        --data-urlencode "terminal_role=printer" \
        --data-urlencode "max_retries=0" \
        -o /dev/null -w "POST /teams -> HTTP %{http_code} (expect 303)\n"
    # 3. Confirm records visible on dashboard listing pages.
    echo "--- /roles contains 'printer'? ---"
    curl -sS "http://localhost:${AGENTRY_DASHBOARD__PORT}/roles" | grep -c ">printer<" || (echo "NOT FOUND"; exit 1)
    echo "--- /teams contains 'printer-team'? ---"
    curl -sS "http://localhost:${AGENTRY_DASHBOARD__PORT}/teams" | grep -c ">printer-team<" || (echo "NOT FOUND"; exit 1)
    # 4. Submit the M2 brief (team: printer-team).
    ./target/release/orchestrator submit examples/verify-M2.json
    echo "Brief submitted. Waiting for verdict..."
    sleep 5
    echo "--- verdict ---"
    redis-cli -h 127.0.0.1 -p 6380 -a "$AGENTRY_REDIS_PASSWORD" --no-auth-warning XREVRANGE agentry:verdicts + - COUNT 1
    echo "M2 verify PASS"

# Verify M1: replay a brief and check the dashboard renders it.
verify-M1:
    #!/usr/bin/env bash
    set -euo pipefail
    ./target/release/orchestrator submit examples/verify-M1.json
    echo "Brief submitted. Waiting for verdict..."
    sleep 5
    echo "--- verdict on Redis ---"
    redis-cli -h 127.0.0.1 -p 6380 -a "$AGENTRY_REDIS_PASSWORD" --no-auth-warning XREVRANGE agentry:verdicts + - COUNT 1
    echo ""
    echo "--- dashboard index reachable? ---"
    curl -sS http://localhost:${AGENTRY_DASHBOARD__PORT}/healthz
    echo ""
    echo "--- index HTML contains brf_verify_m1? ---"
    curl -sS http://localhost:${AGENTRY_DASHBOARD__PORT}/ | grep -c "brf_verify_m1" || (echo "NOT FOUND"; exit 1)
    echo "M1 verify PASS"

# Tail verdicts stream
verdicts:
    redis-cli -u "$AGENTRY_REDIS__URL" XREAD COUNT 20 STREAMS agentry:verdicts 0

# Kill all active briefs (kill switch)
abort-all:
    ./target/release/orchestrator abort --all

# Clean everything
clean:
    just dev-down
    cargo clean
    redis-cli -u "$AGENTRY_REDIS__URL" KEYS 'agentry:*' | xargs -r redis-cli -u "$AGENTRY_REDIS__URL" DEL
