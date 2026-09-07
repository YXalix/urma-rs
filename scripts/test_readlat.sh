#!/bin/bash
# read_lat runner: TCP-vs-URMA READ-semantics latency over a size sweep.
#
# Part 1 (always runs, no URMA device needed): TCP request/response loopback
# on this machine — a smoke test of the protocol, pattern verification and
# the stats path.
#
# Part 2 (only with a two-node list, same config as test_ub.sh): deploy the
# read_lat binary to both nodes and run the full matrix between the SAME
# pair of machines so the two tables are comparable:
#   a) TCP:  serve-tcp on nodeA, read-tcp from nodeB (over the IP fabric)
#   b) URMA: serve-urma on nodeA, read-urma from nodeB via the pasted
#            [desc] line (over the UB fabric, blob import path)
#
# Node list: UB_NODES="ipA ipB" or scripts/ub_nodes.txt (see test_ub.sh).
# Env knobs (passed to both sides):
#   SIZES / ITERS / WARMUP  benchmark knobs, passed to readers AND serves
#                           (SIZES also sizes the serve-side buffer to its max)
#   BUFLEN                  explicit serve-side buffer bytes (rarely needed)
#   PORT                    TCP port (default 13860)
#   DEV / DEV_A / DEV_B     URMA device per node (default: probe via
#                           list_devices; URMA part is skipped when absent)
#   TMO / SSH_OPTS          like test_ub.sh
set -u
cd "$(dirname "$0")/.."

cargo build --examples || exit 1
BIN=./target/debug/examples/read_lat

BENCH=()
[ -n "${SIZES:-}" ]   && BENCH+=(--sizes "$SIZES")
[ -n "${ITERS:-}" ]   && BENCH+=(--iters "$ITERS")
[ -n "${WARMUP:-}" ] && BENCH+=(--warmup "$WARMUP")
PORT=${PORT:-13860}
TMO=${TMO:-120}
SSH_OPTS="${SSH_OPTS:--o BatchMode=yes -o StrictHostKeyChecking=accept-new}"

SRV=()
[ -n "${SIZES:-}" ]   && SRV+=(--sizes "$SIZES")
[ -n "${BUFLEN:-}" ] && SRV+=(--buf-len "$BUFLEN")

FAIL=0

wait_for_line() { # <logfile> <fixed string> <max-seconds>
    local i
    for ((i = 0; i < $3; i++)); do
        grep -qF "$2" "$1" 2>/dev/null && return 0
        sleep 1
    done
    return 1
}

# --- 1) local TCP loopback (no device needed) --------------------------------
echo "== read_lat: tcp loopback (127.0.0.1) =="
LOG=$(mktemp /tmp/read_lat.local.XXXXXX.log)
"$BIN" serve-tcp --port "$PORT" "${SRV[@]}" >/dev/null 2>&1 &
SRV=$!
"$BIN" read-tcp --addr 127.0.0.1 --port "$PORT" "${BENCH[@]}" | tee "$LOG"
kill "$SRV" 2>/dev/null
wait "$SRV" 2>/dev/null
if grep -qF '[read-tcp] done:' "$LOG"; then
    echo "PASS: read_lat tcp loopback"
else
    echo "FAIL: read_lat tcp loopback (no done line; log $LOG)"
    FAIL=1
fi
rm -f "$LOG"

# --- 2) two-node matrix -------------------------------------------------------
NODES=()
if [ -n "${UB_NODES:-}" ]; then
    read -r -a NODES <<< "$UB_NODES"
