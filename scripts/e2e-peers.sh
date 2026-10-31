#!/usr/bin/env bash
# Opt-in, manual: pairs two shepherd hosts on this machine over the real network (n0's relays and pkarr/DNS address
# lookup), then checks that they find each other again by node id alone, with neither keeping the other's last address.
# That is the discovery route the gate can't exercise (T30, decision 0019): the gate never touches the network. Then a
# client of the one tunnels through it to the other and catches up on a multi-MB history over the network (T31a).
#
# Run by the user, never by the gate or a builder. Each host gets a throwaway HOME, so the real ~/.shepherd is never
# touched; both are stopped (by the pids this script started, never by name) and their HOMEs removed on exit.
#
#   scripts/e2e-peers.sh
set -euo pipefail
cd "$(dirname "$0")/.."

# How long to wait (seconds) for a ticket, and for the hosts to find each other after the restart.
LIMIT=${E2E_PEERS_LIMIT:-90}
# How much history (MB) the tunnelled client catches up on.
HISTORY_MB=${E2E_PEERS_HISTORY_MB:-8}

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
# publish its address first, so A's first dial finds it (a failed dial is retried with backoff, but from up to a
# minute apart).
sleep "${E2E_PEERS_PUBLISH:-15}"
start_host "$A" office
PID_A=$STARTED
wait_connected "$A" laptop
wait_connected "$B" office

say "tunnel: a client of A catches up on ${HISTORY_MB} MB of B's history over the network (T31a)"
B_ID=$(HOME=$A "$SHEPHERD" peers | awk '$1 == "laptop" { print $2 }')
[[ -n "$B_ID" ]] || fail "A doesn't know laptop's id"
python3 - "$B/.shepherd/host.sock" "$A/.shepherd/host.sock" "$B_ID" "$HISTORY_MB" <<'PY' || fail "the tunnel catch-up failed"
import json, socket, sys, time

b_sock, a_sock, b_id, mb = sys.argv[1], sys.argv[2], sys.argv[3], int(sys.argv[4])
HELLO = {"version": 1, "machine": "e2e"}

class Conn:
    def __init__(self, path):
        self.s = socket.socket(socket.AF_UNIX)
        self.s.connect(path)
        self.f = self.s.makefile("rb")
        self.next = 0

    def request(self, method, params):
        self.next += 1
        msg = {"jsonrpc": "2.0", "id": self.next, "method": method, "params": params}
        self.s.sendall(json.dumps(msg).encode() + b"\n")
        return self.next

    def answer(self, id):
        while True:
            msg = json.loads(self.f.readline())
            if msg.get("id") == id:
                if "error" in msg:
                    sys.exit(f"request {id} failed: {msg['error']}")
                return msg["result"]

# B's own history: one session renamed until it holds `mb` megabytes of names.
b = Conn(b_sock)
b.answer(b.request("_shepherd/hello", HELLO))
session = b.answer(b.request("_shepherd/session/new", {}))["session"]
name = "n" * 8192
last = None
for i in range(mb * 1024 * 1024 // len(name)):
    last = b.request("_shepherd/session/rename", {"session": session, "name": f"{i}-{name}"})
b.answer(last)

a = Conn(a_sock)
start = time.monotonic()
a.answer(a.request("_shepherd/tunnel", {"machine": b_id}))
opened = time.monotonic()
hello = a.answer(a.request("_shepherd/hello", HELLO))
assert hello["machine"] == b_id, hello
sub = a.request("_shepherd/subscribe", {"from": {}})
entries = size = 0
while True:
    line = a.f.readline()
    if not line:
        sys.exit("the tunnel closed during the catch-up")
    msg = json.loads(line)
    if msg.get("id") == sub:
        break
    if msg.get("method") == "_shepherd/log/entry":
        entries += 1
        size += len(line)
done = time.monotonic()
print(f"tunnel open in {opened - start:.2f} s; {entries} entries, {size / 1e6:.1f} MB in {done - opened:.1f} s")
PY

say "shepherd unpair on A"
HOME=$A "$SHEPHERD" unpair laptop
HOME=$A "$SHEPHERD" peers | grep -q '^No paired machines' || fail "A still lists a peer"

printf '\nPASS: paired, found again by node id over pkarr/DNS, tunnelled, and unpaired\n'
