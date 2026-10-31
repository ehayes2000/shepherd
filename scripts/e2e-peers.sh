#!/usr/bin/env bash
# Opt-in, manual: pairs two shepherd hosts on this machine over the real network (n0's relays and pkarr/DNS address
# lookup), then checks that they find each other again by node id alone, with neither keeping the other's last address.
# That is the discovery route the gate can't exercise (T30, decision 0019): the gate never touches the network.
#
# Run by the user, never by the gate or a builder. Each host gets a throwaway HOME, so the real ~/.shepherd is never
# touched; both are stopped (by the pids this script started, never by name) and their HOMEs removed on exit.
#
#   scripts/e2e-peers.sh
set -euo pipefail
cd "$(dirname "$0")/.."

# How long to wait (seconds) for a ticket, and for the hosts to find each other after the restart.
LIMIT=${E2E_PEERS_LIMIT:-90}

cargo build --release --quiet
SHEPHERD=$PWD/target/release/shepherd

# Short paths under /tmp: the host socket path must stay under the OS limit.
A=$(mktemp -d /tmp/shpeerA.XXXXXX)
B=$(mktemp -d /tmp/shpeerB.XXXXXX)
# The hosts' pids, as this script started them.
PID_A=
PID_B=

cleanup() {
  for pid in "$PID_A" "$PID_B"; do
    if [[ -n "$pid" ]]; then
      kill -TERM "$pid" 2>/dev/null || true
      wait "$pid" 2>/dev/null || true
    fi
  done
  rm -rf "$A" "$B"
}
trap cleanup EXIT

say() { printf '==> %s\n' "$1"; }
fail() {
  printf 'FAIL: %s\n' "$1" >&2
  exit 1
}

# Starts the host for HOME $1, called $2, in the background; its pid is left in STARTED.
start_host() {
  mkdir -p -m 700 "$1/.shepherd"
  printf '[machine]\nname = "%s"\n[defaults]\nagent = "shell"\n' "$2" >"$1/.shepherd/config.toml"
  chmod 600 "$1/.shepherd/config.toml"
  HOME=$1 SHELL=/bin/sh "$SHEPHERD" host </dev/null >/dev/null 2>"$1/host.stderr" &
  STARTED=$!
  for _ in $(seq 100); do
    [[ -S "$1/.shepherd/host.sock" ]] && return 0
    sleep 0.1
  done
  fail "the host for $2 didn't start: $(cat "$1/host.stderr")"
}

# Stops the host with pid $1, and waits for it.
stop_host() {
  kill -TERM "$1"
  wait "$1" 2>/dev/null || true
}

# Waits until `shepherd peers` for HOME $1 shows a connected peer called $2.
wait_connected() {
  for _ in $(seq "$LIMIT"); do
    if HOME=$1 "$SHEPHERD" peers | grep -q "^$2 .* connected$"; then
      return 0
    fi
    sleep 1
  done
  HOME=$1 "$SHEPHERD" peers >&2 || true
  fail "$2 never showed as connected from $1"
}

say "starting two hosts: office (A) and laptop (B)"
start_host "$A" office
PID_A=$STARTED
start_host "$B" laptop
PID_B=$STARTED

say "shepherd pair on A"
HOME=$A "$SHEPHERD" pair >"$A/pair.out" 2>&1 &
PAIR=$!
TICKET=
for _ in $(seq "$LIMIT"); do
  TICKET=$(sed -n 's/^ *shepherd join \(.*\)$/\1/p' "$A/pair.out")
  [[ -n "$TICKET" ]] && break
  sleep 1
done
[[ -n "$TICKET" ]] || fail "no ticket: $(cat "$A/pair.out")"
printf 'ticket (%s characters)\n' "${#TICKET}"

say "shepherd join on B"
HOME=$B "$SHEPHERD" join "$TICKET" || fail "join failed"
wait "$PAIR" || fail "pair failed: $(cat "$A/pair.out")"
grep -q '^Paired with laptop' "$A/pair.out" || fail "A didn't pair with laptop: $(cat "$A/pair.out")"
HOME=$A "$SHEPHERD" peers | grep -q '^laptop ' || fail "A doesn't list laptop"
HOME=$B "$SHEPHERD" peers | grep -q '^office ' || fail "B doesn't list office"

say "restarting both with no last addresses: only pkarr/DNS lookup can find the other"
stop_host "$PID_A"
PID_A=
stop_host "$PID_B"
PID_B=
rm -f "$A/.shepherd/peer-addrs.json" "$B/.shepherd/peer-addrs.json"
start_host "$B" laptop
PID_B=$STARTED
# B dials A at once and fails (A is down); A dials B once, when it starts. Give B time to reach its home relay and
# publish its address first: T30 dials each peer once, on start (reconnecting comes with T31).
sleep "${E2E_PEERS_PUBLISH:-15}"
start_host "$A" office
PID_A=$STARTED
wait_connected "$A" laptop
wait_connected "$B" office

say "shepherd unpair on A"
HOME=$A "$SHEPHERD" unpair laptop
HOME=$A "$SHEPHERD" peers | grep -q '^No paired machines' || fail "A still lists a peer"

printf '\nPASS: paired, found again by node id over pkarr/DNS, and unpaired\n'
