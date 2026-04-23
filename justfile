# agentry — dev workflow

# Environment
export AGENTRY_REDIS_URL := "redis://:RedisRationalized2026@192.168.1.152:6379"
export AGENTRY_DASHBOARD_PORT := "7800"
export RUST_LOG := env_var_or_default("RUST_LOG", "orchestrator=info,info")

# Default
default:
    @just --list

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

# Build echo-agent container image (M0)
build-echo:
    cd containers/echo-agent && podman build -t agentry/echo-agent:v1 -f Containerfile .

# Build naughty-agent container image (M3)
build-naughty:
    cd containers/naughty-agent && podman build -t agentry/naughty-agent:v1 -f Containerfile .

# Build M4 speaker + listener images
build-m4:
    cd containers/speaker-agent && podman build -t agentry/speaker-agent:v1 -f Containerfile .
    cd containers/listener-agent && podman build -t agentry/listener-agent:v1 -f Containerfile .

# Start dev infra (orchestratord + dashboard as user processes, podman for agents)
dev-up: build build-echo
    #!/usr/bin/env bash
    set -euo pipefail
    # Ensure Redis is reachable
    redis-cli -u "$AGENTRY_REDIS_URL" ping > /dev/null
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
    echo "dashboard:        http://localhost:$AGENTRY_DASHBOARD_PORT"

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
    redis-cli -h 192.168.1.152 -p 6379 -a RedisRationalized2026 --no-auth-warning XREVRANGE agentry:verdicts + - COUNT 1

# Verify M4: speaker emits a Message; listener receives it via team_context.
verify-M4:
    #!/usr/bin/env bash
    set -euo pipefail
    ./target/release/orchestrator submit examples/verify-M4.json
    echo "Brief submitted. Waiting for team to complete..."
    sleep 8
    echo "--- verdict (expect kind=shipped) ---"
    redis-cli -h 192.168.1.152 -p 6379 -a RedisRationalized2026 --no-auth-warning XREVRANGE agentry:verdicts + - COUNT 1
    echo "--- trace events ---"
    redis-cli -h 192.168.1.152 -p 6379 -a RedisRationalized2026 --no-auth-warning XRANGE agentry:brief:brf_verify_m4:trace - +
    echo "--- listener must have a 'received_from=speaker-agent' event ---"
    redis-cli -h 192.168.1.152 -p 6379 -a RedisRationalized2026 --no-auth-warning XRANGE agentry:brief:brf_verify_m4:trace - + \
        | grep -q 'received_from":"speaker-agent' && echo "M4 verify PASS" || (echo "M4 verify FAIL"; exit 1)

# Verify M3: naughty agent attempts an unauthorized tool call; broker kills it.
verify-M3:
    #!/usr/bin/env bash
    set -euo pipefail
    ./target/release/orchestrator submit examples/verify-M3.json
    echo "Brief submitted. Waiting for broker to kill the naughty agent..."
    sleep 6
    echo "--- verdict (expect kind=permit_violation) ---"
    redis-cli -h 192.168.1.152 -p 6379 -a RedisRationalized2026 --no-auth-warning XREVRANGE agentry:verdicts + - COUNT 1
    echo "--- audit stream (should record the unauthorized attempt) ---"
    redis-cli -h 192.168.1.152 -p 6379 -a RedisRationalized2026 --no-auth-warning XRANGE agentry:brief:brf_verify_m3:audit - +
    echo ""
    redis-cli -h 192.168.1.152 -p 6379 -a RedisRationalized2026 --no-auth-warning XREVRANGE agentry:verdicts + - COUNT 1 \
        | grep -q '"kind":"permit_violation"' && echo "M3 verify PASS" || (echo "M3 verify FAIL — expected permit_violation"; exit 1)

# Verify M2: create a role + team via dashboard forms, run a brief on them.
verify-M2:
    #!/usr/bin/env bash
    set -euo pipefail
    # 1. Create "printer" role via POST /roles (dashboard must be running).
    curl -sS -X POST "http://localhost:${AGENTRY_DASHBOARD_PORT}/roles" \
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
    curl -sS -X POST "http://localhost:${AGENTRY_DASHBOARD_PORT}/teams" \
        --data-urlencode "name=printer-team" \
        --data-urlencode "roles_csv=printer" \
        --data-urlencode "graph_lines=" \
        --data-urlencode "terminal_role=printer" \
        --data-urlencode "max_retries=0" \
        -o /dev/null -w "POST /teams -> HTTP %{http_code} (expect 303)\n"
    # 3. Confirm records visible on dashboard listing pages.
    echo "--- /roles contains 'printer'? ---"
    curl -sS "http://localhost:${AGENTRY_DASHBOARD_PORT}/roles" | grep -c ">printer<" || (echo "NOT FOUND"; exit 1)
    echo "--- /teams contains 'printer-team'? ---"
    curl -sS "http://localhost:${AGENTRY_DASHBOARD_PORT}/teams" | grep -c ">printer-team<" || (echo "NOT FOUND"; exit 1)
    # 4. Submit the M2 brief (team: printer-team).
    ./target/release/orchestrator submit examples/verify-M2.json
    echo "Brief submitted. Waiting for verdict..."
    sleep 5
    echo "--- verdict ---"
    redis-cli -h 192.168.1.152 -p 6379 -a RedisRationalized2026 --no-auth-warning XREVRANGE agentry:verdicts + - COUNT 1
    echo "M2 verify PASS"

# Verify M1: replay a brief and check the dashboard renders it.
verify-M1:
    #!/usr/bin/env bash
    set -euo pipefail
    ./target/release/orchestrator submit examples/verify-M1.json
    echo "Brief submitted. Waiting for verdict..."
    sleep 5
    echo "--- verdict on Redis ---"
    redis-cli -h 192.168.1.152 -p 6379 -a RedisRationalized2026 --no-auth-warning XREVRANGE agentry:verdicts + - COUNT 1
    echo ""
    echo "--- dashboard index reachable? ---"
    curl -sS http://localhost:${AGENTRY_DASHBOARD_PORT}/healthz
    echo ""
    echo "--- index HTML contains brf_verify_m1? ---"
    curl -sS http://localhost:${AGENTRY_DASHBOARD_PORT}/ | grep -c "brf_verify_m1" || (echo "NOT FOUND"; exit 1)
    echo "M1 verify PASS"

# Tail verdicts stream
verdicts:
    redis-cli -u "$AGENTRY_REDIS_URL" XREAD COUNT 20 STREAMS agentry:verdicts 0

# Kill all active briefs (kill switch)
abort-all:
    ./target/release/orchestrator abort --all

# Clean everything
clean:
    just dev-down
    cargo clean
    redis-cli -u "$AGENTRY_REDIS_URL" KEYS 'agentry:*' | xargs -r redis-cli -u "$AGENTRY_REDIS_URL" DEL
