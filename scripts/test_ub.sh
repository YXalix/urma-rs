#!/bin/bash
# Real-device (UB) testcases across TWO machines. UB has no single-machine
# loopback, so every testcase runs between the two nodes of an IP list.
# Orchestration is over ssh from this machine (which need not be a node):
# the examples are built locally, deployed to both nodes, and run as pairs.
#
# Node list (first two entries are used; a node is [user@]ip), either:
#   UB_NODES="hostA hostB" ./scripts/test_ub.sh
# or a file (one node per line, '#' comments allowed):
#   scripts/ub_nodes.txt
#
# Env knobs:
#   DEV=<name>      device on both nodes (default: probe via list_devices)
#   DEV_A/DEV_B     per-node device override
#   TMO=<secs>      per-process timeout on the nodes (default 60)
#   SSH_OPTS=...    extra ssh/scp options
# SKIPs (exit 0) when no two-node list is configured or a node has no device.
set -u
cd "$(dirname "$0")/.."

# clear proxy env vars so node-to-node requests never go through a proxy
unset http_proxy https_proxy all_proxy no_proxy \
      HTTP_PROXY HTTPS_PROXY ALL_PROXY NO_PROXY 2>/dev/null || true

SSH_OPTS="${SSH_OPTS:--o BatchMode=yes -o StrictHostKeyChecking=accept-new}"
TMO=${TMO:-60}
R=2   # lookup records per client

# --- node list -------------------------------------------------------------
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
    echo 'SKIP: no two-node list configured (set UB_NODES="ipA ipB" or fill scripts/ub_nodes.txt)'
    exit 0
