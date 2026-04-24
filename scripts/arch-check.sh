#!/usr/bin/env bash
# Local arch check — identical semantics to .gitea/workflows/arch.yml.
# Run this before pushing to surface spec drift and ban-rule violations
# without waiting for CI.
#
# Requirements on $PATH:
#   - cfdb (install: cargo install --git https://agency.lab:3000/yg/cfdb.git --rev $(cat .cfdb/cfdb.rev) --locked cfdb-cli)
#   - graph-specs (install: cargo install --git https://agency.lab:3000/yg/graph-specs-rust.git --rev $(cat .cfdb/graph-specs.rev) --locked application)

set -euo pipefail

cd "$(git rev-parse --show-toplevel)"

echo "==> graph-specs check (concept-level equivalence)"
graph-specs check --specs specs/concepts/ --code crates/

echo "==> cfdb extract (x-ray the workspace)"
DB_DIR=".cfdb/db-local"
rm -rf "$DB_DIR"
cfdb extract --workspace . --db "$DB_DIR" --keyspace agentry

if compgen -G ".cfdb/queries/*.cypher" > /dev/null; then
    echo "==> cfdb violations (ban rules)"
    fail=0
    for rule in .cfdb/queries/*.cypher; do
        printf '  rule: %s ... ' "$(basename "$rule")"
        if cfdb violations --db "$DB_DIR" --keyspace agentry --rule "$rule" 2>&1 | grep -q "^violations: 0 "; then
            echo "ok"
        else
            echo "FAIL"
            cfdb violations --db "$DB_DIR" --keyspace agentry --rule "$rule" || true
            fail=1
        fi
    done
    if [ "$fail" -ne 0 ]; then
        echo "==> one or more ban rules triggered"
        exit 1
    fi
else
    echo "==> cfdb queries directory empty — no ban rules to run yet"
fi

echo "==> arch check passed"
