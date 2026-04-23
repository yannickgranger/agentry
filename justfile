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