elif [ -f scripts/ub_nodes.txt ]; then
    while read -r line; do
        line=${line%%#*}
        [ -n "${line// /}" ] && NODES+=($line)
    done < scripts/ub_nodes.txt
fi
if [ "${#NODES[@]}" -lt 2 ]; then
    echo "SKIP: two-node TCP/URMA matrix not configured (set UB_NODES=\"ipA ipB\" or fill scripts/ub_nodes.txt)"
    exit $FAIL
fi
A=${NODES[0]}
B=${NODES[1]}
IP_A=${A##*@}   # address nodeB dials (strip user@ prefix)
echo "UB nodes: $A (serve/nodeA) + $B (read/nodeB)"

# shellcheck disable=SC2086
deploy() {  # <node> -> remote dir on stdout
    local rdir
    rdir=$(ssh $SSH_OPTS "$1" "mktemp -d /tmp/urma_rs_rl.XXXXXX") || return 1
    local b
    for b in read_lat list_devices; do
        # shellcheck disable=SC2086
        scp $SSH_OPTS -q "target/debug/examples/$b" "$1:$rdir/" || return 1
    done
    echo "$rdir"
}
RDIR_A=$(deploy "$A") || { echo "FAIL: cannot deploy to $A (ssh/scp)"; exit 1; }
RDIR_B=$(deploy "$B") || { echo "FAIL: cannot deploy to $B (ssh/scp)"; exit 1; }

LOGDIR=$(mktemp -d /tmp/read_lat.nodes.XXXXXX)
cleanup() {
    rm -rf "$LOGDIR"
    # shellcheck disable=SC2086
    ssh $SSH_OPTS "$A" "rm -rf '$RDIR_A'" 2>/dev/null
    # shellcheck disable=SC2086
    ssh $SSH_OPTS "$B" "rm -rf '$RDIR_B'" 2>/dev/null
}
trap cleanup EXIT

run_node() {  # <node> <rdir> <logfile> <cmd...>
    local node=$1 rdir=$2 log=$3; shift 3
    # shellcheck disable=SC2086
    ssh $SSH_OPTS "$node" "cd '$rdir' && timeout -k 5 $TMO ./$*" >"$LOGDIR/$log" 2>&1
}

# --- 2a) TCP across the nodes -------------------------------------------------
echo "== read_lat: tcp across nodes ($B -> $IP_A) =="
run_node "$A" "$RDIR_A" tcp.serve.log read_lat serve-tcp --port "$PORT" "${SRV[@]}" &
PA=$!
if wait_for_line "$LOGDIR/tcp.serve.log" "listening on" 15; then
    run_node "$B" "$RDIR_B" tcp.read.log read_lat read-tcp --addr "$IP_A" --port "$PORT" "${BENCH[@]}"
    grep -qF '[read-tcp] done:' "$LOGDIR/tcp.read.log" || { echo "MISSING: read-tcp done line"; FAIL=1; }
else
    echo "FAIL: serve-tcp on $A never listened (see $LOGDIR/tcp.serve.log)"
    FAIL=1
fi
kill "$PA" 2>/dev/null
wait "$PA" 2>/dev/null

# --- 2b) URMA across the nodes (needs a device on both) ----------------------
probe_dev() {  # <node> <rdir> -> first device name on stdout (empty if none)
    # shellcheck disable=SC2086
    ssh $SSH_OPTS "$1" "'$2/list_devices' 2>/dev/null | head -1"
}
DEV_A=${DEV_A:-${DEV:-$(probe_dev "$A" "$RDIR_A")}}
DEV_B=${DEV_B:-${DEV:-$(probe_dev "$B" "$RDIR_B")}}

if [ -n "$DEV_A" ] && [ -n "$DEV_B" ]; then
    echo "== read_lat: urma across nodes ($B reads $A; devices $DEV_A / $DEV_B) =="
    run_node "$A" "$RDIR_A" urma.serve.log read_lat serve-urma -d "$DEV_A" "${SRV[@]}" &
    PS=$!
    # serve-urma prints the descriptor as one hex line once its resources are
    # up; play the human: grab it from the log and pass it to nodeB's reader
    DESC=""
    for _ in $(seq 1 30); do
        DESC=$(sed -n 's/^\[desc\] //p' "$LOGDIR/urma.serve.log" 2>/dev/null | head -1)
        [ -n "$DESC" ] && break
        sleep 1
    done
    if [ -z "$DESC" ]; then
        echo "MISSING: no [desc] line from serve-urma on $A (see $LOGDIR/urma.serve.log)"
        FAIL=1
    else
        run_node "$B" "$RDIR_B" urma.read.log read_lat read-urma -d "$DEV_B" "$DESC" "${BENCH[@]}"
        grep -qF '[read-urma] done:' "$LOGDIR/urma.read.log" || { echo "MISSING: read-urma done line"; FAIL=1; }
    fi
    kill "$PS" 2>/dev/null
    wait "$PS" 2>/dev/null
else
    echo "SKIP: urma part (no URMA device on $A or $B); the cross-node TCP part above still ran"
fi

# --- result tables + verdict --------------------------------------------------
echo "---- nodeB tcp table ----"
sed -n '/== tcp READ latency/,$p' "$LOGDIR/tcp.read.log" 2>/dev/null
if [ -f "$LOGDIR/urma.read.log" ]; then
    echo "---- nodeB urma table ----"
    sed -n '/== urma READ latency/,$p' "$LOGDIR/urma.read.log" 2>/dev/null
fi

if [ "$FAIL" -eq 0 ]; then
    echo "PASS: read_lat matrix (tcp loopback + cross-node tcp$( [ -f "$LOGDIR/urma.read.log" ] && echo ' + urma' ))"
else
    echo "FAIL: logs kept in $LOGDIR (rerun to regenerate)"
    trap - EXIT
fi
exit $FAIL
