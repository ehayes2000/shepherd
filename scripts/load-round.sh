#!/usr/bin/env bash
# Run a test binary under parallel load: COPIES copies at once, ROUNDS times, each copy's output in its own log.
# Logs go to a fresh directory every run (printed at the end), so there is never anything to clear first.
#
#   scripts/load-round.sh [-n COPIES] [-r ROUNDS] BINARY [TEST ARGS...]
#   scripts/load-round.sh -n 24 -r 2 target/debug/deps/shepherd_adapters-abc123 worktrees:: --test-threads=8
#
# Exit status is 0 only if every copy passed. Find the binary with `cargo test --no-run` (it prints the path).
set -uo pipefail

copies=24
rounds=1
while getopts "n:r:" opt; do
  case $opt in
    n) copies=$OPTARG ;;
    r) rounds=$OPTARG ;;
    *) echo "usage: $0 [-n COPIES] [-r ROUNDS] BINARY [TEST ARGS...]" >&2; exit 2 ;;
  esac
done
shift $((OPTIND - 1))
[ $# -ge 1 ] || { echo "usage: $0 [-n COPIES] [-r ROUNDS] BINARY [TEST ARGS...]" >&2; exit 2; }
bin=$1
shift
[ -x "$bin" ] || { echo "load-round: not an executable: $bin" >&2; exit 2; }

load=$(sysctl -n vm.loadavg 2>/dev/null | awk '{print $2}')
echo "load-round: $copies copies x $rounds rounds of $(basename "$bin") (load average ${load:-unknown})"

out=$(mktemp -d "${TMPDIR:-/tmp}/load-round.XXXXXX")
failed=0
for r in $(seq 1 "$rounds"); do
  pids=()
  for i in $(seq 1 "$copies"); do
    "$bin" "$@" >"$out/r$r-$i.log" 2>&1 &
    pids+=($!)
  done
  for i in "${!pids[@]}"; do
    if ! wait "${pids[$i]}"; then
      failed=$((failed + 1))
      echo "load-round: round $r copy $((i + 1)) FAILED: $out/r$r-$((i + 1)).log"
    fi
  done
done

total=$((copies * rounds))
echo "load-round: $((total - failed))/$total passed; logs in $out"
[ "$failed" -eq 0 ]