fi
A=${NODES[0]}
B=${NODES[1]}
IP_A=${A##*@}   # address the peer dials (strip user@ prefix)
IP_B=${B##*@}
echo "UB nodes: $A (nodeA) + $B (nodeB)"

# --- build + deploy --------------------------------------------------------
cargo build --examples || exit 1
BINS="urma_hello urma_pingpong urma_lookup list_devices urma_cli"

deploy() {  # <node> -> remote dir on stdout
    local rdir
    # shellcheck disable=SC2086
    rdir=$(ssh $SSH_OPTS "$1" "mktemp -d /tmp/urma_rs_ub.XXXXXX") || return 1
    local b
    for b in $BINS; do
        # shellcheck disable=SC2086
        scp $SSH_OPTS -q "target/debug/examples/$b" "$1:$rdir/" || return 1
    done
    echo "$rdir"
}

RDIR_A=$(deploy "$A") || { echo "FAIL: cannot deploy to $A (ssh/scp)"; exit 1; }
RDIR_B=$(deploy "$B") || { echo "FAIL: cannot deploy to $B (ssh/scp)"; exit 1; }

LOGDIR=$(mktemp -d /tmp/urma_rs_ub_logs.XXXXXX)
echo "node logs: $LOGDIR"
cleanup() {
    rm -rf "$LOGDIR"
    # shellcheck disable=SC2086
    ssh $SSH_OPTS "$A" "rm -rf '$RDIR_A'" 2>/dev/null
    # shellcheck disable=SC2086
    ssh $SSH_OPTS "$B" "rm -rf '$RDIR_B'" 2>/dev/null
}
trap cleanup EXIT

# --- device probe ----------------------------------------------------------
probe_dev() {  # <node> <rdir> -> first device name on stdout (empty if none)
    # shellcheck disable=SC2086
    ssh $SSH_OPTS "$1" "'$2/list_devices' 2>/dev/null | head -1"
}
DEV_A=${DEV_A:-${DEV:-$(probe_dev "$A" "$RDIR_A")}}
DEV_B=${DEV_B:-${DEV:-$(probe_dev "$B" "$RDIR_B")}}
if [ -z "$DEV_A" ] || [ -z "$DEV_B" ]; then
    echo "SKIP: no URMA device on $A or $B (urma_get_device_list empty)"
    exit 0
fi
echo "devices: $DEV_A (nodeA) / $DEV_B (nodeB)"

# run_node <node> <rdir> <logfile> <cmd...>: run under timeout, log locally
run_node() {
    local node=$1 rdir=$2 log=$3; shift 3
    # shellcheck disable=SC2086
    ssh $SSH_OPTS "$node" "cd '$rdir' && timeout -k 5 $TMO ./$*" >"$LOGDIR/$log" 2>&1
}

FAIL=0
check() {  # <logfile> <expected line>
    if ! grep -qF "$2" "$LOGDIR/$1"; then
        echo "MISSING in $1: $2"
        FAIL=1
    fi
}

# --- 1) urma_hello: one-sided READ both ways -------------------------------
echo "== urma_hello =="
run_node "$A" "$RDIR_A" hello.nodeA.log urma_hello -d "$DEV_A" -i "$IP_B" -n nodeA &
PA=$!
run_node "$B" "$RDIR_B" hello.nodeB.log urma_hello -d "$DEV_B" -i "$IP_A" -n nodeB &
PB=$!
wait "$PA" || FAIL=1
wait "$PB" || FAIL=1
check hello.nodeA.log 'read 64 bytes via URMA READ: "hello world from nodeB"'
check hello.nodeB.log 'read 64 bytes via URMA READ: "hello world from nodeA"'
check hello.nodeA.log "peer finished reading, bye"
check hello.nodeB.log "peer finished reading, bye"

# --- 2) urma_pingpong: two-sided SEND/RECV ---------------------------------
echo "== urma_pingpong =="
run_node "$A" "$RDIR_A" pingpong.nodeA.log urma_pingpong -d "$DEV_A" -i "$IP_B" -n nodeA &
PA=$!
run_node "$B" "$RDIR_B" pingpong.nodeB.log urma_pingpong -d "$DEV_B" -i "$IP_A" -n nodeB &
PB=$!
wait "$PA" || FAIL=1
wait "$PB" || FAIL=1
check pingpong.nodeA.log 'pong via URMA SEND/RECV: "pong from nodeB"'
check pingpong.nodeB.log 'pong via URMA SEND/RECV: "pong from nodeA"'
check pingpong.nodeA.log "pong consumed by peer (send completed), no bye needed"
check pingpong.nodeB.log "pong consumed by peer (send completed), no bye needed"

# --- 3) urma_lookup: master on nodeA, one client per node ------------------
echo "== urma_lookup =="
run_node "$A" "$RDIR_A" lookup.master.log urma_lookup --master --clients 2 &
PM=$!
run_node "$A" "$RDIR_A" lookup.nodeA.log urma_lookup -d "$DEV_A" -m "$IP_A" --name nodeA --records "$R" &
PA=$!
run_node "$B" "$RDIR_B" lookup.nodeB.log urma_lookup -d "$DEV_B" -m "$IP_A" --name nodeB --records "$R" &
PB=$!
wait "$PA" || FAIL=1
wait "$PB" || FAIL=1
wait "$PM" || FAIL=1

# Parse the range actually assigned by the master from each client log
# (accept order is arbitrary, do not assume it)
declare -A MY_FIRST=()
for me in nodeA nodeB; do
    f=$(sed -n 's/.*holding records \[\([0-9]*\),.*/\1/p' "$LOGDIR/lookup.$me.log" | head -1)
    if [ -z "$f" ]; then
        echo "MISSING: cannot parse record range of $me"
        FAIL=1
    else
        MY_FIRST[$me]=$f
    fi
done
for me in nodeA nodeB; do
    peer=nodeB; [ "$me" = nodeB ] && peer=nodeA
    f=${MY_FIRST[$me]:-}
    [ -n "$f" ] || continue
    pf=${MY_FIRST[$peer]:-}
    [ -n "$pf" ] || continue
    for ((rid = pf; rid < pf + R; rid++)); do
        check "lookup.$me.log" "fetched record $rid from $peer: \"record $rid from $peer\""
    done
done

# --- 4) urma_cli: manual descriptor exchange (serve on A, read on B) --------
echo "== urma_cli =="
run_node "$A" "$RDIR_A" cli.serve.log urma_cli serve -d "$DEV_A" &
PSERVE=$!
# serve prints the descriptor as one hex line once its resources are up;
# play the human: grab it from the log and pass it as read's argument
DESC=""
for _ in $(seq 1 30); do
    DESC=$(sed -n 's/^\[desc\] //p' "$LOGDIR/cli.serve.log" 2>/dev/null | head -1)
    [ -n "$DESC" ] && break
    sleep 1
done
if [ -z "$DESC" ]; then
    echo "MISSING: no [desc] line from urma_cli serve on $A"
    FAIL=1
else
    run_node "$B" "$RDIR_B" cli.read.log urma_cli read -d "$DEV_B" "$DESC" || FAIL=1
    check cli.read.log '[read ] completion: status 0 len 64'
    check cli.read.log '[read ] data: "hello from urma-cli@'
fi
kill "$PSERVE" 2>/dev/null
wait "$PSERVE" 2>/dev/null

# --- verdict ---------------------------------------------------------------
if [ "$FAIL" -eq 0 ]; then
    echo "PASS: UB testcases (urma_hello + urma_pingpong + urma_lookup + urma_cli) between $IP_A and $IP_B"
else
    echo "FAIL: log tails:"
    tail -n 10 "$LOGDIR"/*.log
    echo "FAIL: logs kept in $LOGDIR (rerun to regenerate)"
    trap - EXIT
fi
exit $FAIL
