#!/usr/bin/env bash
# The gate. Every commit, every agent, CI: same script.
set -euo pipefail
cd "$(dirname "$0")/.."

step() { printf '\n==> %s\n' "$1"; }

step "fmt"
cargo fmt --all -- --check

step "clippy"
cargo clippy --workspace --all-targets -- -D warnings

step "test"
cargo test --workspace --quiet

step "architecture"
scripts/check-arch.sh

step "tasks"
python3 scripts/tasks.py --check
echo "tasks ok"

printf '\n==> gate passed\n'
